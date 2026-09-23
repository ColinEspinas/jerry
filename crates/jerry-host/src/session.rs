//! The session table (`docs/architecture/decisions.md` §23): every PTY this host owns or is
//! tracking, agents and plain terminal tabs alike. [`SessionManager::spawn`] owns the real
//! `jerry_pty::PtySession` and relays its output to whichever client attached at spawn time -
//! single-consumer for this issue, see [`SessionHandle`]'s own docs. `AgentTable`'s pre-existing
//! register/forget bookkeeping (kept working for `jerry-app`'s callers during its own migration)
//! creates an entry with no owned process at all - this module is the one source of truth for
//! both kinds of entry, so a caller checking agent confinement and one listing sessions never see
//! two different tables.

use crate::fanout::Fanout;
use futures::channel::mpsc as futures_mpsc;
use futures::executor::block_on;
use futures::{SinkExt, StreamExt};
use jerry_core::{
    AgentId, ExitStatusWire, Message, SessionAgentInfo, SessionId, SessionKind, SessionRecord,
};
use jerry_pty::{PtyError, PtyOutput, PtySession, SpawnOptions};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many items [`SessionManager::spawn`]'s relay channel buffers before the relay thread
/// blocks - the same bound `jerry_pty`'s own output channel uses, so relaying never becomes the
/// tighter constraint on throughput.
const RELAY_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no session with id {0}")]
    NotFound(SessionId),
    #[error("session {0} has no process this host owns")]
    NotOwned(SessionId),
    #[error(transparent)]
    Pty(#[from] PtyError),
}

/// [`jerry_core::ExitStatusWire`]'s conversion from the real `jerry_pty::ExitStatus` - a free
/// function, not a `From` impl, since neither type is local to this crate (the orphan rule).
fn exit_status_wire(status: &jerry_pty::ExitStatus) -> ExitStatusWire {
    ExitStatusWire {
        success: status.success(),
        code: status.exit_code(),
        signal: status.signal().map(str::to_owned),
    }
}

struct Entry {
    record: SessionRecord,
    /// `Some` only for a session this host actually spawned and owns the process for
    /// ([`SessionManager::spawn`]) - `None` for an `AgentTable`-compatibility registration, which
    /// carries no live process handle at all (see the module docs). Kept here, not just handed
    /// out once, so a later [`SessionManager::handle_for`] ("attach") can hand the *same* handle
    /// to whichever client asks - `SessionHandle::take_output`'s own `Option::take` is what
    /// actually enforces single-consumer, not this table.
    handle: Option<Arc<SessionHandle>>,
}

/// The data-plane adapter handed to the one attaching client at spawn time (decisions.md §23):
/// bytes and input, neither travelling through `Call`/`Report`. Resize, kill, attach and exit
/// stay on the control plane (`SessionManager::resize`/`kill`, `SessionsQuery`,
/// `event/session-exited`) - this type exists only for the byte stream and writing back into it.
///
/// Single-consumer: [`Self::take_output`] returns the relayed receiver exactly once. A second
/// pane attaching to the same session (were that ever wired up) would need a real per-session
/// fan-out here instead of a plain `Option::take` - out of scope for this issue, which has at
/// most one consumer per session.
pub struct SessionHandle {
    id: SessionId,
    process: Arc<Mutex<PtySession>>,
    output: Mutex<Option<futures_mpsc::Receiver<PtyOutput>>>,
}

impl SessionHandle {
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Writes to the pty's input side - the same non-blocking-enqueue contract
    /// `jerry_pty::PtySession::write_input` documents.
    pub fn write_input(&self, data: &[u8]) -> Result<(), PtyError> {
        lock(&self.process).write_input(data)
    }

    /// The relayed output stream: raw bytes, then at most one terminal `Exited` - see
    /// [`SessionManager::spawn`]'s relay thread for why this is a relay of, not the same
    /// receiver as, `jerry_pty::PtySession::take_output`'s own stream. `None` once already taken.
    pub fn take_output(&self) -> Option<futures_mpsc::Receiver<PtyOutput>> {
        lock(&self.output).take()
    }

    pub fn process_id(&self) -> Option<u32> {
        lock(&self.process).process_id()
    }

