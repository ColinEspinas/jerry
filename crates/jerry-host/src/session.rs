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
    SessionSnapshot, SnapshotCell, SnapshotCellWidth,
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

/// How many lines of real scrollback history a [`jerry_core::SessionSnapshot`] carries
/// (`docs/architecture/decisions.md` §25) - bounded so a long-lived session's own retained
/// history never makes a reattach's own control-plane message unbounded.
const SNAPSHOT_SCROLLBACK_CAP_LINES: usize = 200;

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
    /// This session's own headless grid (`jerry_term::grid::TerminalGrid`, decisions.md §25) -
    /// `Some` exactly when `handle`/`data_plane` are. Fed the identical bytes `data_plane` is, by
    /// the same relay thread ([`spawn_relay`]), so [`SessionManager::attach`] can answer a real
    /// [`jerry_core::SessionSnapshot`] instead of a raw byte replay.
    grid: Option<Arc<Mutex<jerry_term::grid::TerminalGrid>>>,
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
        let (rows, cols) = (options.rows, options.cols);
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
            rows,
            cols,
        };
        let process = Arc::new(Mutex::new(session));
        let data_plane = DataPlane::bind(&self.sockets_dir, Arc::clone(&process))?;
        let grid = Arc::new(Mutex::new(jerry_term::grid::TerminalGrid::new(rows, cols)));
        let relay_process = Arc::clone(&process);
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
                grid: Some(Arc::clone(&grid)),
            },
        );
        spawn_relay(
            id.clone(),
            raw_output,
            relay_tx,
            data_plane,
            grid,
            relay_process,
            self.clone(),
        )
        .map_err(SessionSpawnError::Relay)?;
        Ok((id, handle))
    }

    /// [`crate::data_plane::DataPlane`]'s own socket path, plus a real
    /// [`jerry_core::SessionSnapshot`] of this session's own headless grid at this exact moment -
    /// `command/session-attach`'s real answer (`docs/architecture/decisions.md` §25). The byte
    /// stream on the returned socket continues from the snapshot point; there is no separate
    /// pre-attach byte buffer to replay (§24's own buffer is gone - a byte replay after a resize
    /// renders at the wrong size until the child reacts, which the snapshot sidesteps entirely).
    /// Checked against the socket layer's own `attached` flag as an earlier, friendlier rejection
    /// of the common case; the socket layer itself is what actually enforces "exactly one attach
    /// at a time" (`crate::data_plane`'s own module docs) against a race with this check.
    pub fn attach(
        &self,
        id: &SessionId,
    ) -> Result<(PathBuf, Option<u32>, SessionSnapshot), SessionAttachError> {
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
        // Structurally always `Some` alongside `data_plane` (both set together in `Self::spawn`,
        // never independently) - a graceful empty snapshot rather than a panic if that invariant
        // is ever violated, since this is a real answer over the wire, never a place to `expect`.
        let snapshot = entry
            .grid
            .as_ref()
            .map(|grid| snapshot_from_grid(&lock(grid)))
            .unwrap_or_else(|| SessionSnapshot {
                rows: 0,
                cols: 0,
                cursor: None,
                cells: Vec::new(),
                scrollback: Vec::new(),
            });
        // Armed under this same `entries` lock, right alongside the snapshot it pairs with - the
        // real fix for a real bug: `DataPlane::push` had nowhere to put a byte the relay thread
        // produced between this exact snapshot and the client's own, later, real connect (a
        // genuine gap - the control-plane round trip a caller needs before it can even attempt
        // that connect is not instantaneous). `serve_one` drains this arm's own queue into the
        // connection before anything else once it registers - see `DataPlane::arm_for_attach`'s
        // own docs for the bounded, "never serve a gapped stream" contract this keeps.
        data_plane.arm_for_attach();
        Ok((
            data_plane.socket_path().to_path_buf(),
            entry.record.process_id,
            snapshot,
        ))
    }

    /// [`crate::data_plane::DataPlane::pending_len`] for the session's own armed-but-not-yet-
    /// connected queue - `0` for an unknown id, matching that method's own contract for an unarmed
    /// data plane.
    #[cfg(test)]
    pub(crate) fn pending_len_for_test(&self, id: &SessionId) -> usize {
        lock(&self.entries)
            .get(id)
            .and_then(|entry| entry.data_plane.as_ref())
            .map(|data_plane| data_plane.pending_len())
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

    /// Resizes a session's real pty and records the new size on its own `SessionRecord`
    /// (`SessionsQuery`'s own real, observable proof a resize actually took effect - the one a
    /// socket-attached, out-of-process `TerminalPane` has to verify against, since it has no
    /// in-process `PtySession` to read the applied size back from directly). `NotOwned` for an
    /// `AgentTable`-compatibility registration, which has no process to resize.
    pub fn resize(&self, id: &SessionId, rows: u16, cols: u16) -> Result<(), SessionError> {
        self.process_for(id)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resize(rows, cols)?;
        if let Some(entry) = lock(&self.entries).get_mut(id) {
            entry.record.rows = rows;
            entry.record.cols = cols;
            // Keeps a reattach snapshot honest about the real pty size - without this, a client
            // that resized then detached and reattached would paint the *old* geometry until the
            // next byte arrived, exactly the artifact the snapshot exists to avoid (§25).
            if let Some(grid) = &entry.grid {
                lock(grid).resize(rows, cols);
            }
        }
        Ok(())
    }

    /// Kills a session's process tree. Non-blocking, like `jerry_pty::PtySession::kill` itself:
    /// the real exit status arrives asynchronously as an `Exited` item, observed by the same
    /// relay thread [`Self::spawn`] started, which publishes `event/session-exited` from it.
    /// Idempotent for a session that has already exited: `record_exit` clears [`Entry::handle`]
    /// the moment the real process is confirmed dead (there is nothing left to signal), but a
    /// caller racing its own cleanup against a real, fast exit - the common case for a short-lived
    /// one-shot script, real on unix - must not see that as a failure. Still a real `NotOwned`
    /// for an `AgentTable`-only registration, which never had a process to kill in the first
    /// place (`Entry::record.exit` distinguishes the two: `None` for that case, `Some` for an
    /// exited real session).
    pub fn kill(&self, id: &SessionId) -> Result<(), SessionError> {
        match self.process_for(id) {
            Ok(process) => process
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .kill()
                .map_err(SessionError::from),
            Err(SessionError::NotOwned(_)) if self.already_exited(id) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn already_exited(&self, id: &SessionId) -> bool {
        lock(&self.entries)
            .get(id)
            .is_some_and(|entry| entry.record.exit.is_some())
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
            agent: Some(SessionAgentInfo {
                kind,
                agent_id,
                grants: Vec::new(),
                parent: None,
            }),
            started_at: unix_now(),
            exit: None,
            process_id: None,
            rows: 0,
            cols: 0,
        };
        lock(&self.entries).insert(
            id,
            Entry {
                record,
                handle: None,
                data_plane: None,
                grid: None,
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

    /// The worktree a real session's own `SessionSpawn` associated `agent_id` with
    /// (`SessionSpawn::agent`) - `confine`'s own lookup, keyed by the same identity `jerry
    /// hook`/every other agent call carries. Prefers a still-live entry over an exited one so a
    /// dead session's stale record cannot keep confining an id a fresh spawn has already reused.
    pub(crate) fn worktree_of_agent(&self, agent_id: &AgentId) -> Option<PathBuf> {
        let entries = lock(&self.entries);
        let matches = || {
            entries.values().filter(|entry| {
                entry
                    .record
                    .agent
                    .as_ref()
                    .is_some_and(|agent| &agent.agent_id == agent_id)
            })
        };
        matches()
            .find(|entry| entry.record.exit.is_none())
            .or_else(|| matches().next())
            .map(|entry| entry.record.worktree.clone())
    }

    /// Whether `agent_id` already names a real, still-running session - [`SessionSpawn::agent`]'s
    /// own denial rule (decisions.md §24): two live sessions must never answer to the same agent
    /// identity, since `confine`'s lookup above would then have to guess which one a hook call
    /// meant.
    pub(crate) fn agent_is_live(&self, agent_id: &AgentId) -> bool {
        lock(&self.entries).values().any(|entry| {
            entry.record.exit.is_none()
                && entry
                    .record
                    .agent
                    .as_ref()
                    .is_some_and(|agent| &agent.agent_id == agent_id)
        })
    }

    /// The live session id `agent_id` is currently spawned as, if any - the real counterpart to
    /// [`Self::worktree_of_agent`], used to resolve an `AttentionRaise`/`SessionSend` caller's own
    /// session (`docs/architecture/decisions.md` §28). Same "prefer a still-live entry" rule as
    /// that method.
    pub(crate) fn session_id_for_agent(&self, agent_id: &AgentId) -> Option<SessionId> {
        let entries = lock(&self.entries);
        let matches = || {
            entries.values().filter(|entry| {
                entry
                    .record
                    .agent
                    .as_ref()
                    .is_some_and(|agent| &agent.agent_id == agent_id)
            })
        };
        matches()
            .find(|entry| entry.record.exit.is_none())
            .or_else(|| matches().next())
            .map(|entry| entry.record.id.clone())
    }

    /// `SessionAgentInfo::grants` for `agent_id`'s own live session, empty for none/not found -
    /// what the dispatcher's `permits` check consults for an agent caller whose own
    /// `Invocability::Denied` would otherwise refuse a call (`docs/architecture/decisions.md`
    /// §28).
    pub(crate) fn grants_for_agent(&self, agent_id: &AgentId) -> Vec<String> {
        lock(&self.entries)
            .values()
            .filter(|entry| entry.record.exit.is_none())
            .find_map(|entry| {
                entry
                    .record
                    .agent
                    .as_ref()
                    .filter(|agent| &agent.agent_id == agent_id)
                    .map(|agent| agent.grants.clone())
            })
            .unwrap_or_default()
    }

    /// `SessionAgentInfo::parent` for `agent_id`'s own live session - the orchestrator a `Stop`
    /// hook from it is forwarded to, if any (`docs/architecture/decisions.md` §28).
    pub(crate) fn parent_of_agent(&self, agent_id: &AgentId) -> Option<AgentId> {
        lock(&self.entries)
            .values()
            .filter(|entry| entry.record.exit.is_none())
            .find_map(|entry| {
                entry
                    .record
                    .agent
                    .as_ref()
                    .filter(|agent| &agent.agent_id == agent_id)
                    .and_then(|agent| agent.parent.clone())
            })
    }

    /// Writes to a live session's real pty input - `SessionSend`'s own execution
    /// (`docs/architecture/decisions.md` §28). `NotOwned` for an `AgentTable`-compatibility
    /// registration or an already-exited session (both have no process to write to), exactly like
    /// [`Self::resize`]/[`Self::kill`].
    pub fn write_input(&self, id: &SessionId, data: &[u8]) -> Result<(), SessionError> {
        self.process_for(id)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_input(data)
            .map_err(SessionError::from)
    }

    pub(crate) fn agent_count(&self) -> usize {
        lock(&self.entries)
            .values()
            .filter(|entry| entry.record.agent.is_some())
            .count()
    }

    /// Every still-live agent (`record.exit.is_none()`) this host is tracking, for `AgentsQuery` -
    /// unlike [`Self::list`] (`SessionsQuery`'s own answer), which still reports an exited
    /// session's record once. An exited real session's own `agent` association is never cleared,
    /// only its liveness - `SessionSpawn::agent`'s own doc comment covers why an agent id becomes
    /// reusable again the moment this stops counting it as live.
    pub(crate) fn agent_entries(&self) -> Vec<(AgentId, PathBuf, String)> {
        lock(&self.entries)
            .values()
            .filter(|entry| entry.record.exit.is_none())
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
///
/// Also answers `grid`'s own [`jerry_term::grid::TerminalGrid::take_pending_pty_writes`] back
/// into `process` whenever [`DataPlane::push`] reports a chunk as dropped (no client attached or
/// armed to ever see it) - a real ConPTY's own startup handshake sends a cursor-position query
/// (`ESC[6n`) and blocks its *entire* output stream on a real reply, so a session spawned before
/// any client has attached (the ordinary "guaranteed startup shell" case) would otherwise hang
/// forever with no output ever reaching anyone, real or headless. `crate::terminal::pane`'s own
/// client-side task in `jerry-app` answers the identical query once a client *is* attached (or is
/// racing to attach - `push`'s own `true` there means the client's own grid will see, and answer,
/// these same bytes itself); only `push` answering `false` here is the signal this thread must
/// step in instead, so a query is never answered twice.
fn spawn_relay(
    id: SessionId,
    mut raw: futures_mpsc::Receiver<PtyOutput>,
    mut relay_tx: futures_mpsc::Sender<PtyOutput>,
    data_plane: Arc<DataPlane>,
    grid: Arc<Mutex<jerry_term::grid::TerminalGrid>>,
    process: Arc<Mutex<PtySession>>,
    manager: SessionManager,
) -> io::Result<()> {
    thread::Builder::new()
        .name("jerry-host-session-relay".into())
        .spawn(move || {
            while let Some(item) = block_on(raw.next()) {
                match &item {
                    PtyOutput::Bytes(chunk) => {
                        let captured = data_plane.push(chunk);
                        let pending_writes = {
                            let mut grid = lock(&grid);
                            grid.append_bytes(chunk);
                            if captured {
                                Vec::new()
                            } else {
                                grid.take_pending_pty_writes()
                            }
                        };
                        if !pending_writes.is_empty() {
                            if let Err(err) = lock(&process).write_input(&pending_writes) {
                                log::warn!(
                                    "jerry-host: failed to answer a terminal query for \
                                     session {id} with no client attached yet: {err}"
                                );
                            }
                        }
                    }
                    PtyOutput::Exited(status) => {
                        manager.record_exit(&id, status);
                        data_plane.mark_exited();
                        lock(&grid).mark_ended();
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

/// A real [`SessionSnapshot`] of `grid`'s current state - [`SessionManager::attach`]'s own answer
/// for a reattaching client to paint immediately, before a single further byte off the socket
/// ever arrives (`docs/architecture/decisions.md` §25). Colors are resolved against the one
/// palette this app has: `jerry_term::grid::TerminalPalette::default()` is a literal transcription
/// of the app's real (and currently only) theme, not a placeholder - see that type's own docs.
fn snapshot_from_grid(grid: &jerry_term::grid::TerminalGrid) -> SessionSnapshot {
    let palette = jerry_term::grid::TerminalPalette::default();
    let (cols, rows) = grid.dimensions();
    SessionSnapshot {
        rows,
        cols,
        cursor: grid.cursor_position(),
        cells: convert_rows(grid.visible_rows_plain(&palette)),
        scrollback: convert_rows(grid.scrollback_tail(&palette, SNAPSHOT_SCROLLBACK_CAP_LINES)),
    }
}

fn convert_rows(rows: Vec<Vec<jerry_term::grid::GridCell>>) -> Vec<Vec<SnapshotCell>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(convert_cell).collect())
        .collect()
}

fn convert_cell(cell: jerry_term::grid::GridCell) -> SnapshotCell {
    SnapshotCell {
        c: cell.c,
        fg: cell.fg,
        bg: cell.bg,
        bold: cell.bold,
        italic: cell.italic,
        underline: cell.underline,
        strikethrough: cell.strikethrough,
        width: match cell.width {
            jerry_term::grid::CellWidth::Narrow => SnapshotCellWidth::Narrow,
            jerry_term::grid::CellWidth::Wide => SnapshotCellWidth::Wide,
            jerry_term::grid::CellWidth::Spacer => SnapshotCellWidth::Spacer,
        },
    }
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
    ///
    /// Adopts this test process into a kill-on-close job first (GitHub issue #534): this crate
    /// cannot depend on `jerry-app`, whose own job object backstops issue #482, so every fixture
    /// here that spawns a real process needs this call to survive a nextest `TerminateProcess`
    /// on timeout the same way.
    fn shell_options(script: &str) -> SpawnOptions {
        test_support::adopt_this_process();
        if cfg!(windows) {
            SpawnOptions::new("cmd").args(["/c", script])
        } else {
            SpawnOptions::new("sh").args(["-c", script])
        }
    }

    /// Stays alive and lets whatever is written to the pty come back out of it - `cat` on unix,
    /// and on Windows the shell itself, since ConPTY does the echoing rather than the child
    /// (mirrors `jerry_pty`'s own `stdin_echoing_command` test helper).
    fn stdin_echoing_command() -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd.exe")
        } else {
            SpawnOptions::new("cat")
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

    #[test]
    fn session_id_grants_and_parent_are_resolved_from_the_spawning_agents_own_live_session() {
        use jerry_core::{AgentId, SessionAgentInfo};

        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let agent_id = AgentId::from("agent-orchestrator");
        let (id, _handle) = manager
            .spawn(
                PathBuf::from("/repo"),
                Some(SessionAgentInfo {
                    kind: "Claude".into(),
                    agent_id: agent_id.clone(),
                    grants: vec!["command/session-send".into()],
                    parent: Some(AgentId::from("agent-parent")),
                }),
                shell_options("echo hello"),
            )
            .expect("spawn");

        assert_eq!(manager.session_id_for_agent(&agent_id), Some(id));
        assert_eq!(
            manager.grants_for_agent(&agent_id),
            vec!["command/session-send".to_owned()]
        );
        assert_eq!(
            manager.parent_of_agent(&agent_id),
            Some(AgentId::from("agent-parent"))
        );

        let stranger = AgentId::from("nobody-spawned-this");
        assert_eq!(manager.session_id_for_agent(&stranger), None);
        assert!(manager.grants_for_agent(&stranger).is_empty());
        assert_eq!(manager.parent_of_agent(&stranger), None);
    }

    #[test]
    fn write_input_reaches_the_real_process_and_an_unknown_target_is_refused() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let (id, handle) = manager
            .spawn(PathBuf::from("/repo"), None, stdin_echoing_command())
            .expect("spawn");
        let mut output = handle.take_output().expect("output stream");

        manager
            .write_input(&id, b"echo-me-back\r\n")
            .expect("write to a real, live session");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        let mut answered = false;
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the write must reach the real process and come back out - either the child's \
                 own echo (unix `cat`) or ConPTY's own local echo (Windows `cmd.exe`); saw {:?}",
                String::from_utf8_lossy(&seen)
            );
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match recv_timeout(&mut output, remaining) {
                Some(PtyOutput::Bytes(chunk)) => {
                    answer_cursor_position_query(&handle, &chunk, &mut answered);
                    seen.extend_from_slice(&chunk);
                    if String::from_utf8_lossy(&seen).contains("echo-me-back") {
                        break;
                    }
                }
                Some(PtyOutput::Exited(status)) => {
                    panic!("the still-alive echoing session exited unexpectedly: {status:?}")
                }
                None => panic!(
                    "the output stream produced nothing further: {:?}",
                    String::from_utf8_lossy(&seen)
                ),
            }
        }
        manager
            .kill(&id)
            .expect("kill the still-alive echoing session so this test does not leak it");

        let unknown = jerry_core::SessionId::from("no-such-session");
        assert!(matches!(
            manager.write_input(&unknown, b"x"),
            Err(super::SessionError::NotFound(_))
        ));
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
        // Short on purpose: macOS's own `$TMPDIR` is already ~49 bytes
        // (`/var/folders/<2>/<random>/T/`) before `tempfile::TempDir`'s own random suffix, and
        // this directory name is test-only overhead a real caller never pays (production sockets
        // live directly under `sockets_dir`, never a nested subdirectory this test invents) - see
        // `every_platforms_runtime_dir_leaves_real_margin_for_a_data_plane_socket_too` for the
        // real production-shape check.
        let sockets_dir = temp.path().join("nc");
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

    /// A real, fast-exiting one-shot session, killed only after it has genuinely already exited -
    /// the exact race a short-lived child wins on unix (a `sh -c "echo ..."` regularly beats the
    /// caller's own cleanup to the punch). `record_exit` clears `Entry::handle` the moment the
    /// real process is confirmed dead, so a naive `kill` would answer `NotOwned` here - a real
    /// bug this test pins, distinct from [`an_agent_table_registration_is_listed_but_cannot_be_resized_or_killed`]'s
    /// own `NotOwned`, which is a registration that never had a process at all.
    #[test]
    fn killing_a_session_that_already_exited_is_not_an_error() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let (id, handle) = manager
            .spawn(
                PathBuf::from("/repo"),
                None,
                shell_options("echo already-gone"),
            )
            .expect("spawn");
        let mut output = handle.take_output().expect("output stream");
        let (_bytes, status) = drain_until_exit(&handle, &mut output, Duration::from_secs(10));
        assert!(
            status.success(),
            "sanity check: a plain echo exits clean: {status:?}"
        );

        let result = manager.kill(&id);
        assert!(
            result.is_ok(),
            "killing a session that already exited must be a real no-op, not an error: {result:?}"
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

    /// `docs/architecture/decisions.md` §25's own reattach snapshot, proven against a real
    /// spawn: content the child produced before anyone ever attached still shows up in
    /// [`SessionManager::attach`]'s own `SessionSnapshot`, since the headless grid
    /// ([`spawn_relay`], `super::snapshot_from_grid`) is fed the same bytes the data-plane socket
    /// is - not a raw byte buffer replayed over that socket, which issue #506's own §24 buffer
    /// this replaces never guaranteed correctly sized output after a resize (this type's own
    /// docs). `manager.attach` never marks the socket as attached by itself (only a real
    /// `Stream::connect` does, in `crate::data_plane::serve_one`), so polling it here is safe.
    #[test]
    fn attach_answers_a_real_snapshot_reflecting_output_produced_before_anyone_attaches() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let (id, handle) = manager
            .spawn(
                PathBuf::from("/repo"),
                None,
                shell_options("echo jerry-snapshot-marker"),
            )
            .expect("spawn");
        let mut output = handle.take_output().expect("output stream");

        let mut seen = Vec::new();
        let mut answered = false;
        let mut last_snapshot_text = String::new();
        let found = wait_until(Duration::from_secs(10), || {
            if let Some(PtyOutput::Bytes(chunk)) =
                recv_timeout(&mut output, Duration::from_millis(200))
            {
                seen.extend_from_slice(&chunk);
                answer_cursor_position_query(&handle, &seen, &mut answered);
            }
            let (_socket, _process_id, snapshot) = manager
                .attach(&id)
                .expect("attach must succeed while nobody has really connected yet");
            last_snapshot_text = snapshot_text(&snapshot);
            last_snapshot_text.contains("jerry-snapshot-marker")
        });
        assert!(
            found,
            "the reattach snapshot must reflect real output produced before any client ever \
             attached: last snapshot text {last_snapshot_text:?}, last raw bytes seen {:?}",
            String::from_utf8_lossy(&seen)
        );

        manager.kill(&id).expect("kill");
    }

    /// `docs/architecture/decisions.md` §25: "reattach after resize = `SessionResize` then
    /// attach, snapshot rendered at the new size, never raw replay." A resize that lands between
    /// two attaches of the same still-live session must be reflected the *second* time, not just
    /// on the next byte the child happens to print.
    #[test]
    fn attach_after_a_resize_renders_the_snapshot_at_the_new_size() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let (id, _handle) = manager
            .spawn(PathBuf::from("/repo"), None, shell_options(sleep))
            .expect("spawn");

        let (_socket, _process_id, before) = manager.attach(&id).expect("attach before any resize");
        assert_eq!(
            (before.rows, before.cols),
            (24, 80),
            "sanity check: SpawnOptions::new's own default size"
        );

        manager.resize(&id, 40, 120).expect("resize");
        let (_socket, _process_id, after) = manager.attach(&id).expect("attach after resize");
        assert_eq!(
            (after.rows, after.cols),
            (40, 120),
            "a reattach after a resize must render the snapshot at the new size, never the size \
             the session was originally spawned at"
        );
        assert_eq!(
            after.cells.len(),
            40,
            "the cells themselves must actually be reshaped, not just the reported dimensions"
        );
        assert_eq!(after.cells[0].len(), 120);

        manager.kill(&id).expect("kill");
    }

    /// Every visible-then-scrollback cell's own character, concatenated in reading order - enough
    /// to search for a marker string without the test caring exactly which row it landed on.
    fn snapshot_text(snapshot: &jerry_core::SessionSnapshot) -> String {
        snapshot
            .cells
            .iter()
            .chain(snapshot.scrollback.iter())
            .flat_map(|row| row.iter().map(|cell| cell.c))
            .collect()
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

        let (socket, _process_id, _snapshot) = manager.attach(&id).expect("attach");
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

    /// A real bug this test pins: `SessionManager::attach` captures a real snapshot, but the
    /// control-plane round trip a caller needs before it can even attempt the real data-plane
    /// connect that follows is not instantaneous - real output the relay thread pushes in that
    /// gap used to have nowhere to land (`DataPlane::push` dropped it outright, since nobody was
    /// connected and the old pre-attach buffer §25 removed no longer existed to catch it either).
    /// `DataPlane::arm_for_attach` closes it: this drives real input through the session's own
    /// in-process handle (standing in for the caller's real, slower round trip) *between*
    /// `attach` and the real `Stream::connect` that follows it, and proves the resulting bytes
    /// still reach the client - through the armed pending queue, not a raw replay.
    #[test]
    fn output_produced_between_attach_and_connect_is_not_lost() {
        let manager = SessionManager::new(Fanout::default(), crate::default_sockets_dir());
        let (id, handle) = manager
            .spawn(PathBuf::from("/repo"), None, interactive_shell_options())
            .expect("spawn");

        let (socket, _process_id, _snapshot) =
            manager.attach(&id).expect("attach arms the data plane");

        // Real output produced strictly *after* this attach's own snapshot and strictly *before*
        // anything connects - exactly the gap `DataPlane::arm_for_attach`'s own queue exists for.
        handle
            .write_input(b"echo jerry-gap-marker\r\n")
            .expect("write to the real pty");
        assert!(
            wait_until(Duration::from_secs(10), || manager
                .pending_len_for_test(&id)
                > 0),
            "sanity check: the relay thread must have pushed something into the armed pending \
             queue before this test ever connects"
        );

        let mut stream = Stream::connect(&socket).expect("connect to the data-plane socket");
        let seen = read_until_contains(&mut stream, b"jerry-gap-marker", Duration::from_secs(15));
        assert!(
            String::from_utf8_lossy(&seen).contains("jerry-gap-marker"),
            "output produced between attach and connect must still reach the client: {:?}",
            String::from_utf8_lossy(&seen)
        );

        manager.kill(&id).expect("kill");
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

        let (socket, _process_id, _snapshot) = manager.attach(&id).expect("first attach");
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
