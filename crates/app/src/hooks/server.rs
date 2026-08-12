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
//! - **Hard bounds on everything read.** An 8 KiB cap on the request line plus headers, a
//!   100-header cap, and [`crate::hooks::event::MAX_PAYLOAD_BYTES`] on the body - refused by
//!   `Content-Length` *before* a single body byte is buffered, and enforced again while reading
//!   in case the header lied.
//! - **A bound on request *time*, not just size.** [`REQUEST_DEADLINE`] caps the whole exchange
//!   from a single absolute instant, and each blocking read is clamped to the time left. A
//!   per-read timeout alone is not enough and was a real hole: `SO_RCVTIMEO` resets on every byte
//!   that arrives, so a client dripping one byte per second held a handler for hours - pre-auth,
//!   since the token is not checked until the headers are read.
//! - **A cap on concurrent handlers.** [`MAX_IN_FLIGHT`] connections are handled at once; past
//!   that, new connections are closed immediately rather than spawning unbounded threads. Slots
//!   are released by an RAII guard ([`InFlightSlot`]), so a panic cannot leak one.
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
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hooks::event::{self, HookFact, HookReport, MAX_PAYLOAD_BYTES};
use crate::work_surface::agents::AgentId;

/// How long a single blocking read may wait for *some* data to arrive.
///
/// This alone is not a bound on the request: see [`REQUEST_DEADLINE`].
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The wall-clock budget for one entire request, start to finish.
///
/// [`READ_TIMEOUT`] is a socket option (`SO_RCVTIMEO`) and therefore bounds each individual
/// `recv`, not the request - every byte that arrives resets it. A client dripping one byte just
/// inside that window, never sending a newline, keeps a handler thread alive for
/// `MAX_HEAD_BYTES * READ_TIMEOUT` - hours - and doing that on [`MAX_IN_FLIGHT`] connections
/// starves every real hook for as long as it cares to. Worse, it costs nothing to mount: the
/// token is not checked until the headers have been read, so this is reachable *pre-auth*.
///
/// So every read is additionally bounded by an absolute deadline taken once per connection, and
/// each blocking read's timeout is clamped to the time actually left. Five seconds is generous
/// for a loopback POST of a few hundred bytes from a `curl` on the same machine, and is the
/// timeout the forwarder itself gives up at anyway (`--max-time 5`), so a request still running
/// past this has already lost its client.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// Cap on the request line plus all headers. Real Claude Code hook requests carry a handful of
/// short headers; anything approaching this is either broken or hostile.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Cap on header count, so a request of many tiny headers can't be used to burn CPU under the
/// byte cap.
const MAX_HEADERS: usize = 100;

/// How many connections are handled concurrently. Hook traffic is a trickle (a few events per
/// agent turn), so this is purely a bound on pathological behaviour, not a throughput knob.
///
/// ## Known, accepted limitation: aggregate starvation by reconnect
///
/// [`REQUEST_DEADLINE`] bounds any *single* connection, which is what closed the original
/// slow-drip hole. It does not bound the *aggregate*: a process that holds all `MAX_IN_FLIGHT`
/// sockets and immediately reconnects as each one expires keeps every slot occupied
/// indefinitely, and real hook connections are then closed on arrival. Because the forwarder
/// always exits 0 (deliberately - see [`crate::hooks::settings_file`]), that failure is silent,
/// and affected agents fall back to the Phase 1 title/quiescence signals.
///
/// Accepted rather than fixed, for now, because it sits inside the threat model this feature
/// already documents: it requires a sustained same-user, same-machine reconnect loop, and such a
/// process can already read the token straight out of `/proc/<pid>/environ`, which buys it
/// strictly more than degrading a status glyph. It is also strictly weaker than the bug it
/// replaced - a one-shot drip is no longer enough.
///
/// A future pass should reserve some slots for connections that have already authenticated, so
/// unauthenticated churn cannot crowd out real hooks.
const MAX_IN_FLIGHT: usize = 16;