    /// `jerry_pty::PtySession::pause` - real `SIGSTOP`, unix only (see that method's own docs for
    /// the platform scope). In-process, like [`Self::write_input`]: pausing a pane's own process
    /// is not an authorization concern the way spawn/resize/kill are, so it stays off the control
    /// plane too.
    pub fn pause(&self) -> Result<(), PtyError> {
        lock(&self.process).pause()
    }

    /// `jerry_pty::PtySession::resume` - the real `SIGCONT` counterpart to [`Self::pause`].
    pub fn resume(&self) -> Result<(), PtyError> {
        lock(&self.process).resume()
    }

    /// `jerry_pty::PtySession::shutdown`: blocks until this session's whole process tree is
    /// confirmed dead and reaped, unlike [`SessionManager::kill`]'s non-blocking signal-only
    /// contract. For a caller that must be certain the process is gone before proceeding (the
    /// worktree-discard flow, GitHub issue #470, before removing a directory a live child's cwd
    /// would otherwise keep locked on Windows) - call off the UI thread.
    pub fn shutdown(&self) -> Result<(), PtyError> {
        lock(&self.process).shutdown()
    }

    /// The same live process [`SessionManager::resize`]/[`SessionManager::kill`] act on - private
    /// to this module: any other caller acts through those, or through this handle's own
    /// data-plane methods above, never by reaching into the process directly.
    fn process_arc(&self) -> Arc<Mutex<PtySession>> {
        Arc::clone(&self.process)
    }
}

#[derive(Clone)]
pub struct SessionManager {
    entries: Arc<Mutex<HashMap<SessionId, Entry>>>,
    fanout: Fanout,
    next_id: Arc<AtomicU64>,
}

impl SessionManager {
    pub(crate) fn new(fanout: Fanout) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            fanout,
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// This host's own `AgentTable`-shaped view - see `crate::AgentTable`.
    pub(crate) fn agent_table(&self) -> crate::AgentTable {
        crate::AgentTable(self.clone())
    }

    fn next_session_id(&self) -> SessionId {
        SessionId(format!(
            "session-{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        ))
    }

    /// Spawns a real PTY via `jerry_pty::spawn`, owned by this host from then on: the process,
    /// the relay thread that observes its exit, and the record `SessionsQuery`/`AgentsQuery`
    /// report. Bytes come back on the returned handle, never through `Call`/`Report`; a distinct
    /// id every time, even for a repeat spawn in the same worktree.
    pub fn spawn(
        &self,
        worktree: PathBuf,
        agent: Option<SessionAgentInfo>,
        options: SpawnOptions,
    ) -> Result<(SessionId, Arc<SessionHandle>), SessionSpawnError> {
        let mut session = jerry_pty::spawn(options)?;
        let raw_output = session
            .take_output()
            .ok_or(SessionSpawnError::NoOutputStream)?;
        let id = self.next_session_id();
        let (relay_tx, relay_rx) = futures_mpsc::channel(RELAY_CHANNEL_CAPACITY);
        let record = SessionRecord {
            id: id.clone(),
            kind: SessionKind::Pty,
            worktree,
            agent,
            started_at: unix_now(),
            exit: None,
        };
        let handle = Arc::new(SessionHandle {
            id: id.clone(),
            process: Arc::new(Mutex::new(session)),
            output: Mutex::new(Some(relay_rx)),
        });
        lock(&self.entries).insert(
            id.clone(),
            Entry {
                record,
                handle: Some(Arc::clone(&handle)),
            },
        );
        spawn_relay(id.clone(), raw_output, relay_tx, self.clone())
            .map_err(SessionSpawnError::Relay)?;
        Ok((id, handle))
    }

    /// Hands back the same handle [`Self::spawn`] returned, for a caller that only has the
    /// [`SessionId`] (an "attach" reached some other way than the spawn call itself - decisions.md
    /// §23). `None` for an unknown id or an `AgentTable`-compatibility registration with no
    /// process. Cloning the `Arc` does not grant a second output stream:
    /// [`SessionHandle::take_output`]'s own `Option::take` still enforces single-consumer.
    pub fn handle_for(&self, id: &SessionId) -> Option<Arc<SessionHandle>> {
        lock(&self.entries).get(id)?.handle.clone()
    }

    /// Resizes a session's real pty. `NotOwned` for an `AgentTable`-compatibility registration,
    /// which has no process to resize.
    pub fn resize(&self, id: &SessionId, rows: u16, cols: u16) -> Result<(), SessionError> {
        self.process_for(id)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resize(rows, cols)
            .map_err(SessionError::from)
    }

    /// Kills a session's process tree. Non-blocking, like `jerry_pty::PtySession::kill` itself:
    /// the real exit status arrives asynchronously as an `Exited` item, observed by the same
    /// relay thread [`Self::spawn`] started, which publishes `event/session-exited` from it.
    pub fn kill(&self, id: &SessionId) -> Result<(), SessionError> {
        self.process_for(id)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill()
            .map_err(SessionError::from)
    }

    fn process_for(&self, id: &SessionId) -> Result<Arc<Mutex<PtySession>>, SessionError> {
        let entries = lock(&self.entries);
        let entry = entries
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.clone()))?;
        entry
            .handle
            .as_ref()
            .map(|handle| handle.process_arc())
            .ok_or_else(|| SessionError::NotOwned(id.clone()))
    }

