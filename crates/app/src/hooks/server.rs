//! The loopback listener Claude Code's hooks report into (GitHub issue #239, phase 2).
//!
//! One listener per Jerry process, started once at app startup and serving every Claude agent
//! this instance ever spawns - not one per agent. The port is OS-assigned (bind to `:0`, read the
//! real port back) so Jerry never fights another program, another Jerry instance, or a stale
//! socket for a hardcoded number.
//!
//! ## Why a socket rather than a file or a pipe
//!
//! A hook is a *detached subprocess*: Claude Code spawns it without the interactive TUI's stdio,
//! so there is no existing channel back to Jerry to reuse. The forwarder needs a destination it
//! can address knowing nothing but two environment variables, from a shell one-liner, with no
//! Jerry code running inside it. A loopback TCP port is the one such destination that needs no
//! filesystem coordination, no cleanup if Jerry dies, and no per-agent setup.
//!
//! ## Threat model, and what is actually defended
//!
//! This listener accepts unauthenticated TCP connect attempts from anything running as this user
//! - that is unavoidable for any loopback port. So the real defences are, in order:
//!
//! - **Loopback only.** Bound to `127.0.0.1`, never `0.0.0.0`, so nothing off-host can reach it
//!   at all. [`HookListener::start`] also re-checks `peer_addr()` per connection and drops
//!   anything non-loopback - belt and braces against a future refactor changing the bind address.
//! - **A real token.** A 256-bit CSPRNG token generated fresh per app launch, required in an
//!   `Authorization` header, compared in constant time ([`constant_time_eq`]). A request without
//!   it is refused before its body is read, let alone parsed. There is deliberately no "no token
//!   configured" fallback path: the token is generated in [`HookListener::start`] and is not
//!   optional, so there is no configuration under which the check silently becomes a no-op.
//! - **Hard bounds on everything read.** A 2-second read timeout, an 8 KiB cap on the request
//!   line plus headers, a 100-header cap, and [`crate::hooks::event::MAX_PAYLOAD_BYTES`] on the
//!   body - refused by `Content-Length` *before* a single body byte is buffered, and enforced
//!   again while reading in case the header lied. A connection that stalls mid-request dies on
//!   the read timeout rather than pinning a thread.
//! - **A cap on concurrent handlers.** [`MAX_IN_FLIGHT`] connections are handled at once; past
//!   that, new connections are closed immediately rather than spawning unbounded threads.
//!
//! What this deliberately does *not* defend against: another process running as this same user
//! reading Jerry's generated settings file to learn the token. That is not a boundary this can
//! hold - a process running as you can already read `~/.claude`, attach a debugger to Jerry, or
//! read the pty directly. The token defends against *unauthenticated* local processes (a browser
//! page's fetch to `127.0.0.1:<port>`, another user's process, a stray port scanner), which is
//! the real exposure a loopback port creates.
//!
//! The worst a forged, correctly-tokened request can do is set a wrong status glyph on one rail
//! row until the next real event or the TTL expires. No hook payload is executed, none is
//! rendered as markup, and none can reach a `ProcessKind::Shell` row (see
//! [`crate::rail::status::derive_status`]).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hooks::event::{self, HookReport, MAX_PAYLOAD_BYTES};
use crate::work_surface::agents::AgentId;

/// How long a single connection may take to deliver its request before it is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on the request line plus all headers. Real Claude Code hook requests carry a handful of
/// short headers; anything approaching this is either broken or hostile.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Cap on header count, so a request of many tiny headers can't be used to burn CPU under the
/// byte cap.
const MAX_HEADERS: usize = 100;

/// How many connections are handled concurrently. Hook traffic is a trickle (a few events per
/// agent turn), so this is purely a bound on pathological behaviour, not a throughput knob.
const MAX_IN_FLIGHT: usize = 16;

/// The header the forwarder puts the token in.
const AUTH_HEADER: &str = "authorization";

