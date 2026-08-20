//! The loopback listener Claude Code's hooks report into (GitHub issue #239, phase 2).

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hooks::cursor_event;
use crate::hooks::event::{self, EditedFile, HookFact, HookReport, MAX_PAYLOAD_BYTES};
use crate::work_surface::agents::AgentId;

/// How long a single blocking read may wait for *some* data to arrive.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The wall-clock budget for one entire request, start to finish.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

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

/// Most agents [`HookInbox`] will track at once - see [`HookInbox::record`].
const MAX_TRACKED_AGENTS: usize = 512;

/// How long [`HookListener::drop`]'s self-connect may block the dropping thread - see that impl.
const SHUTDOWN_WAKE_TIMEOUT: Duration = Duration::from_millis(250);

/// Smallest write budget allowed for the reply, used when the read phase already consumed the
/// whole [`REQUEST_DEADLINE`] - see [`handle_connection`].
const REPLY_WRITE_FLOOR: Duration = Duration::from_millis(500);

/// How many un-drained agent edits [`EditLog`] will hold before it starts dropping the oldest.
const MAX_PENDING_EDITS: usize = 4096;

/// One agent's most recent hook fact, as stored for the rail to read, plus the two facts about
/// the *run as a whole* that a single latest-wins report structurally cannot carry (GitHub issue
/// #227).
#[derive(Debug, Clone)]
pub struct HookRecord {
    /// The parsed event - see [`crate::hooks::event::parse`].
    pub report: HookReport,
    /// When it arrived, for the freshness check in [`crate::rail::status::HookSignal`].
    pub received_at: Instant,
    /// How many turns this agent has really completed - one per `Stop`
    /// ([`HookFact::TurnEnded`]) it has reported.
    pub turns: u32,
    /// The first prompt this agent's human typed, latched once and never overwritten - the run's
    /// **title** (`crate::hooks::event::HookReport::prompt`).
    pub first_prompt: Option<String>,
}

/// What an agent's hooks have said about its **run**, as opposed to about its current state
/// (GitHub issue #227) - see [`HookRecord::turns`] and [`HookRecord::first_prompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFacts {
    /// Completed turns - one per `Stop`.
    pub turns: u32,
    /// The run's title: the first prompt its human typed, if hooks caught one.
    pub title: Option<String>,
}

/// Every agent's latest hook fact, shared between the listener thread and the UI thread.
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
    pub fn record(&mut self, id: AgentId, report: HookReport) {
        // GitHub issue #227's run-level accumulation, taken from the *incoming* payload before
        // any nudge merging: a turn really ended if this payload said so, and the first prompt is
        // whichever `UserPromptSubmit` arrived first. Both are carried forward from whatever this
        // agent already had - unlike the report itself, neither is ever replaced by a later
        // event, and neither is subject to the TTL below (a run's turn count does not become
        // untrue because the agent went quiet for half an hour).
        let previous_run_facts = self
            .latest
            .get(&id)
            .map(|record| (record.turns, record.first_prompt.clone()));
        let (previous_turns, previous_prompt) = previous_run_facts.unwrap_or((0, None));
        let turns = previous_turns.saturating_add(u32::from(
            report.kind == event::EventKind::Transition && report.fact == HookFact::TurnEnded,
        ));
        let first_prompt = previous_prompt.or_else(|| report.prompt.clone());

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
                turns,
                first_prompt,
            },
        );
    }

    /// Drops an agent's facts - called when the agent closes, so a recycled
    /// [`AgentId`] can never inherit a dead agent's status.
    pub fn forget(&mut self, id: AgentId) {
        self.latest.remove(&id);
    }
}

/// One agent's file write, as it came off the wire (GitHub issue #284).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEdit {
    /// Which agent - the `agent=` query parameter, i.e. the `JERRY_AGENT_ID` the spawn set.
    pub agent: AgentId,
    /// The file and the phase - see [`crate::hooks::event::EditedFile`].
    pub file: EditedFile,
    /// The file's content as it stood when a [`crate::hooks::event::EditPhase::Before`] event
    /// arrived - always `None` for an `After` event.
    pub before: Option<String>,
}