    /// Every session this host is tracking right now, for `SessionsQuery` - dispatch.rs's own
    /// answer for a request `execute_locally` can never resolve on its own (§15).
    pub fn list(&self) -> Vec<SessionRecord> {
        lock(&self.entries)
            .values()
            .map(|entry| entry.record.clone())
            .collect()
    }

    /// `AgentTable::register`'s real implementation: an entry with no owned process, tagged by
    /// `agent_id` alone (never a spawned `SessionId`) so [`Self::forget_agent`] can find it back.
    pub(crate) fn register_agent(&self, agent_id: AgentId, worktree: PathBuf, kind: String) {
        let id = agent_session_id(&agent_id);
        let record = SessionRecord {
            id: id.clone(),
            kind: SessionKind::Pty,
            worktree,
            agent: Some(SessionAgentInfo { kind, agent_id }),
            started_at: unix_now(),
            exit: None,
        };
        lock(&self.entries).insert(
            id,
            Entry {
                record,
                handle: None,
            },
        );
    }

    pub(crate) fn forget_agent(&self, agent_id: &AgentId) {
        lock(&self.entries).remove(&agent_session_id(agent_id));
    }

    pub(crate) fn worktree_of_agent(&self, agent_id: &AgentId) -> Option<PathBuf> {
        lock(&self.entries)
            .get(&agent_session_id(agent_id))
            .map(|entry| entry.record.worktree.clone())
    }

    pub(crate) fn agent_count(&self) -> usize {
        lock(&self.entries)
            .values()
            .filter(|entry| entry.record.agent.is_some())
            .count()
    }

    pub(crate) fn agent_entries(&self) -> Vec<(AgentId, PathBuf, String)> {
        lock(&self.entries)
            .values()
            .filter_map(|entry| {
                let agent = entry.record.agent.as_ref()?;
                Some((
                    agent.agent_id.clone(),
                    entry.record.worktree.clone(),
                    agent.kind.clone(),
                ))
            })
            .collect()
    }

    /// Records `id`'s exit and publishes `event/session-exited`, once - called by the relay
    /// thread [`Self::spawn`] started, the moment it observes a real `Exited` item. A no-op for
    /// an id that has since been forgotten (an `AgentTable`-only entry `forget_agent` already
    /// removed, or a race with a very short-lived session).
    fn record_exit(&self, id: &SessionId, status: &jerry_pty::ExitStatus) {
        let wire = exit_status_wire(status);
        let recorded = {
            let mut entries = lock(&self.entries);
            match entries.get_mut(id) {
                Some(entry) => {
                    entry.record.exit = Some(wire.clone());
                    true
                }
                None => false,
            }
        };
        if recorded {
            self.fanout.broadcast(Message::Notification {
                method: "event/session-exited".into(),
                params: serde_json::json!({ "id": id, "status": wire }),
            });
        }
    }
}