/// One agent's most recent hook fact, as stored for the rail to read.
#[derive(Debug, Clone)]
pub struct HookRecord {
    /// The parsed event - see [`crate::hooks::event::parse`].
    pub report: HookReport,
    /// When it arrived, for the freshness check in [`crate::rail::status::HookSignal`].
    pub received_at: Instant,
}

/// Every agent's latest hook fact, shared between the listener thread and the UI thread.
///
/// Deliberately "latest wins" rather than an accumulated history: a hook stream is a sequence of
/// state *transitions*, so the most recent event is by definition the agent's current state.
/// This is what keeps a routine `PostToolUseFailure` (a failing test, a `grep` that matched
/// nothing - both extremely common and both things Claude Code recovers from without help) from
/// pinning a row to "Failed": the very next `PreToolUse` overwrites it. A failure only *stays*
/// visible if the agent produced no further events after it, which is exactly the case where a
/// human really does want to know something broke and nothing has happened since.
#[derive(Debug, Default)]
pub struct HookInbox {
    latest: HashMap<AgentId, HookRecord>,
}

impl HookInbox {
    /// This agent's most recent hook fact, if it has ever reported one.
    pub fn get(&self, id: AgentId) -> Option<&HookRecord> {
        self.latest.get(&id)
    }

    /// Records a freshly parsed event as `id`'s current state.
    pub fn record(&mut self, id: AgentId, report: HookReport) {
        self.latest.insert(
            id,
            HookRecord {
                report,
                received_at: Instant::now(),
            },
        );
    }

    /// Drops an agent's facts - called when the agent closes, so a recycled
    /// [`AgentId`] can never inherit a dead agent's status.
    pub fn forget(&mut self, id: AgentId) {
        self.latest.remove(&id);
    }
}

/// A running loopback listener. Dropping it stops the accept loop.
pub struct HookListener {
    port: u16,
    token: String,
    inbox: Arc<Mutex<HookInbox>>,
    shutdown: Arc<AtomicBool>,
}

