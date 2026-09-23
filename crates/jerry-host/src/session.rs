//! The session table (`docs/architecture/decisions.md` §23): every PTY this host owns or is
//! tracking, agents and plain terminal tabs alike. [`SessionManager::spawn`] owns the real
//! `jerry_pty::PtySession` and relays its output to whichever client attached at spawn time -
//! single-consumer for this issue, see [`SessionHandle`]'s own docs. `AgentTable`'s pre-existing
//! register/forget bookkeeping (kept working for `jerry-app`'s callers during its own migration)
//! creates an entry with no owned process at all - this module is the one source of truth for
//! both kinds of entry, so a caller checking agent confinement and one listing sessions never see
//! two different tables.

use crate::data_plane::{DataPlane, DataPlaneBindError};
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

/// [`SessionManager::attach`]'s errors - see `command/session-attach`'s own docs
/// (`jerry_core::session::SessionAttach`) for how each maps onto the wire.
#[derive(Debug, thiserror::Error)]
pub enum SessionAttachError {
    #[error("no session with id {0}")]
    NotFound(SessionId),
    #[error("session {0} already has an attached client")]
    AlreadyAttached(SessionId),
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
    /// This session's real data-plane socket (`crate::data_plane`, decisions.md §24) - `Some`
    /// exactly when `handle` is, bound the moment [`SessionManager::spawn`] starts the session and
    /// shut down (socket file removed) by [`SessionManager::forget_session`] or
    /// [`SessionManager::shutdown_all`].
    data_plane: Option<Arc<DataPlane>>,
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
    /// Where every session's own [`DataPlane`] binds its socket - the same directory the host's
    /// own control-plane socket and registry descriptor live under.
    sockets_dir: PathBuf,
}