/// [`SessionManager::spawn`]'s errors: a real `jerry_pty` failure, a freshly spawned session with
/// no output stream (never actually observed - `jerry_pty::spawn` always leaves one, see
/// `PtySession::take_output`'s own docs - but handled honestly rather than `unwrap`ped), or a
/// failure to start the relay thread that observes this session's exit.
#[derive(Debug, thiserror::Error)]
pub enum SessionSpawnError {
    #[error(transparent)]
    Pty(#[from] PtyError),
    #[error("a freshly spawned session had no output stream")]
    NoOutputStream,
    #[error("could not start this session's exit-observing relay thread: {0}")]
    Relay(io::Error),
}

/// A session id for an `AgentTable`-compatibility registration (decisions.md §23): deterministic
/// in `agent_id` alone, so [`SessionManager::forget_agent`]/[`SessionManager::worktree_of_agent`]
/// can find the same entry back without their own separate index.
fn agent_session_id(agent_id: &AgentId) -> SessionId {
    SessionId(format!("agent:{agent_id}"))
}

/// Drains `raw` (the real `jerry_pty::PtySession` output stream) on a dedicated thread, forwarding
/// every item to `relay_tx` in the exact order received - preserving `jerry_pty`'s own
/// Exited-last contract (`docs/architecture/decisions.md` §8's amendment) - and recording the
/// session's exit on `manager` the moment `Exited` is observed, before forwarding it downstream.
/// Ends the moment `Exited` is forwarded, or the moment the downstream receiver (the attached
/// client) is dropped, whichever comes first.
fn spawn_relay(
    id: SessionId,
    mut raw: futures_mpsc::Receiver<PtyOutput>,
    mut relay_tx: futures_mpsc::Sender<PtyOutput>,
    manager: SessionManager,
) -> io::Result<()> {
    thread::Builder::new()
        .name("jerry-host-session-relay".into())
        .spawn(move || {
            while let Some(item) = block_on(raw.next()) {
                if let PtyOutput::Exited(status) = &item {
                    manager.record_exit(&id, status);
                }
                let exited = matches!(item, PtyOutput::Exited(_));
                if block_on(relay_tx.send(item)).is_err() || exited {
                    break;
                }
            }
        })
        .map(|_join_handle| ())
}

/// Real wall-clock seconds since the Unix epoch, matching `jerry_app::work_surface::agents::
/// unix_now`'s own convention and `unwrap_or(0)` fallback for a system clock set before 1970.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod session_manager_tests {
    use super::SessionHandle;
    use super::SessionManager;
    use crate::fanout::Fanout;
    use jerry_pty::{PtyOutput, SpawnOptions};
    use std::path::PathBuf;
    use std::time::Duration;
    use test_support::wait_until;