impl HookListener {
    /// Binds `127.0.0.1:0`, reads back the real assigned port, generates this launch's token and
    /// starts the accept loop on a dedicated thread.
    ///
    /// Returns the bind error rather than panicking: a machine with no usable loopback is a real
    /// (if strange) state, and it must degrade to "no hook signal, quiescence heuristic as
    /// before" rather than failing app startup - see `crate::root::state`'s call site.
    pub fn start() -> std::io::Result<HookListener> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = generate_token();
        let inbox: Arc<Mutex<HookInbox>> = Arc::new(Mutex::new(HookInbox::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_token = token.clone();
        let thread_inbox = Arc::clone(&inbox);
        let thread_shutdown = Arc::clone(&shutdown);
        std::thread::Builder::new()
            .name("jerry-hook-listener".to_owned())
            .spawn(move || {
                let in_flight = Arc::new(AtomicUsize::new(0));
                for stream in listener.incoming() {
                    if thread_shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else {
                        // A single failed accept (an interrupted syscall, a client that vanished
                        // between the SYN and the accept) must not kill the listener for the
                        // whole app session.
                        continue;
                    };
                    if !is_loopback(&stream) {
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    if in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let handler_token = thread_token.clone();
                    let handler_inbox = Arc::clone(&thread_inbox);
                    let handler_in_flight = Arc::clone(&in_flight);
                    let spawned = std::thread::Builder::new()
                        .name("jerry-hook-conn".to_owned())
                        .spawn(move || {
                            handle_connection(stream, &handler_token, &handler_inbox);
                            handler_in_flight.fetch_sub(1, Ordering::Relaxed);
                        });
                    if spawned.is_err() {
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            })?;

        Ok(HookListener {
            port,
            token,
            inbox,
            shutdown,
        })
    }

    /// The real OS-assigned port, for the `JERRY_HOOK_PORT` env var.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// This launch's token, for the `JERRY_HOOK_TOKEN` env var.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The shared inbox the rail reads from.
    pub fn inbox(&self) -> &Arc<Mutex<HookInbox>> {
        &self.inbox
    }

    /// This agent's current hook fact and how long ago it arrived, ready for
    /// [`crate::rail::status::HookSignal`]. A poisoned lock reports "no signal" rather than
    /// panicking - a hook fact is an optional refinement, and losing it must never take the rail
    /// down (same rule `crate::persisted_state_lock` applies to its own poisoned mutex).
    pub fn signal_for(&self, id: AgentId) -> crate::rail::status::HookSignal {
        let Ok(inbox) = self.inbox.lock() else {
            return crate::rail::status::HookSignal::default();
        };
        match inbox.get(id) {
            Some(record) => crate::rail::status::HookSignal {
                fact: Some(record.report.fact),
                age: record.received_at.elapsed(),
            },
            None => crate::rail::status::HookSignal::default(),
        }
    }

    /// This agent's current hook-derived activity/question text, if any is fresh enough to show.
    /// Returns `(activity, question)`.
    pub fn text_for(&self, id: AgentId) -> (Option<String>, Option<String>) {
        let Ok(inbox) = self.inbox.lock() else {
            return (None, None);
        };
        match inbox.get(id) {
            Some(record) if record.received_at.elapsed() < event::HOOK_SIGNAL_TTL => (
                record.report.activity.clone(),
                record.report.question.clone(),
            ),
            _ => (None, None),
        }
    }

    /// Drops an agent's recorded facts - see [`HookInbox::forget`].
    pub fn forget(&self, id: AgentId) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.forget(id);
        }
    }
}

impl Drop for HookListener {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Unblock the accept loop so the flag is actually observed: `incoming()` is blocking, so
        // setting the flag alone would leave the thread parked in `accept` until the next real
        // connection. A self-connect is the portable way to wake it.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Whether the peer really is on the loopback interface - see the module docs.
fn is_loopback(stream: &TcpStream) -> bool {
    match stream.peer_addr() {
        Ok(SocketAddr::V4(addr)) => addr.ip().is_loopback(),
        Ok(SocketAddr::V6(addr)) => {
            let ip = *addr.ip();
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| IpAddr::V4(mapped).is_loopback())
        }
        Err(_) => false,
    }
}

/// A 256-bit token, hex encoded.
///
/// Uses `rand`'s CSPRNG (`rand::rngs::OsRng` via `rand::rng`) rather than this codebase's usual
/// `std::process::id()` + `AtomicU64` uniqueness convention: that convention exists to make temp
/// *filenames* unique, where predictability costs nothing. Here predictability is the whole
/// attack - a token derived from a pid and a counter is guessable by any local process that can
/// read `/proc`.
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compares two secrets without an early return, so a caller can't binary-search the token by
/// timing. Length is compared first and non-secretly (the token's length is fixed and public).
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Percent-decodes one query-string value. Hook event names and agent ids are plain ASCII, so
/// this exists only so a percent-encoded value round-trips rather than being silently mangled
/// into a name that matches nothing.
fn percent_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pulls `key`'s value out of a raw query string (`event=Stop&agent=7`).
fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

/// The outcome of reading one request, kept separate from the socket work so it is testable.
#[derive(Debug, PartialEq, Eq)]
enum Response {
    /// Accepted (whether or not the payload turned out to be an event Jerry acts on).
    NoContent,
    BadRequest,
    Unauthorized,
    NotFound,
    PayloadTooLarge,
}

impl Response {
    fn status_line(&self) -> &'static str {
        match self {
            Response::NoContent => "HTTP/1.1 204 No Content",
            Response::BadRequest => "HTTP/1.1 400 Bad Request",
            Response::Unauthorized => "HTTP/1.1 401 Unauthorized",
            Response::NotFound => "HTTP/1.1 404 Not Found",
            Response::PayloadTooLarge => "HTTP/1.1 413 Payload Too Large",
        }
    }
}

/// Reads one request off `stream`, records whatever it turned out to be, and writes a response.
fn handle_connection(mut stream: TcpStream, token: &str, inbox: &Arc<Mutex<HookInbox>>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));

    let response = read_and_record(&mut stream, token, inbox).unwrap_or(Response::BadRequest);

    // Always `Connection: close` and always a `Content-Length: 0` body, so a client that speaks
    // HTTP/1.1 keep-alive doesn't sit waiting for a body that isn't coming.
    let reply = format!(
        "{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        response.status_line()
    );
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// The real request handling: parse, authenticate, bound, record. Returns `None` on any I/O
/// failure, which the caller turns into a 400.
fn read_and_record(
    stream: &mut TcpStream,
    token: &str,
    inbox: &Arc<Mutex<HookInbox>>,
) -> Option<Response> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    // -- request line, bounded --
    let mut request_line = String::new();
    let mut head_budget = MAX_HEAD_BYTES;
    let read = read_line_bounded(&mut reader, &mut request_line, head_budget)?;
    head_budget = head_budget.saturating_sub(read);

    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "POST" {
        return Some(Response::NotFound);
    }

    // -- headers, bounded in both bytes and count --
    let mut content_length: Option<usize> = None;
    let mut authorization: Option<String> = None;
    for _ in 0..MAX_HEADERS {
        let mut line = String::new();
        let read = read_line_bounded(&mut reader, &mut line, head_budget)?;
        head_budget = head_budget.saturating_sub(read);
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Some(Response::BadRequest);
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => content_length = Some(value.parse().ok()?),
            AUTH_HEADER => authorization = Some(value.to_owned()),
            _ => {}
        }
    }

    // -- authenticate before reading a single body byte --
    let supplied = authorization?;
    let supplied = supplied
        .strip_prefix("Bearer ")
        .or_else(|| supplied.strip_prefix("bearer "))
        .unwrap_or(&supplied);
    if !constant_time_eq(supplied, token) {
        return Some(Response::Unauthorized);
    }

    // -- route --
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    if path != "/hook" {
        return Some(Response::NotFound);
    }

    // -- bound the body by the declared length before buffering any of it --
    let length = content_length?;
    if length > MAX_PAYLOAD_BYTES {
        return Some(Response::PayloadTooLarge);
    }
    let mut body = vec![0u8; length];
    // `read_exact` over a `take` so a `Content-Length` that overstates the real body dies on the
    // read timeout instead of blocking forever, and one that understates it can't bleed the next
    // request's bytes into this payload.
    reader
        .by_ref()
        .take(length as u64)
        .read_exact(&mut body)
        .ok()?;

    let event_name = query_value(query, "event").unwrap_or_default();
    let agent_id: Option<AgentId> = query_value(query, "agent").and_then(|id| id.parse().ok());

    // An unparseable or absent agent id is a real, expected case (the forwarder ran outside a
    // Jerry-spawned pane, or a hand-made request): accept the request so the client isn't left
    // retrying, but record nothing - there is no row it could belong to.
    if let (Some(agent_id), Some(report)) = (agent_id, event::parse(&event_name, &body)) {
        if let Ok(mut inbox) = inbox.lock() {
            inbox.record(agent_id, report);
        }
    }
    Some(Response::NoContent)
}