impl SessionManager {
    pub(crate) fn new(fanout: Fanout, sockets_dir: PathBuf) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            fanout,
            next_id: Arc::new(AtomicU64::new(0)),
            sockets_dir,
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
        let process_id = session.process_id();
        let (relay_tx, relay_rx) = futures_mpsc::channel(RELAY_CHANNEL_CAPACITY);
        let record = SessionRecord {
            id: id.clone(),
            kind: SessionKind::Pty,
            worktree,
            agent,
            started_at: unix_now(),
            exit: None,
            process_id,
        };
        let process = Arc::new(Mutex::new(session));
        let data_plane = DataPlane::bind(&self.sockets_dir, Arc::clone(&process))?;
        let handle = Arc::new(SessionHandle {
            id: id.clone(),
            process,
            output: Mutex::new(Some(relay_rx)),
        });
        lock(&self.entries).insert(
            id.clone(),
            Entry {
                record,
                handle: Some(Arc::clone(&handle)),
                data_plane: Some(Arc::clone(&data_plane)),
            },
        );
        spawn_relay(id.clone(), raw_output, relay_tx, data_plane, self.clone())
            .map_err(SessionSpawnError::Relay)?;
        Ok((id, handle))
    }

    /// [`crate::data_plane::DataPlane`]'s own socket path for an already-spawned session -
    /// `command/session-attach`'s real answer (`docs/architecture/decisions.md` §24). Checked
    /// against the socket layer's own `attached` flag as an earlier, friendlier rejection of the
    /// common case; the socket layer itself is what actually enforces "exactly one attach at a
    /// time" (`crate::data_plane`'s own module docs) against a race with this check.
    pub fn attach(&self, id: &SessionId) -> Result<PathBuf, SessionAttachError> {
        let entries = lock(&self.entries);
        let entry = entries
            .get(id)
            .ok_or_else(|| SessionAttachError::NotFound(id.clone()))?;
        let data_plane = entry
            .data_plane
            .as_ref()
            .ok_or_else(|| SessionAttachError::NotFound(id.clone()))?;
        if data_plane.attached() {
            return Err(SessionAttachError::AlreadyAttached(id.clone()));
        }
        Ok(data_plane.socket_path().to_path_buf())
    }

    /// [`crate::data_plane::DataPlane::buffered_len`] for the session's own pre-attach buffer -
    /// `0` for an unknown id, matching `DataPlane`'s own test-only accessor.
    #[cfg(test)]
    pub(crate) fn buffered_len_for_test(&self, id: &SessionId) -> usize {
        lock(&self.entries)
            .get(id)
            .and_then(|entry| entry.data_plane.as_ref())
            .map(|data_plane| data_plane.buffered_len())
            .unwrap_or(0)
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

    /// Kills and confirms dead every session this host still owns a real process for - the
    /// host's own shutdown path. Before this method existed, dropping the last `TerminalPane`
    /// attached to a session dropped its `PtySession` too, whose `Drop` killed the child; now
    /// this table (and each session's own relay thread, which holds a clone of the same
    /// `SessionHandle`) is what actually keeps a `PtySession` alive, so nothing killed a live
    /// child just because the process that owned its pane quit - GitHub issue #530's own
    /// regression test caught exactly this once every spawn started going through the host.
    /// Sequential, one real, bounded `SessionHandle::shutdown` per session (`jerry-pty`'s own
    /// grace period, never a fresh timeout stacked on top) - a handful of open agents/shells is
    /// the real, expected case, not hundreds, so parallelizing this would add real complexity for
    /// no real win.
    pub fn shutdown_all(&self) {
        let sessions: Vec<(Arc<SessionHandle>, Option<Arc<DataPlane>>)> = lock(&self.entries)
            .values()
            .filter_map(|entry| {
                entry
                    .handle
                    .clone()
                    .map(|handle| (handle, entry.data_plane.clone()))
            })
            .collect();
        for (handle, data_plane) in sessions {
            if let Err(err) = handle.shutdown() {
                log::warn!(
                    "jerry-host: failed to shut down session {} during host shutdown: {err}",
                    handle.id()
                );
            }
            // The process is confirmed dead by the line above; nothing will ever attach to its
            // data plane again, so its socket goes with it rather than lingering as a stale file.
            if let Some(data_plane) = data_plane {
                data_plane.shutdown();
            }
        }
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
            process_id: None,
        };
        lock(&self.entries).insert(
            id,
            Entry {
                record,
                handle: None,
                data_plane: None,
            },
        );
    }

    pub(crate) fn forget_agent(&self, agent_id: &AgentId) {
        lock(&self.entries).remove(&agent_session_id(agent_id));
    }

    /// Removes `id`'s entry from this table entirely - unlike [`Self::record_exit`] (which keeps
    /// the record, `exit` set, so `SessionsQuery` can still report it once), this is the app
    /// telling this table it has no further interest in `id` at all. Without a caller ever doing
    /// this, every session this host ever spawned stayed in the table (dead `PtySession` and all)
    /// until the host itself exited - GitHub issue #530's own follow-up, once `Agents::close`
    /// already knew which real `SessionId` a closed tab's process was
    /// (`crate::work_surface::agents::Agent::host_session_id`).
    pub(crate) fn forget_session(&self, id: &SessionId) {
        // Torn down explicitly, not just left to `Arc<DataPlane>`'s own `Drop`: the session's
        // relay thread may still hold a clone briefly after this call (draining its last item),
        // and a stale socket file must not outlive the app's own stated "no further interest".
        if let Some(entry) = lock(&self.entries).remove(id) {
            if let Some(data_plane) = entry.data_plane {
                data_plane.shutdown();
            }
        }
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
    ///
    /// Drops this entry's own `Arc<SessionHandle>` (and so, once nothing else - a still-attached
    /// pane - holds one too, the real `PtySession` underneath it): the process is confirmed dead
    /// by definition of this being called at all, so there is nothing left for that handle to do,
    /// and holding it forever would keep a dead `PtySession` around for the record's own natural
    /// lifetime instead of just the exit status this call already captures. The record itself
    /// (`entry.record`, with `exit` now set) is left in the table so `SessionsQuery` can still
    /// report the exit once - [`Self::forget_session`] is the real removal.
    fn record_exit(&self, id: &SessionId, status: &jerry_pty::ExitStatus) {
        let wire = exit_status_wire(status);
        let recorded = {
            let mut entries = lock(&self.entries);
            match entries.get_mut(id) {
                Some(entry) => {
                    entry.record.exit = Some(wire.clone());
                    entry.handle = None;
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
/// `PtySession::take_output`'s own docs - but handled honestly rather than `unwrap`ped), a failure
/// to bind this session's own data-plane socket, or a failure to start the relay thread that
/// observes this session's exit.
#[derive(Debug, thiserror::Error)]
pub enum SessionSpawnError {
    #[error(transparent)]
    Pty(#[from] PtyError),
    #[error("a freshly spawned session had no output stream")]
    NoOutputStream,
    #[error(transparent)]
    DataPlane(#[from] DataPlaneBindError),
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
/// Exited-last contract (`docs/architecture/decisions.md` §8's amendment) - pushing every
/// `Bytes` chunk to `data_plane` (its own buffer-and-relay to whichever client is attached, or
/// none yet), and recording the session's exit on `manager` and `data_plane` the moment `Exited`
/// is observed, before forwarding it downstream. Ends the moment `Exited` is forwarded, or the
/// moment the downstream receiver (the attached client) is dropped, whichever comes first.
fn spawn_relay(
    id: SessionId,
    mut raw: futures_mpsc::Receiver<PtyOutput>,
    mut relay_tx: futures_mpsc::Sender<PtyOutput>,
    data_plane: Arc<DataPlane>,
    manager: SessionManager,
) -> io::Result<()> {
    thread::Builder::new()
        .name("jerry-host-session-relay".into())
        .spawn(move || {
            while let Some(item) = block_on(raw.next()) {
                match &item {
                    PtyOutput::Bytes(chunk) => data_plane.push(chunk),
                    PtyOutput::Exited(status) => {
                        manager.record_exit(&id, status);
                        data_plane.mark_exited();
                    }
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
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
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

    /// The real CI regression behind PR #538: every other test in this module reaches
    /// `sockets_dir` via `crate::default_sockets_dir()`, a directory some *other*, unrelated test
    /// in the same process may have already created as a side effect of its own
    /// `Registry::open` call - true by luck on Windows (many test helpers key off the same real
    /// `runtime_dir()`), never true on unix (every other helper here uses its own isolated
    /// `tempfile::TempDir` instead), so unix never got the accidental head start and every real
    /// spawn failed outright (`DataPlane::bind`'s `Listener::bind` cannot create a missing parent
    /// directory). A definitely-fresh, never-created directory makes the bug deterministic on
    /// every platform instead of dependent on unrelated test ordering.
    #[test]
    fn spawn_creates_its_own_sockets_directory_when_it_does_not_exist_yet() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let sockets_dir = temp.path().join("never-created-until-now");
        assert!(!sockets_dir.exists(), "sanity check: truly not created yet");
        let manager = SessionManager::new(Fanout::default(), sockets_dir);
        let (_id, handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options("echo hello"))
            .expect("spawn must succeed even when its own sockets directory does not exist yet");
        assert!(
            handle.process_id().is_some(),
            "a real spawn always has a pid"
        );
        let mut output = handle.take_output().expect("output stream");
        let (bytes, status) = drain_until_exit(&handle, &mut output, Duration::from_secs(10));
        assert!(
            String::from_utf8_lossy(&bytes).contains("hello"),
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(status.success(), "{status:?}");
    }

    #[test]
    fn a_second_spawn_in_the_same_worktree_gets_a_distinct_id() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
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
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
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
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
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
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
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

/// [`SessionManager::attach`] and the real per-session socket it hands back
/// (`crate::data_plane`, `docs/architecture/decisions.md` §24): a real interactive shell, driven
/// entirely over the socket - never `SessionHandle::write_input`/`take_output` directly, which
/// would prove nothing about the data plane actually reachable through `command/session-attach`.
#[cfg(test)]
mod data_plane_tests {
    use super::SessionManager;
    use crate::fanout::Fanout;
    use jerry_core::client::Stream;
    use jerry_pty::SpawnOptions;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::time::Duration;
    use test_support::wait_until;

    /// A real, interactive shell (no script, no `-c`/`/c`): reads commands from its stdin - here,
    /// the data-plane socket - for as long as it stays open, exactly like a real terminal tab.
    fn interactive_shell_options() -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd")
        } else {
            SpawnOptions::new("sh")
        }
    }

    #[cfg(windows)]
    const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
    #[cfg(windows)]
    const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

    /// ConPTY's startup Device Status Report query (see `session_manager_tests::
    /// answer_cursor_position_query`'s identical docs) - answered here over the raw socket
    /// itself, since a bare test client is exactly as VT-blind as a bare `SessionHandle` is.
    #[cfg(windows)]
    fn answer_cursor_position_query(stream: &mut Stream, seen: &[u8], answered: &mut bool) {
        if !*answered
            && seen
                .windows(CURSOR_POSITION_QUERY.len())
                .any(|window| window == CURSOR_POSITION_QUERY)
        {
            let _ = stream.write_all(CURSOR_POSITION_REPORT);
            *answered = true;
        }
    }
    #[cfg(not(windows))]
    fn answer_cursor_position_query(_stream: &mut Stream, _seen: &[u8], _answered: &mut bool) {}

    /// Writes `line` followed by a real line ending, as if a user had typed it and pressed enter.
    fn write_line(stream: &mut Stream, line: &str) {
        stream
            .write_all(format!("{line}\r\n").as_bytes())
            .expect("write a line to the data-plane socket");
    }

    /// Reads from `stream` (a short per-call timeout, so a stalled connection cannot hang this
    /// past `overall_timeout`) until `needle` appears in the accumulated bytes, answering the
    /// Windows cursor-position query along the way. Panics with what was actually seen if the
    /// deadline passes first - a real failure, not a silent false negative.
    fn read_until_contains(
        stream: &mut Stream,
        needle: &[u8],
        overall_timeout: Duration,
    ) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set a short per-read timeout");
        let mut collected = Vec::new();
        let mut answered = false;
        let deadline = std::time::Instant::now() + overall_timeout;
        let mut buf = [0u8; 4096];
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {:?} in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&collected)
            );
            match stream.read(&mut buf) {
                Ok(0) => panic!(
                    "the connection closed before {:?} ever appeared in {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&collected)
                ),
                Ok(n) => {
                    collected.extend_from_slice(&buf[..n]);
                    answer_cursor_position_query(stream, &collected, &mut answered);
                    if collected
                        .windows(needle.len())
                        .any(|window| window == needle)
                    {
                        return collected;
                    }
                }
                Err(ref error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(error) => panic!("read error: {error}"),
            }
        }
    }

    /// Whether `stream` has been closed from the other end - a real `Ok(0)`/error observed
    /// within the short per-call timeout [`read_until_contains`] also uses, not assumed.
    fn is_closed(stream: &mut Stream) -> bool {
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => true,
            Ok(_) => false,
            Err(ref error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                false
            }
            Err(_) => true,
        }
    }

    #[test]
    fn attach_over_the_real_socket_carries_bytes_both_ways_and_the_host_closes_it_on_exit() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let (id, _handle) = manager
            .spawn(PathBuf::from("/repo"), None, interactive_shell_options())
            .expect("spawn");

        let socket = manager.attach(&id).expect("attach");
        let mut stream = Stream::connect(&socket).expect("connect to the data-plane socket");
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set read timeout");

        // Real input, over the socket - never `SessionHandle::write_input` - is what makes the
        // shell produce this marker, proving the client-to-host direction is real.
        write_line(&mut stream, "echo jerry-data-plane-marker");
        read_until_contains(
            &mut stream,
            b"jerry-data-plane-marker",
            Duration::from_secs(15),
        );

        write_line(&mut stream, "exit");

        assert!(
            wait_until(Duration::from_secs(15), || is_closed(&mut stream)),
            "the host must close the data-plane socket once the session exits"
        );
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
    fn a_second_attach_is_denied_until_the_first_disconnects() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let (id, _handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options_for_test(sleep))
            .expect("spawn");

        let socket = manager.attach(&id).expect("first attach");
        let first = Stream::connect(&socket).expect("first connection");

        assert!(
            wait_until(Duration::from_secs(5), || manager.attach(&id).is_err()),
            "a second attach must be denied once a real connection is live"
        );
        assert!(matches!(
            manager.attach(&id),
            Err(super::SessionAttachError::AlreadyAttached(_))
        ));

        drop(first);
        assert!(
            wait_until(Duration::from_secs(5), || manager.attach(&id).is_ok()),
            "a second attach must succeed again once the first connection disconnects"
        );

        manager.kill(&id).expect("kill");
    }

    #[test]
    fn output_produced_before_any_client_attaches_is_still_delivered() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let (id, _handle) = manager
            .spawn(
                PathBuf::from("/repo"),
                None,
                shell_options_for_test("echo jerry-pre-attach-marker"),
            )
            .expect("spawn");

        // A real, deterministic wait on the pre-attach buffer actually holding something -
        // never a blind sleep - before this test's own client ever attaches. On Windows this is
        // ConPTY's own startup handshake bytes (its Device Status Report query among them, see
        // `answer_cursor_position_query`'s docs): those are pushed immediately, unlike the
        // `echo` command's own output, which - per `docs/architecture/decisions.md` §23 - ConPTY
        // withholds until something answers that query, which nothing does here on purpose.
        assert!(
            wait_until(Duration::from_secs(10), || manager
                .buffered_len_for_test(&id)
                > 0),
            "sanity check: the session must have produced some output before anyone attaches"
        );

        let socket = manager.attach(&id).expect("a late attach is still allowed");
        let mut stream = Stream::connect(&socket).expect("connect to the data-plane socket");
        // The very first bytes delivered are the buffer snapshot itself, so this needle is
        // whatever the buffer already held the moment this connection registered - real,
        // pre-attach output, not anything produced by this client's own connection.
        read_until_contains(&mut stream, pre_attach_needle(), Duration::from_secs(10));
    }

    /// What [`output_produced_before_any_client_attaches_is_still_delivered`] looks for - see
    /// that test's own docs for why this differs by platform.
    #[cfg(windows)]
    fn pre_attach_needle() -> &'static [u8] {
        CURSOR_POSITION_QUERY
    }
    #[cfg(not(windows))]
    fn pre_attach_needle() -> &'static [u8] {
        b"jerry-pre-attach-marker"
    }

    /// [`session_manager_tests::shell_options`]'s exact one-shot-script shape, duplicated here
    /// only because that helper lives in a sibling test module and this one needs its own
    /// interactive-vs-one-shot distinction to stay explicit at each call site.
    fn shell_options_for_test(script: &str) -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd").args(["/c", script])
        } else {
            SpawnOptions::new("sh").args(["-c", script])
        }
    }
}