/// Every file write no reader has taken yet, oldest first.
#[derive(Debug, Default)]
pub struct EditLog {
    pending: VecDeque<AgentEdit>,
    /// How many entries were evicted un-drained since the last drain - a real number for a real
    /// log line, so an overflow is visible rather than silent.
    dropped: usize,
}

impl EditLog {
    pub fn record(&mut self, agent: AgentId, file: EditedFile, before: Option<String>) {
        while self.pending.len() >= MAX_PENDING_EDITS {
            self.pending.pop_front();
            self.dropped += 1;
        }
        self.pending.push_back(AgentEdit {
            agent,
            file,
            before,
        });
    }

    /// Takes everything pending, in arrival order, leaving the log empty. Returns
    /// `(edits, dropped_since_last_drain)`.
    pub fn drain(&mut self) -> (Vec<AgentEdit>, usize) {
        let dropped = std::mem::take(&mut self.dropped);
        (self.pending.drain(..).collect(), dropped)
    }

    /// Drops an agent's un-drained edits - called when the agent closes, so a recycled
    /// [`AgentId`] cannot be handed a dead agent's writes.
    pub fn forget(&mut self, id: AgentId) {
        self.pending.retain(|edit| edit.agent != id);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Folds a `Notification` into the fact Jerry already holds for the same agent, returning the
/// report that should actually be stored.
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
        // A `Notification` never carries an edit, so this only ever restores what the kept fact
        // already said. Delivery of edits does not depend on this field either way - they are
        // appended to [`EditLog`] straight off the wire, before any merging - but a stored report
        // whose `edit` disagreed with the rest of it would be a trap for the next reader.
        edit: previous.edit.clone(),
        // Same reasoning as `edit`: only a `UserPromptSubmit` ever carries a prompt, and a nudge
        // is never one, so this restores what the kept fact already said rather than blanking it.
        // The run title itself does not depend on this field surviving here either - see
        // [`HookRecord::first_prompt`], which latches the first prompt once and never re-reads
        // the stored report.
        prompt: previous.prompt.clone(),
    }
}

/// Holds one of the [`MAX_IN_FLIGHT`] connection slots, releasing it on drop.
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
    edits: Arc<Mutex<EditLog>>,
    shutdown: Arc<AtomicBool>,
}