    /// A real, short-lived shell command for this platform - `cmd /c` on Windows, `sh -c`
    /// elsewhere, matching the issue's own test plan.
    fn shell_options(script: &str) -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd").args(["/c", script])
        } else {
            SpawnOptions::new("sh").args(["-c", script])
        }
    }

    /// ConPTY's startup Device Status Report query: it withholds all child output until
    /// something answers it. `crates/jerry-app`'s real pane answers this from its own
    /// `alacritty_terminal`-backed grid; this table has none, so a test holding a bare
    /// [`SessionHandle`] answers it by hand, exactly like `jerry-pty`'s own bare-`PtySession`
    /// tests do for the identical reason (see that crate's `answer_cursor_position_query`) - a
    /// no-op off Windows, where nothing withholds output pending it.
    #[cfg(windows)]
    const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
    #[cfg(windows)]
    const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

    #[cfg(windows)]
    fn answer_cursor_position_query(handle: &SessionHandle, seen: &[u8], answered: &mut bool) {
        if !*answered
            && seen
                .windows(CURSOR_POSITION_QUERY.len())
                .any(|window| window == CURSOR_POSITION_QUERY)
        {
            let _ = handle.write_input(CURSOR_POSITION_REPORT);
            *answered = true;
        }
    }
    #[cfg(not(windows))]
    fn answer_cursor_position_query(_handle: &SessionHandle, _seen: &[u8], _answered: &mut bool) {}

    /// `test_support::wait_until` over a non-blocking `try_recv`, so a stalled stream fails this
    /// test instead of hanging it - the same shape `jerry-pty`'s own `recv_timeout` test helper
    /// uses, since the async channel has no blocking-with-timeout receive of its own.
    fn recv_timeout(
        rx: &mut futures::channel::mpsc::Receiver<PtyOutput>,
        timeout: Duration,
    ) -> Option<PtyOutput> {
        let mut item = None;
        wait_until(timeout, || match rx.try_recv() {
            Ok(value) => {
                item = Some(value);
                true
            }
            Err(err) if err.is_closed() => true,
            Err(_empty) => false,
        });
        item
    }

    /// Drains `handle`'s output to its terminal `Exited` item, answering the Windows cursor
    /// position query along the way - bounded by `timeout`, and fails loudly rather than hanging
    /// if `Exited` never arrives.
    fn drain_until_exit(
        handle: &SessionHandle,
        rx: &mut futures::channel::mpsc::Receiver<PtyOutput>,
        timeout: Duration,
    ) -> (Vec<u8>, jerry_pty::ExitStatus) {
        let mut bytes = Vec::new();
        let mut answered = false;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out draining output: {:?}",
                String::from_utf8_lossy(&bytes)
            );
            match recv_timeout(rx, remaining) {
                Some(PtyOutput::Bytes(chunk)) => {
                    bytes.extend_from_slice(&chunk);
                    answer_cursor_position_query(handle, &bytes, &mut answered);
                }
                Some(PtyOutput::Exited(status)) => return (bytes, status),
                None => panic!(
                    "the output stream produced nothing further: {:?}",
                    String::from_utf8_lossy(&bytes)
                ),
            }
        }
    }

    #[test]
    fn spawn_delivers_bytes_then_exit_and_kill_is_observed_the_same_way() {
        let manager = SessionManager::new(Fanout::default());
        let (id, handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options("echo hello"))
            .expect("spawn");
        let mut output = handle.take_output().expect("output stream");

        let (bytes, status) = drain_until_exit(&handle, &mut output, Duration::from_secs(10));
        assert!(
            String::from_utf8_lossy(&bytes).contains("hello"),
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(status.success(), "{status:?}");

        assert!(
            wait_until(Duration::from_secs(5), || manager
                .list()
                .iter()
                .find(|record| record.id == id)
                .and_then(|record| record.exit.as_ref())
                .is_some()),
            "the manager's own record must reflect the observed exit"
        );
    }

    #[test]
    fn a_second_spawn_in_the_same_worktree_gets_a_distinct_id() {
        let manager = SessionManager::new(Fanout::default());
        let (first, first_handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options("echo one"))
            .expect("first spawn");
        let (second, second_handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options("echo two"))
            .expect("second spawn");
        assert_ne!(first, second);

        // Every spawned process is guarded: drained to a real exit, not left running.
        let mut first_output = first_handle.take_output().expect("output stream");
        drain_until_exit(&first_handle, &mut first_output, Duration::from_secs(10));
        let mut second_output = second_handle.take_output().expect("output stream");
        drain_until_exit(&second_handle, &mut second_output, Duration::from_secs(10));
    }

    #[test]
    fn killing_a_live_session_is_observed_as_a_real_exit() {
        let manager = SessionManager::new(Fanout::default());
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let (id, handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options(sleep))
            .expect("spawn");
        let mut output = handle.take_output().expect("output stream");

        manager.kill(&id).expect("kill");
        let (_bytes, status) = drain_until_exit(&handle, &mut output, Duration::from_secs(10));
        assert!(
            !status.success(),
            "a killed process must not report success: {status:?}"
        );
    }

    #[test]
    fn resizing_or_killing_an_unknown_id_is_a_real_typed_error() {
        let manager = SessionManager::new(Fanout::default());
        let unknown = jerry_core::SessionId::from("no-such-session");
        assert!(matches!(
            manager.resize(&unknown, 24, 80),
            Err(super::SessionError::NotFound(_))
        ));
        assert!(matches!(
            manager.kill(&unknown),
            Err(super::SessionError::NotFound(_))
        ));
    }

    #[test]
    fn an_agent_table_registration_is_listed_but_cannot_be_resized_or_killed() {
        let manager = SessionManager::new(Fanout::default());
        let agent_id = jerry_core::AgentId::from("agent-1");
        manager.register_agent(agent_id.clone(), PathBuf::from("/repo"), "Claude".into());

        let sessions = manager.list();
        let entry = sessions
            .iter()
            .find(|record| {
                record
                    .agent
                    .as_ref()
                    .is_some_and(|agent| agent.agent_id == agent_id)
            })
            .expect("the registration is listed as a session");
        assert!(entry.exit.is_none());

        assert!(matches!(
            manager.resize(&entry.id, 24, 80),
            Err(super::SessionError::NotOwned(_))
        ));
        assert!(matches!(
            manager.kill(&entry.id),
            Err(super::SessionError::NotOwned(_))
        ));

        manager.forget_agent(&agent_id);
        assert!(manager.worktree_of_agent(&agent_id).is_none());
    }
}