/// The header the forwarder puts the token in.
const AUTH_HEADER: &str = "authorization";

/// Most agents [`HookInbox`] will track at once - see [`HookInbox::record`].
///
/// Far above any real usage (each entry is one live agent pane) and far below anything that
/// matters for memory, which is the right place for a bound whose only job is to stop unbounded
/// growth.
const MAX_TRACKED_AGENTS: usize = 512;

/// How long [`HookListener::drop`]'s self-connect may block the dropping thread - see that impl.
const SHUTDOWN_WAKE_TIMEOUT: Duration = Duration::from_millis(250);

/// Smallest write budget allowed for the reply, used when the read phase already consumed the
/// whole [`REQUEST_DEADLINE`] - see [`handle_connection`].
const REPLY_WRITE_FLOOR: Duration = Duration::from_millis(500);

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
///
/// The one real exception - found by watching a live session rather than by reading the docs - is
/// that not every hook event *is* a transition. A `Notification` is Claude Code re-announcing a
/// state it has already reported precisely, with a fixed generic message, so under a literal
/// "latest wins" it destroyed the better fact that had arrived moments earlier. See
/// [`merge_nudge`] and [`event::EventKind`] for the two bugs that caused and for the rule that
/// replaced it.
#[derive(Debug, Default)]
pub struct HookInbox {
    latest: HashMap<AgentId, HookRecord>,
}

impl HookInbox {
    /// This agent's most recent hook fact, if it has ever reported one.
    pub fn get(&self, id: AgentId) -> Option<&HookRecord> {
        self.latest.get(&id)
    }