impl HookListener {
    /// Binds `127.0.0.1:0`, reads back the real assigned port, generates this launch's token and
    /// starts the accept loop on a dedicated thread.
    pub fn start() -> std::io::Result<HookListener> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = generate_token();
        let inbox: Arc<Mutex<HookInbox>> = Arc::new(Mutex::new(HookInbox::default()));
        let edits: Arc<Mutex<EditLog>> = Arc::new(Mutex::new(EditLog::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_token = token.clone();
        let thread_inbox = Arc::clone(&inbox);
        let thread_edits = Arc::clone(&edits);
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
                    let handler_edits = Arc::clone(&thread_edits);
                    let spawned = std::thread::Builder::new()
                        .name("jerry-hook-conn".to_owned())
                        .spawn(move || {
                            let _slot = slot;
                            handle_connection(
                                stream,
                                &handler_token,
                                &handler_inbox,
                                &handler_edits,
                            );
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
            edits,
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

    /// Drops an agent's recorded facts - see [`HookInbox::forget`] and [`EditLog::forget`].
    pub fn forget(&self, id: AgentId) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.forget(id);
        }
        if let Ok(mut edits) = self.edits.lock() {
            edits.forget(id);
        }
    }

    /// Takes every file write reported since the last call, in arrival order (GitHub issue #284).
    /// Returns `(edits, dropped)`; `dropped` counts entries evicted un-drained by
    /// [`MAX_PENDING_EDITS`], which is a real (if never-yet-observed) failure worth logging rather
    /// than swallowing.
    pub fn drain_edits(&self) -> (Vec<AgentEdit>, usize) {
        let Ok(mut edits) = self.edits.lock() else {
            return (Vec::new(), 0);
        };
        edits.drain()
    }

    /// This agent's most recently reported real Claude Code `session_id` (GitHub issue #227),
    /// if it has ever reported one - the id `claude --resume`/`-r` takes, verified against a real
    /// binary (see [`crate::hooks::event::HookReport::session_id`]).
    pub fn session_id_for(&self, id: AgentId) -> Option<String> {
        let inbox = self.inbox.lock().ok()?;
        inbox.get(id)?.report.session_id.clone()
    }

    /// This agent's real run-level facts - its completed-turn count and the first prompt its human
    /// typed (GitHub issue #227). `RunFacts::default()` for an agent that has never reported a
    /// hook, or if the lock is poisoned.
    pub fn run_facts_for(&self, id: AgentId) -> RunFacts {
        let Ok(inbox) = self.inbox.lock() else {
            return RunFacts::default();
        };
        match inbox.get(id) {
            Some(record) => RunFacts {
                turns: record.turns,
                title: record.first_prompt.clone(),
            },
            None => RunFacts::default(),
        }
    }
}

impl Drop for HookListener {
    /// Signals the accept loop to stop and wakes it so it notices.
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
fn handle_connection(
    mut stream: TcpStream,
    token: &str,
    inbox: &Arc<Mutex<HookInbox>>,
    edits: &Arc<Mutex<EditLog>>,
) {
    // One absolute budget for the whole exchange, taken before the first byte is read - see
    // `REQUEST_DEADLINE`.
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let response =
        read_and_record(&mut stream, token, inbox, edits, deadline).unwrap_or(Response::BadRequest);

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
    edits: &Arc<Mutex<EditLog>>,
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
    // The route decides only which parser turns a payload into a `HookReport` - the auth, body
    // bounding, and everything downstream of getting one (edits/inbox recording) stay identical
    // and agent-agnostic (GitHub issue #479's second route, `/hook/cursor`).
    let parse: fn(&str, &[u8]) -> Option<HookReport> = match path {
        "/hook" => event::parse,
        "/hook/cursor" => cursor_event::parse,
        _ => return Some(Response::NotFound),
    };

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
    if let (Some(agent_id), Some(report)) = (agent_id, parse(&event_name, &body)) {
        // The edit is appended *before* the inbox merge and from the same parse, so it is never
        // affected by `merge_nudge` deciding the report itself is superseded - a file really was
        // written whatever the row's status ends up saying (GitHub issue #284).
        if let Some(file) = report.edit.clone() {
            // The "before" snapshot has to be taken now, on this thread, while the agent is still
            // *asking* to write - see `AgentEdit::before` for what happens if it is deferred.
            let before = match file.phase {
                event::EditPhase::Before => crate::provenance::store::snapshot_for_edit(
                    &crate::provenance::absolute_edit_path(&file),
                ),
                event::EditPhase::After => None,
            };
            if let Ok(mut edits) = edits.lock() {
                edits.record(agent_id, file, before);
            }
        }
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
fn arm_read(reader: &BufReader<TcpStream>, deadline: Instant) -> Option<()> {
    let left = time_left(deadline)?;
    reader
        .get_ref()
        .set_read_timeout(Some(left.min(READ_TIMEOUT)))
        .ok()
}

/// Reads one `\n`-terminated line under both a byte budget and an absolute deadline. Returns the
/// bytes consumed.
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
        post_to(port, token, "/hook", query, body)
    }

    /// [`post`], with the request path parameterized - GitHub issue #479's second route,
    /// `/hook/cursor`, is exercised through this rather than a separate near-duplicate helper.
    fn post_to(port: u16, token: &str, path: &str, query: &str, body: &str) -> String {
        raw_request(
            port,
            &format!(
                "POST {path}?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    /// One real edit-tool payload, in the shape a real `claude` 2.1.228 sends.
    fn edit_body(event: &str, file: &std::path::Path) -> String {
        format!(
            r#"{{"session_id":"5a4bef04","cwd":"/tmp/capture","hook_event_name":"{event}","tool_name":"Edit","tool_input":{{"file_path":"{}","old_string":"a","new_string":"b"}},"tool_use_id":"toolu_01"}}"#,
            file.display()
        )
    }

    #[test]
    fn every_file_write_in_a_burst_survives_the_drain_rather_than_only_the_last_one() {
        // The whole reason `EditLog` is a second structure rather than another field on
        // `HookRecord`: a turn that writes six files is six facts, and the inbox next to it keeps
        // exactly one. Stored latest-wins, five of these would be gone before the UI thread looked
        // (GitHub issue #284).
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = HookListener::start().expect("listener");
        let files: Vec<std::path::PathBuf> = (0..6)
            .map(|n| dir.path().join(format!("f{n}.rs")))
            .collect();
        for file in &files {
            std::fs::write(file, "before\n").expect("seed");
            post(
                listener.port(),
                listener.token(),
                "event=PostToolUse&agent=7",
                &edit_body("PostToolUse", file),
            );
        }

        let (edits, dropped) = listener.drain_edits();
        assert_eq!(dropped, 0);
        assert_eq!(
            edits
                .iter()
                .map(|edit| edit.file.path.clone())
                .collect::<Vec<_>>(),
            files
                .iter()
                .map(|file| file.display().to_string())
                .collect::<Vec<_>>(),
            "every write, in arrival order"
        );
        assert!(
            edits.iter().all(|edit| edit.agent == 7),
            "and each under the agent that reported it"
        );

        let (drained_again, _) = listener.drain_edits();
        assert!(
            drained_again.is_empty(),
            "a drain really takes them - a second reader must not replay the same edits"
        );
    }

    #[test]
    fn a_pre_tool_use_captures_the_file_as_it_stands_before_the_agent_writes() {
        // The timing this whole field exists for: the snapshot is read while the request is being
        // handled, so a write that lands milliseconds later cannot get into it. Deferring this to
        // the UI thread's drain would make every recorded edit diff clean against itself.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "before\n").expect("seed");

        let listener = HookListener::start().expect("listener");
        post(
            listener.port(),
            listener.token(),
            "event=PreToolUse&agent=7",
            &edit_body("PreToolUse", &file),
        );
        std::fs::write(&file, "after\n").expect("agent write");
        post(
            listener.port(),
            listener.token(),
            "event=PostToolUse&agent=7",
            &edit_body("PostToolUse", &file),
        );

        let (edits, _) = listener.drain_edits();
        assert_eq!(edits.len(), 2);
        assert_eq!(
            edits[0].before.as_deref(),
            Some("before\n"),
            "the before snapshot must be the content as of the PreToolUse, not as of the drain"
        );
        assert_eq!(
            edits[1].before, None,
            "a PostToolUse has nothing to snapshot - the file is read at record time"
        );
    }

    #[test]
    fn an_event_that_writes_no_file_never_reaches_the_edit_log() {
        let listener = HookListener::start().expect("listener");
        for (event, body) in [
            (
                "PostToolUse",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/a.rs"}}"#,
            ),
            (
                "PostToolUse",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
            ),
            ("Stop", r#"{"hook_event_name":"Stop"}"#),
            (
                "Notification",
                r#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"m"}"#,
            ),
        ] {
            post(
                listener.port(),
                listener.token(),
                &format!("event={event}&agent=7"),
                body,
            );
        }
        assert!(listener.drain_edits().0.is_empty());
    }

    #[test]
    fn forgetting_an_agent_drops_its_undrained_edits_so_a_recycled_id_cannot_inherit_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "before\n").expect("seed");
        let listener = HookListener::start().expect("listener");
        for agent in [7, 8] {
            post(
                listener.port(),
                listener.token(),
                &format!("event=PostToolUse&agent={agent}"),
                &edit_body("PostToolUse", &file),
            );
        }

        listener.forget(7);
        let (edits, _) = listener.drain_edits();
        assert_eq!(
            edits.iter().map(|edit| edit.agent).collect::<Vec<_>>(),
            vec![8],
            "the closed agent's writes go with it; the live agent's stay"
        );
    }

    #[test]
    fn the_edit_log_sheds_its_oldest_entries_rather_than_growing_without_bound() {
        let mut log = EditLog::default();
        for index in 0..MAX_PENDING_EDITS + 3 {
            log.record(
                7,
                EditedFile {
                    phase: event::EditPhase::After,
                    path: format!("f{index}.rs"),
                    cwd: None,
                },
                None,
            );
        }
        let (edits, dropped) = log.drain();
        assert_eq!(edits.len(), MAX_PENDING_EDITS);
        assert_eq!(dropped, 3, "an overflow is counted, not swallowed");
        assert_eq!(
            edits.first().map(|edit| edit.file.path.as_str()),
            Some("f3.rs"),
            "the oldest go first - their files are the ones most likely already overwritten"
        );
        assert_eq!(log.drain().1, 0, "the dropped count resets with the drain");
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
        assert_eq!(listener.signal_for(8).fact, None);
    }

    #[test]
    fn a_real_tokened_post_to_the_cursor_route_round_trips_through_the_cursor_parser() {
        // GitHub issue #479's second route: `/hook/cursor` must dispatch to
        // `crate::hooks::cursor_event::parse`, not `event::parse` - a Cursor-shaped payload
        // (`conversation_id`, `preToolUse`) is not valid under Claude's own schema at all, so a
        // round trip through the *wrong* parser would record nothing rather than the right fact.
        let listener = HookListener::start().expect("listener must start");
        let body = r#"{"conversation_id":"conv-9","tool_name":"Bash"}"#;
        let response = post_to(
            listener.port(),
            listener.token(),
            "/hook/cursor",
            "event=preToolUse&agent=42",
            body,
        );
        assert!(
            response.starts_with("HTTP/1.1 204"),
            "expected 204, got {response:?}"
        );
        assert_eq!(listener.signal_for(42).fact, Some(HookFact::Working));
        assert_eq!(
            listener.text_for(42).0.as_deref(),
            Some("Bash"),
            "the Cursor parser's own activity formatting must be the one that ran"
        );
        assert_eq!(
            listener.session_id_for(42).as_deref(),
            Some("conv-9"),
            "conversation_id is Cursor's session_id equivalent"
        );
    }

    #[test]
    fn an_unknown_route_is_a_real_404_not_a_silent_accept() {
        let listener = HookListener::start().expect("listener must start");
        let response = post_to(
            listener.port(),
            listener.token(),
            "/hook/not-a-real-route",
            "event=Stop&agent=1",
            "{}",
        );
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "expected 404, got {response:?}"
        );
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

        let wrong = post(
            listener.port(),
            "0000000000000000000000000000000000000000000000000000000000000000",
            "event=Stop&agent=3",
            body,
        );
        assert!(wrong.starts_with("HTTP/1.1 401"), "got {wrong:?}");

        let missing = raw_request(
            listener.port(),
            &format!(
                "POST /hook?event=Stop&agent=3 HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(missing.starts_with("HTTP/1.1 400"), "got {missing:?}");

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
            format!("POST /hook?event=Stop&agent=1 HTTP/1.1\r\nX-Huge: {}", "A".repeat(MAX_HEAD_BYTES * 2)),
        ];
        for request in hostile {
            let _ = raw_request(port, &request);
        }

        assert_eq!(
            listener.signal_for(1).fact,
            None,
            "none of the hostile requests may have recorded a fact"
        );

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

        // Every slot occupied by a client that drips for ~25s and never completes a request. Each
        // reports when the server let go of it - a failed write is the drip-feeder's own view of
        // the handler giving up, and counting them is what turns the wait below into an observed
        // event rather than a guessed duration.
        let cut_off = Arc::new(AtomicUsize::new(0));
        let mut drips = Vec::new();
        for _ in 0..MAX_IN_FLIGHT {
            let cut_off = Arc::clone(&cut_off);
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
                cut_off.fetch_add(1, Ordering::SeqCst);
            }));
        }

        // Deliberately *not* polled with a probe request: a probe competes with the drip-feeders
        // for the same `MAX_IN_FLIGHT` slots, and one that wins a slot at startup leaves the
        // server permanently one short of saturation - the test then measures its own polling
        // instead of the server. Wait on the drip-feeders' own signal instead, which perturbs
        // nothing. The bound is absolute rather than derived from `REQUEST_DEADLINE`, and well
        // inside the drippers' own ~25s lifetime, so reaching it at all means the handlers really
        // did give up rather than the clients running out of patience - see the sibling test.
        assert!(
            test_support::wait_until(Duration::from_secs(15), || cut_off.load(Ordering::SeqCst)
                == MAX_IN_FLIGHT),
            "every handler must give up on its drip-feeder on its own, but only {} of {} did",
            cut_off.load(Ordering::SeqCst),
            MAX_IN_FLIGHT
        );

        // And the slots they held are genuinely free again, not merely accounted free.
        let good = post(
            port,
            listener.token(),
            "event=Stop&agent=99",
            r#"{"hook_event_name":"Stop"}"#,
        );
        assert!(
            good.starts_with("HTTP/1.1 204"),
            "a real hook must be served again once the deadline cuts the slow clients off, got \
             {good:?}"
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
    fn a_runs_title_and_turn_count_come_off_its_own_real_hook_stream() {
        let listener = HookListener::start().expect("listener must start");
        let port = listener.port();
        let token = listener.token().to_owned();

        assert_eq!(
            listener.run_facts_for(11),
            crate::hooks::server::RunFacts::default(),
            "an agent that has reported nothing has no run facts at all"
        );

        post(
            port,
            &token,
            "event=UserPromptSubmit&agent=11",
            r#"{"session_id":"s-11","hook_event_name":"UserPromptSubmit","prompt":"Reproduce the refresh race in a test"}"#,
        );
        assert_eq!(
            listener.run_facts_for(11).title.as_deref(),
            Some("Reproduce the refresh race in a test")
        );
        assert_eq!(listener.run_facts_for(11).turns, 0);

        post(
            port,
            &token,
            "event=Stop&agent=11",
            r#"{"session_id":"s-11","hook_event_name":"Stop"}"#,
        );
        post(
            port,
            &token,
            "event=Stop&agent=11",
            r#"{"session_id":"s-11","hook_event_name":"Stop"}"#,
        );
        assert_eq!(
            listener.run_facts_for(11).turns,
            2,
            "each real Stop is one completed turn"
        );

        post(
            port,
            &token,
            "event=UserPromptSubmit&agent=11",
            r#"{"session_id":"s-11","hook_event_name":"UserPromptSubmit","prompt":"yes"}"#,
        );
        assert_eq!(
            listener.run_facts_for(11).title.as_deref(),
            Some("Reproduce the refresh race in a test"),
            "the title is the first prompt, never the latest one"
        );

        post(
            port,
            &token,
            "event=Notification&agent=11",
            r#"{"session_id":"s-11","hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input"}"#,
        );
        assert_eq!(listener.run_facts_for(11).turns, 2);
    }

    #[test]
    fn forgetting_an_agent_drops_its_run_facts_as_well_as_its_status() {
        let listener = HookListener::start().expect("listener must start");
        let port = listener.port();
        let token = listener.token().to_owned();

        post(
            port,
            &token,
            "event=UserPromptSubmit&agent=12",
            r#"{"session_id":"s-12","hook_event_name":"UserPromptSubmit","prompt":"Port the planner tests"}"#,
        );
        post(
            port,
            &token,
            "event=Stop&agent=12",
            r#"{"session_id":"s-12","hook_event_name":"Stop"}"#,
        );
        assert_eq!(listener.run_facts_for(12).turns, 1);

        listener.forget(12);
        assert_eq!(
            listener.run_facts_for(12),
            crate::hooks::server::RunFacts::default()
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