/// [`BufRead::read_line`] with a hard byte budget, so a client that never sends a newline can't
/// grow Jerry's memory without bound. Returns the bytes consumed.
fn read_line_bounded(
    reader: &mut BufReader<TcpStream>,
    out: &mut String,
    budget: usize,
) -> Option<usize> {
    let mut limited = reader.take(budget as u64);
    let mut raw = Vec::new();
    let read = limited.read_until(b'\n', &mut raw).ok()?;
    if read == 0 {
        return None;
    }
    // A line that exactly consumed the budget without a terminator is over-long: refuse rather
    // than silently treating the truncation as a complete line.
    if read == budget && !raw.ends_with(b"\n") {
        return None;
    }
    out.push_str(&String::from_utf8_lossy(&raw));
    Some(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::event::HookFact;

    /// Sends one raw request to a live listener and returns the raw response.
    fn raw_request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.write_all(request.as_bytes()).expect("write");
        stream.flush().ok();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn post(port: u16, token: &str, query: &str, body: &str) -> String {
        raw_request(
            port,
            &format!(
                "POST /hook?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    #[test]
    fn a_real_tokened_post_round_trips_into_the_inbox() {
        let listener = HookListener::start().expect("listener must start");
        assert!(listener.port() > 0, "the OS must have assigned a real port");

        let body = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#;
        let response = post(
            listener.port(),
            listener.token(),
            "event=PreToolUse&agent=7",
            body,
        );
        assert!(
            response.starts_with("HTTP/1.1 204"),
            "expected 204, got {response:?}"
        );

        let signal = listener.signal_for(7);
        assert_eq!(signal.fact, Some(HookFact::Working));
        let (activity, question) = listener.text_for(7);
        assert_eq!(activity.as_deref(), Some("Bash: cargo test"));
        assert_eq!(question, None);
        // A different agent id must be untouched by another agent's event.
        assert_eq!(listener.signal_for(8).fact, None);
    }

    #[test]
    fn every_launch_gets_a_distinct_high_entropy_token() {
        let a = HookListener::start().expect("start a");
        let b = HookListener::start().expect("start b");
        assert_ne!(
            a.token(),
            b.token(),
            "tokens must not repeat across launches"
        );
        assert_eq!(a.token().len(), 64, "256 bits, hex encoded");
        assert!(a.token().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.port(), b.port(), "each listener gets its own real port");
    }

    #[test]
    fn a_request_without_the_right_token_is_refused_and_records_nothing() {
        let listener = HookListener::start().expect("listener must start");
        let body = r#"{"hook_event_name":"Stop"}"#;

        // Wrong token.
        let wrong = post(
            listener.port(),
            "0000000000000000000000000000000000000000000000000000000000000000",
            "event=Stop&agent=3",
            body,
        );
        assert!(wrong.starts_with("HTTP/1.1 401"), "got {wrong:?}");

        // No Authorization header at all.
        let missing = raw_request(
            listener.port(),
            &format!(
                "POST /hook?event=Stop&agent=3 HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(missing.starts_with("HTTP/1.1 400"), "got {missing:?}");

        // A token that is a prefix of the real one must not be accepted either.
        let prefix = post(
            listener.port(),
            &listener.token()[..32],
            "event=Stop&agent=3",
            body,
        );
        assert!(prefix.starts_with("HTTP/1.1 401"), "got {prefix:?}");

        assert_eq!(
            listener.signal_for(3).fact,
            None,
            "no unauthenticated request may reach the inbox"
        );
    }

    #[test]
    fn an_oversized_body_is_refused_without_being_buffered() {
        let listener = HookListener::start().expect("listener must start");
        let response = raw_request(
            listener.port(),
            &format!(
                "POST /hook?event=Stop&agent=1 HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
                listener.token(),
                MAX_PAYLOAD_BYTES + 1
            ),
        );
        assert!(response.starts_with("HTTP/1.1 413"), "got {response:?}");
        assert_eq!(listener.signal_for(1).fact, None);
    }

    #[test]
    fn malformed_truncated_and_hostile_requests_neither_crash_nor_hang_the_listener() {
        let listener = HookListener::start().expect("listener must start");
        let token = listener.token().to_owned();
        let port = listener.port();

        // Garbage, an empty request, a wrong method, a wrong path, an unterminated header,
        // a non-numeric Content-Length, a body shorter than its declared length.
        let hostile = vec![
            "not http at all\r\n\r\n".to_owned(),
            "\r\n".to_owned(),
            format!("GET /hook?event=Stop&agent=1 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
            format!("POST /evil?event=Stop&agent=1 HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n{{}}"),
            format!("POST /hook?event=Stop&agent=1 HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: notanumber\r\n\r\n"),
            format!("POST /hook?event=Stop&agent=1 HTTP/1.1\r\nAuthorization Bearer {token}\r\n\r\n"),
            // A header line far past the head budget, never terminated.
            format!("POST /hook?event=Stop&agent=1 HTTP/1.1\r\nX-Huge: {}", "A".repeat(MAX_HEAD_BYTES * 2)),
        ];
        for request in hostile {
            // The assertion that matters is that this returns at all - a hang here is the bug.
            let _ = raw_request(port, &request);
        }

        assert_eq!(
            listener.signal_for(1).fact,
            None,
            "none of the hostile requests may have recorded a fact"
        );

        // The listener must still be alive and correct after all of that.
        let good = post(
            port,
            &token,
            "event=Stop&agent=1",
            r#"{"hook_event_name":"Stop"}"#,
        );
        assert!(good.starts_with("HTTP/1.1 204"), "got {good:?}");
        assert_eq!(listener.signal_for(1).fact, Some(HookFact::TurnEnded));
    }

    #[test]
    fn a_valid_request_for_an_event_jerry_ignores_is_accepted_but_records_nothing() {
        let listener = HookListener::start().expect("listener must start");
        let response = post(
            listener.port(),
            listener.token(),
            "event=PreCompact&agent=5",
            r#"{"hook_event_name":"PreCompact"}"#,
        );
        assert!(response.starts_with("HTTP/1.1 204"), "got {response:?}");
        assert_eq!(listener.signal_for(5).fact, None);
    }

    #[test]
    fn a_request_with_no_usable_agent_id_is_accepted_but_records_nothing() {
        // Exactly what a forwarder run outside a Jerry-spawned pane would produce.
        let listener = HookListener::start().expect("listener must start");
        for query in [
            "event=Stop",
            "event=Stop&agent=",
            "event=Stop&agent=notanumber",
        ] {
            let response = post(
                listener.port(),
                listener.token(),
                query,
                r#"{"hook_event_name":"Stop"}"#,
            );
            assert!(
                response.starts_with("HTTP/1.1 204"),
                "{query}: {response:?}"
            );
        }
    }

    #[test]
    fn the_latest_event_replaces_the_previous_one_for_the_same_agent() {
        // "Latest wins" is what keeps a routine tool failure from pinning a row to Failed - see
        // `HookInbox`'s own docs.
        let listener = HookListener::start().expect("listener must start");
        post(
            listener.port(),
            listener.token(),
            "event=PostToolUseFailure&agent=2",
            r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","error":"exit 1"}"#,
        );
        assert_eq!(listener.signal_for(2).fact, Some(HookFact::TurnFailed));

        post(
            listener.port(),
            listener.token(),
            "event=PreToolUse&agent=2",
            r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/tmp/a.rs"}}"#,
        );
        assert_eq!(
            listener.signal_for(2).fact,
            Some(HookFact::Working),
            "the next real event must supersede the failure"
        );
    }

    #[test]
    fn forgetting_an_agent_clears_its_facts_so_a_reused_id_cannot_inherit_them() {
        let listener = HookListener::start().expect("listener must start");
        post(
            listener.port(),
            listener.token(),
            "event=Stop&agent=9",
            r#"{"hook_event_name":"Stop"}"#,
        );
        assert_eq!(listener.signal_for(9).fact, Some(HookFact::TurnEnded));
        listener.forget(9);
        assert_eq!(listener.signal_for(9).fact, None);
        assert_eq!(listener.text_for(9), (None, None));
    }

    #[test]
    fn constant_time_eq_is_a_real_comparison() {
        // A "constant time" helper that always returned true would pass a naive auth test, so
        // this pins the actual semantics.
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn query_values_are_parsed_and_percent_decoded() {
        assert_eq!(
            query_value("event=PreToolUse&agent=12", "event").as_deref(),
            Some("PreToolUse")
        );
        assert_eq!(
            query_value("event=PreToolUse&agent=12", "agent").as_deref(),
            Some("12")
        );
        assert_eq!(query_value("event=a%20b", "event").as_deref(), Some("a b"));
        assert_eq!(query_value("event=x", "missing"), None);
        assert_eq!(query_value("", "event"), None);
    }
}