    /// Records a freshly parsed event as `id`'s current state, evicting the oldest entry if that
    /// would push the inbox past [`MAX_TRACKED_AGENTS`].
    ///
    /// "Latest wins" holds for real lifecycle *transitions* only. A `Notification`
    /// ([`event::EventKind::BlockedNudge`]/[`event::EventKind::IdleNudge`]) is Claude Code
    /// re-announcing a state it has already reported through a precise event, and is folded into
    /// what Jerry already knows rather than replacing it - see [`merge_nudge`] and
    /// [`event::EventKind`] for the two real, live-observed bugs that came from treating them as
    /// equals.
    ///
    /// The cap exists because the id in a request is not checked against the set of live agents -
    /// the listener has no view of that, and adding one would couple it to `AdeApp` state behind
    /// a lock it would then hold on every request. So a client that knows the token (see the
    /// module docs on what that does and does not defend) can name arbitrary ids, and without a
    /// cap each one would allocate a permanent entry. Eviction is by arrival time, which keeps
    /// the real agents - the ones actually emitting events - and sheds invented ids that never
    /// report again.
    pub fn record(&mut self, id: AgentId, report: HookReport) {
        // Only a *fresh* previous record is worth folding into: past the TTL the rail has already
        // stopped believing it (see `crate::rail::status::HookSignal::fresh`), so a nudge is then
        // the only real evidence there is and must stand on its own.
        let report = match self.latest.get(&id) {
            Some(previous) if previous.received_at.elapsed() < event::HOOK_SIGNAL_TTL => {
                merge_nudge(&previous.report, report)
            }
            _ => report,
        };
        if self.latest.len() >= MAX_TRACKED_AGENTS && !self.latest.contains_key(&id) {
            if let Some(oldest) = self
                .latest
                .iter()
                .min_by_key(|(_, record)| record.received_at)
                .map(|(id, _)| *id)
            {
                self.latest.remove(&oldest);
            }
        }
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

/// Folds a `Notification` into the fact Jerry already holds for the same agent, returning the
/// report that should actually be stored.
///
/// A real lifecycle transition is returned untouched - it is the whole truth about where the agent
/// now is, and it supersedes everything before it. A `Notification` is not: Claude Code emits one
/// *because of* an event it has already reported, with a fixed generic message, so taken as an
/// equal it destroys strictly better information. Both of these were observed live against a real
/// `claude` 2.1.228 driven through Jerry's own spawn path:
///
/// - `PermissionRequest` ("Write needs permission: notes.txt") followed milliseconds later by a
///   `permission_prompt` `Notification` ("Claude needs your permission"). Every permission
///   question the rail could ever show was that one constant, and the real per-tool question this
///   codebase parses was unreachable in practice.
/// - `Stop` ([`HookFact::TurnEnded`] - the review boundary) followed about a minute later by an
///   `idle_prompt` `Notification`, which flipped the finished agent to `Ask` and erased the
///   review-ready state phase 2 exists to produce.
///
/// So: a nudge keeps the previous fact whenever the previous fact already *implies* it, and only
/// takes over when it genuinely carries news.
fn merge_nudge(previous: &HookReport, incoming: HookReport) -> HookReport {
    let keep_previous = match incoming.kind {
        // Not a nudge at all.
        event::EventKind::Transition => false,
        // A block Jerry may not have heard about yet: it must still be able to move a working or
        // finished agent to "needs you". The one thing it may not do is overwrite the specific
        // question a `PermissionRequest` gave for the very block it is announcing.
        event::EventKind::BlockedNudge => previous.fact == HookFact::NeedsInput,
        // "You have not typed for a while" - true of every agent that is blocked or done, and
        // news about neither.
        event::EventKind::IdleNudge => matches!(
            previous.fact,
            HookFact::NeedsInput | HookFact::TurnEnded | HookFact::TurnFailed
        ),
    };
    if !keep_previous {
        return incoming;
    }
    HookReport {
        kind: incoming.kind,
        fact: previous.fact,
        activity: previous.activity.clone(),
        // A question belongs to a row that is actually blocked. When the kept fact is a turn
        // boundary, the generic "waiting for your input" would be rendered next to a `Review`
        // row and describe nothing it doesn't already say.
        question: match previous.fact {
            HookFact::NeedsInput => previous.question.clone().or(incoming.question),
            _ => previous.question.clone(),
        },
        // The nudge is the more recent payload, so prefer its session id, but never lose one by
        // taking a payload that happened to omit it.
        session_id: incoming.session_id.or_else(|| previous.session_id.clone()),
    }
}

/// Holds one of the [`MAX_IN_FLIGHT`] connection slots, releasing it on drop.
///
/// An RAII guard rather than a `fetch_sub` at the end of the handler: the handler parses
/// untrusted input, and a panic anywhere in it would skip a trailing decrement and leak the slot
/// permanently. Sixteen such panics and the listener stops accepting anything, for the rest of
/// the session, with no error path that would ever say so. No such panic exists today - every
/// parse step returns `Option` - but "no panic today" is a property of the current code, whereas
/// this is a property of the type.
struct InFlightSlot(Arc<AtomicUsize>);

impl InFlightSlot {
    fn take(counter: Arc<AtomicUsize>) -> InFlightSlot {
        counter.fetch_add(1, Ordering::Relaxed);
        InFlightSlot(counter)
    }
}

impl Drop for InFlightSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
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
                    // The slot is released by `InFlightSlot`'s `Drop`, so a panic anywhere in
                    // the handler returns it instead of leaking it - see that type's docs.
                    let slot = InFlightSlot::take(Arc::clone(&in_flight));
                    let handler_token = thread_token.clone();
                    let handler_inbox = Arc::clone(&thread_inbox);
                    let spawned = std::thread::Builder::new()
                        .name("jerry-hook-conn".to_owned())
                        .spawn(move || {
                            let _slot = slot;
                            handle_connection(stream, &handler_token, &handler_inbox);
                        });
                    if let Err(err) = spawned {
                        // The slot was moved into the closure only on success; on failure it was
                        // dropped with the un-spawned closure, which already released it.
                        log::warn!("could not spawn a hook connection handler: {err}");
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

    /// This agent's most recently reported real Claude Code `session_id` (GitHub issue #227),
    /// if it has ever reported one - the id `claude --resume`/`-r` takes, verified against a real
    /// binary (see [`crate::hooks::event::HookReport::session_id`]).
    ///
    /// Deliberately **not** gated by [`event::HOOK_SIGNAL_TTL`] the way [`Self::text_for`] is: an
    /// activity/question line describes the *present* and must not outlive its truth, but a
    /// session id identifies a *conversation*, which stays exactly as resumable an hour after the
    /// agent went quiet as it was the instant its last hook fired - the whole reason GitHub
    /// issue #227 wants to persist it at all.
    pub fn session_id_for(&self, id: AgentId) -> Option<String> {
        let inbox = self.inbox.lock().ok()?;
        inbox.get(id)?.report.session_id.clone()
    }
}

impl Drop for HookListener {
    /// Signals the accept loop to stop and wakes it so it notices.
    ///
    /// `incoming()` is blocking, so setting the flag alone would leave the thread parked in
    /// `accept` until the next real connection; a self-connect is the portable way to wake it.
    /// It uses `connect_timeout` rather than a plain `connect` because this runs on whichever
    /// thread drops `AdeApp` - in practice the UI thread - and a bare loopback connect, though
    /// almost always instant, has no upper bound the OS is obliged to honour.
    ///
    /// The accept thread is deliberately *not* joined. It exits on its own as soon as it observes
    /// the flag, holding nothing but its own listener, and joining would trade a bounded wake-up
    /// for an unbounded wait on the UI thread - the very thing the timeout above is avoiding.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let _ = TcpStream::connect_timeout(&address, SHUTDOWN_WAKE_TIMEOUT);
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
    // One absolute budget for the whole exchange, taken before the first byte is read - see
    // `REQUEST_DEADLINE`.
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let response =
        read_and_record(&mut stream, token, inbox, deadline).unwrap_or(Response::BadRequest);

    // The reply is written *after* the read phase, which may already have consumed the whole
    // deadline - so a fixed write timeout here simply adds to the worst-case hold rather than
    // bounding it (5s of reading plus 2s of writing is a 7s hold, not the 5s the deadline
    // advertises). Bound the write by whatever is actually left instead.
    //
    // `REPLY_WRITE_FLOOR` rather than zero for the already-expired case, for two reasons: a
    // client that timed out still deserves its 400 rather than a bare RST, and
    // `set_write_timeout(Some(Duration::ZERO))` is an error on Unix, not "don't wait".
    let write_budget = time_left(deadline).unwrap_or(REPLY_WRITE_FLOOR);
    let _ = stream.set_write_timeout(Some(write_budget.max(REPLY_WRITE_FLOOR)));

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
    deadline: Instant,
) -> Option<Response> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    // -- request line, bounded in bytes and in time --
    let mut request_line = String::new();
    let mut head_budget = MAX_HEAD_BYTES;
    let read = read_line_bounded(&mut reader, &mut request_line, head_budget, deadline)?;
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
        let read = read_line_bounded(&mut reader, &mut line, head_budget, deadline)?;
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
    // Exactly `length` bytes and no more, so a `Content-Length` that understates the real body
    // can't bleed the next request's bytes into this payload - and under the same absolute
    // deadline, so one that *overstates* it (or a client that simply stops sending) fails fast
    // instead of holding the handler for as long as the client keeps dripping.
    read_exact_bounded(&mut reader, &mut body, deadline)?;

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

/// Time left before `deadline`, or `None` once it has passed.
fn time_left(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    (now < deadline).then(|| deadline - now)
}

/// Arms the socket so the *next* blocking read cannot outlast `deadline`, returning `None` once
/// there is no time left.
///
/// Clamping to the remaining budget is what makes [`REQUEST_DEADLINE`] real rather than advisory:
/// without it a drip-feeding client resets [`READ_TIMEOUT`] forever and no single read ever fails.
fn arm_read(reader: &BufReader<TcpStream>, deadline: Instant) -> Option<()> {
    let left = time_left(deadline)?;
    reader
        .get_ref()
        .set_read_timeout(Some(left.min(READ_TIMEOUT)))
        .ok()
}

/// Reads one `\n`-terminated line under both a byte budget and an absolute deadline. Returns the
/// bytes consumed.
///
/// Reads a byte at a time deliberately. [`BufRead::read_until`] loops internally until it finds
/// the delimiter, so the deadline could only be checked *after* it returned - which is exactly
/// the hole a drip-feeding client walks through. Going byte by byte puts a deadline check between
/// every byte; it is not a syscall per byte, because [`BufReader`] serves them from its buffer.
fn read_line_bounded(
    reader: &mut BufReader<TcpStream>,
    out: &mut String,
    budget: usize,
    deadline: Instant,
) -> Option<usize> {
    let mut raw = Vec::new();
    while raw.len() < budget {
        arm_read(reader, deadline)?;
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            // A clean EOF mid-line is a truncated request, not a complete one.
            Ok(0) => return None,
            Ok(_) => raw.push(byte[0]),
            // A timeout lands here too, which is the deadline doing its job.
            Err(_) => return None,
        }
        if byte[0] == b'\n' {
            out.push_str(&String::from_utf8_lossy(&raw));
            return Some(raw.len());
        }
    }
    // Budget exhausted with no terminator: over-long, so refuse rather than silently treating the
    // truncation as a complete line.
    None
}

/// Fills `buf` under an absolute deadline - [`Read::read_exact`]'s bounded counterpart, for the
/// same reason [`read_line_bounded`] exists.
fn read_exact_bounded(
    reader: &mut BufReader<TcpStream>,
    buf: &mut [u8],
    deadline: Instant,
) -> Option<()> {
    let mut filled = 0;
    while filled < buf.len() {
        arm_read(reader, deadline)?;
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return None,
            Ok(read) => filled += read,
            Err(_) => return None,
        }
    }
    Some(())
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
    fn a_real_session_id_round_trips_into_the_inbox_for_history_and_resume() {
        // GitHub issue #227's whole resume flow starts here: the real `session_id` a hook payload
        // carries must reach `session_id_for` unchanged.
        let listener = HookListener::start().expect("listener must start");
        let body =
            r#"{"session_id":"5af4c210-34fa-4ab2-9c35-f6ceab76551c","hook_event_name":"Stop"}"#;
        let response = post(
            listener.port(),
            listener.token(),
            "event=Stop&agent=11",
            body,
        );
        assert!(response.starts_with("HTTP/1.1 204"), "got {response:?}");
        assert_eq!(
            listener.session_id_for(11).as_deref(),
            Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c")
        );
        // No event ever recorded for this id: no session id to report.
        assert_eq!(listener.session_id_for(12), None);
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
    fn a_slow_drip_client_is_cut_off_by_the_deadline_instead_of_holding_a_handler() {
        // The hole `REQUEST_DEADLINE` exists to close, and one the existing hostile-request test
        // structurally cannot catch because it sends everything in a single `write_all`:
        // `SO_RCVTIMEO` bounds each `recv`, not the request, so a client dripping a byte just
        // inside the window and never sending a newline resets the clock forever. Pre-auth, so
        // it costs an attacker nothing.
        let listener = HookListener::start().expect("listener");
        let port = listener.port();

        // Absolute bounds, deliberately not expressed in terms of `REQUEST_DEADLINE`: an
        // assertion scaled by the very constant under test passes trivially when that constant is
        // wrong, which is exactly how a test like this fails to catch the bug it was written for.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let started = Instant::now();
        // Never a newline, so the request line never completes. Drips for up to 30s, which is far
        // longer than the handler is allowed to wait.
        let dripped = std::thread::spawn(move || {
            for _ in 0..60 {
                if stream.write_all(b"A").is_err() || stream.flush().is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            response
        });

        let response = dripped.join().expect("the drip thread must finish");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(15),
            "the handler must give up on a drip-feeding client, but the connection lived {elapsed:?}"
        );
        assert!(
            response.is_empty() || response.starts_with("HTTP/1.1 400"),
            "a request that never completed must not be accepted, got {response:?}"
        );

        // The listener must still be fully usable afterwards - the slot has to have been
        // released, not leaked.
        let good = post(
            port,
            listener.token(),
            "event=Stop&agent=1",
            r#"{"hook_event_name":"Stop"}"#,
        );
        assert!(good.starts_with("HTTP/1.1 204"), "got {good:?}");
    }

    #[test]
    fn many_slow_clients_at_once_cannot_starve_a_real_hook() {
        // The reason the deadline matters in practice: `MAX_IN_FLIGHT` drip-feeders holding every
        // handler thread would silently stop every real Claude hook for as long as they liked.
        let listener = HookListener::start().expect("listener");
        let port = listener.port();

        // Every slot occupied by a client that drips for ~25s and never completes a request.
        let mut drips = Vec::new();
        for _ in 0..MAX_IN_FLIGHT {
            drips.push(std::thread::spawn(move || {
                let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
                    return;
                };
                for _ in 0..50 {
                    if stream.write_all(b"A").is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    std::thread::sleep(Duration::from_millis(500));
                }
            }));
        }

        // A fixed wait, longer than the handler deadline but well inside the drippers' lifetime,
        // so the only way real traffic gets served here is if the handlers really did give up.
        // Absolute rather than derived from `REQUEST_DEADLINE` - see the sibling test.
        std::thread::sleep(Duration::from_secs(9));
        let good = post(
            port,
            listener.token(),
            "event=Stop&agent=99",
            r#"{"hook_event_name":"Stop"}"#,
        );
        assert!(
            good.starts_with("HTTP/1.1 204"),
            "a real hook must still be served while slow clients are connected, got {good:?}"
        );
        assert_eq!(listener.signal_for(99).fact, Some(HookFact::TurnEnded));

        for drip in drips {
            let _ = drip.join();
        }
    }

    #[test]
    fn the_inbox_is_capped_so_invented_agent_ids_cannot_grow_it_without_bound() {
        let mut inbox = HookInbox::default();
        let report = crate::hooks::event::parse("Stop", br#"{"hook_event_name":"Stop"}"#)
            .expect("must parse");
        for id in 0..(MAX_TRACKED_AGENTS as u64 + 50) {
            inbox.record(id, report.clone());
        }
        assert_eq!(
            inbox.latest.len(),
            MAX_TRACKED_AGENTS,
            "the inbox must not grow past its cap"
        );
        // Eviction is oldest-first, so the most recent ids - the ones still reporting - survive.
        assert!(inbox.get(MAX_TRACKED_AGENTS as u64 + 49).is_some());
        assert!(inbox.get(0).is_none());
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

    /// The real event sequence a permission prompt produces, captured verbatim from a `claude`
    /// 2.1.228 session driven through Jerry's own spawn path: `PreToolUse`, then
    /// `PermissionRequest` carrying the real tool and argument, then - milliseconds later - a
    /// `Notification` whose entire message is the constant `"Claude needs your permission"`.
    ///
    /// Under plain "latest wins" the rail could therefore *never* show the specific question this
    /// codebase goes to the trouble of parsing: it was overwritten by the constant every single
    /// time, for every tool, which made `crate::hooks::event`'s whole `PermissionRequest` arm
    /// unreachable in practice. Observed live on the real rail before it was fixed.
    #[test]
    fn a_generic_permission_notification_cannot_overwrite_the_real_permission_question() {
        let listener = HookListener::start().expect("listener must start");
        let port = listener.port();
        let token = listener.token().to_owned();

        post(
            port,
            &token,
            "event=PreToolUse&agent=4",
            r#"{"session_id":"s-1","hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"probe-notes.txt"}}"#,
        );
        post(
            port,
            &token,
            "event=PermissionRequest&agent=4",
            r#"{"session_id":"s-1","hook_event_name":"PermissionRequest","tool_name":"Write","tool_input":{"file_path":"probe-notes.txt"}}"#,
        );
        assert_eq!(
            listener.text_for(4).1.as_deref(),
            Some("Write needs permission: probe-notes.txt")
        );

        post(
            port,
            &token,
            "event=Notification&agent=4",
            r#"{"session_id":"s-1","hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission"}"#,
        );

        assert_eq!(
            listener.signal_for(4).fact,
            Some(HookFact::NeedsInput),
            "the agent is still blocked on the human"
        );
        assert_eq!(
            listener.text_for(4).1.as_deref(),
            Some("Write needs permission: probe-notes.txt"),
            "the generic notification must not replace the real question for the same block"
        );
    }

    /// The other half of the same real sequence: a completed turn fires `Stop`, and about a minute
    /// later Claude Code fires an `idle_prompt` `Notification` because nobody has typed since.
    ///
    /// Treating that as a state transition flipped every finished agent from `Review`/`Idle` back
    /// to `Ask` roughly one minute after it finished - erasing the "a turn that ended is a review
    /// boundary even though the process is still alive" capability that is the entire point of
    /// this phase, and replacing its question with the constant "Claude is waiting for your
    /// input". Also observed live on the real rail.
    #[test]
    fn an_idle_notification_cannot_erase_the_turn_boundary_a_real_stop_established() {
        let listener = HookListener::start().expect("listener must start");
        let port = listener.port();
        let token = listener.token().to_owned();

        post(
            port,
            &token,
            "event=Stop&agent=6",
            r#"{"session_id":"s-2","hook_event_name":"Stop"}"#,
        );
        assert_eq!(listener.signal_for(6).fact, Some(HookFact::TurnEnded));

        post(
            port,
            &token,
            "event=Notification&agent=6",
            r#"{"session_id":"s-2","hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input"}"#,
        );

        assert_eq!(
            listener.signal_for(6).fact,
            Some(HookFact::TurnEnded),
            "the turn really did end; an idle nudge is not evidence that it didn't"
        );
        assert_eq!(
            listener.text_for(6).1,
            None,
            "a finished row must not carry a question describing nothing it doesn't already say"
        );
        // ...and the session id a resume needs must survive the fold.
        assert_eq!(listener.session_id_for(6).as_deref(), Some("s-2"));
    }

    #[test]
    fn a_notification_still_reports_a_block_jerry_has_not_already_heard_about() {
        // The other side of the rule: nudges are folded in, never ignored. An agent Jerry last saw
        // *working* that hits a permission prompt must still land on `NeedsInput` off the
        // notification alone - which is the only signal at all for a block that fires no
        // `PermissionRequest`.
        let listener = HookListener::start().expect("listener must start");
        let port = listener.port();
        let token = listener.token().to_owned();

        post(
            port,
            &token,
            "event=PreToolUse&agent=7",
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#,
        );
        assert_eq!(listener.signal_for(7).fact, Some(HookFact::Working));

        post(
            port,
            &token,
            "event=Notification&agent=7",
            r#"{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission"}"#,
        );
        assert_eq!(listener.signal_for(7).fact, Some(HookFact::NeedsInput));
        assert_eq!(
            listener.text_for(7).1.as_deref(),
            Some("Claude needs your permission"),
            "with no better question on record, the notification's own message is the best there is"
        );

        // And a real transition after it still wins outright - the fold applies to nudges only.
        post(
            port,
            &token,
            "event=PostToolUse&agent=7",
            r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#,
        );
        assert_eq!(listener.signal_for(7).fact, Some(HookFact::Working));
        assert_eq!(listener.text_for(7).1, None);
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
