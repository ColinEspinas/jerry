//! The session host: the one place a `Call` is authorized and executed. Owns the dispatch
//! thread, the socket listener, the session table (`crate::session`, every PTY it spawned or is
//! tracking) and the notification fan-out. Every task here wakes on a channel, never on a timer.
//! The hook store is not here yet; a request needing it is answered `NEEDS_HOST`. Zero `gpui`.

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod data_plane;
mod dispatch;
mod fanout;
mod listener;
mod session;

pub use session::{SessionError, SessionHandle, SessionManager, SessionSpawnError};

use futures::channel::{mpsc, oneshot};
use jerry_core::client::Stream;
use jerry_core::wire::rpc_code;
use jerry_core::{AgentId, Call, Message, Report, RpcError, SessionId, StopDecision};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The dispatch loop, to run on whichever executor the embedding process provides.
pub type DispatchFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("could not spawn the {name} thread: {source}")]
    Thread {
        name: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("could not listen on {}: {source}", path.display())]
    Bind {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("already listening on {}", path.display())]
    AlreadyListening { path: PathBuf },
    #[error(transparent)]
    SocketDirectory(#[from] jerry_core::registry::RegistryError),
}

/// One agent this host is tracking: the worktree it's confined to, and which CLI it runs -
/// `kind` is a free-form label (`jerry-app`'s own `AgentKind::label()`, e.g. `"Claude"`), never
/// interpreted here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub worktree: PathBuf,
    pub kind: String,
}

/// An agent identity registered with this host without a `SessionSpawn` of its own - the
/// dispatcher classifies callers against it and answers `AgentsQuery` from it exactly like a real
/// session's own `SessionSpawn::agent` association (`SessionManager::worktree_of_agent`/
/// `agent_entries` read both the same way). `jerry-app`'s own `Agents` no longer calls this for a
/// session it spawns itself - `SessionSpawn::agent` makes that association atomically instead -
/// but this stays the real path for an agent identity that exists without a session this host
/// owns a process for (a hook-only test double, or a future non-PTY agent kind).
///
/// A thin view over [`SessionManager`] (`docs/architecture/decisions.md` §23) - kept as its own
/// type, rather than every caller reaching for `SessionManager` directly, only for its narrower
/// public shape (`register`/`forget`/`worktree_of`/`list`). There is exactly one underlying
/// table: constructing one from scratch is not offered - see [`SessionManager::agent_table`].
#[derive(Clone)]
pub struct AgentTable(SessionManager);

impl AgentTable {
    pub fn register(&self, id: AgentId, worktree: PathBuf, kind: String) {
        self.0.register_agent(id, worktree, kind);
    }

    pub fn forget(&self, id: &AgentId) {
        self.0.forget_agent(id);
    }

    /// [`SessionManager::forget_session`] - the real `SessionId` a spawn minted, distinct from
    /// [`Self::forget`]'s synthetic `agent:<id>` key.
    pub fn forget_session(&self, id: &SessionId) {
        self.0.forget_session(id);
    }

    pub fn worktree_of(&self, id: &AgentId) -> Option<PathBuf> {
        self.0.worktree_of_agent(id)
    }

    pub fn len(&self) -> usize {
        self.0.agent_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every agent this host is tracking right now, for `AgentsQuery` - the dispatcher's own
    /// answer for a request `execute_locally` can never resolve on its own (§15).
    pub fn list(&self) -> Vec<(AgentId, AgentRecord)> {
        self.0
            .agent_entries()
            .into_iter()
            .map(|(id, worktree, kind)| (id, AgentRecord { worktree, kind }))
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One call waiting for the dispatcher, and where its answer goes.
struct Job {
    call: Call,
    reply: oneshot::Sender<Result<Value, RpcError>>,
}

pub(crate) struct Inner {
    jobs: Mutex<Option<mpsc::UnboundedSender<Job>>>,
    agents: AgentTable,
    sessions: SessionManager,
    fanout: fanout::Fanout,
    /// One handle per live socket connection, so shutdown can close them under their threads.
    connections: Mutex<Vec<Stream>>,
    shutting_down: AtomicBool,
    /// [`Host::run_lifecycle`]'s own wake sender - always present (paired with `Host::
    /// shutdown_rx` at construction, before anything could possibly connect and race it), so
    /// `Self::request_shutdown` from the `Shutdown` command never has a window where it is a
    /// silent no-op. Harmless when no lifecycle loop is running at all
    /// (`HostRuntime::in_process`/test hosts never start one): the bounded channel just buffers
    /// the one wake with nothing reading it.
    shutdown_tx: std_mpsc::SyncSender<()>,
    /// Pending `SessionStopPolicy` decisions, keyed by the child agent id the *next* `Stop` hook
    /// from them should honour (`docs/architecture/decisions.md` §26) - consumed once
    /// ([`Self::take_stop_policy`]). Registrations are rare (one per orchestrator-managed stop),
    /// never a hot path, so a plain mutex-guarded map is enough.
    stop_policies: Mutex<HashMap<AgentId, (StopDecision, Option<String>)>>,
}

impl Inner {
    /// Hands a call to the dispatcher. After shutdown the receiver resolves at once with
    /// `SHUTTING_DOWN` instead of hanging.
    pub(crate) fn submit(&self, call: Call) -> oneshot::Receiver<Result<Value, RpcError>> {
        let (reply, receiver) = oneshot::channel();
        let sent = match &*lock(&self.jobs) {
            Some(jobs) => jobs.unbounded_send(Job { call, reply }).is_ok(),
            None => false,
        };
        if !sent {
            let (reply, receiver) = oneshot::channel();
            // A oneshot's receiver cannot have gone away between these two lines.
            let _ = reply.send(Err(shutting_down()));
            return receiver;
        }
        receiver
    }

    pub(crate) fn agents(&self) -> &AgentTable {
        &self.agents
    }

    pub(crate) fn sessions(&self) -> &SessionManager {
        &self.sessions
    }

    pub(crate) fn fanout(&self) -> &fanout::Fanout {
        &self.fanout
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// No live sessions and no subscribed client (`docs/architecture/decisions.md` §24) -
    /// [`Host::run_lifecycle`]'s own idle check. A socket connection that never sent `event/
    /// subscribe` (an ordinary request/response call) does not count.
    pub(crate) fn is_idle(&self) -> bool {
        self.sessions.list().is_empty() && self.fanout.sink_count() == 0
    }

    /// Wakes [`Host::run_lifecycle`] immediately, regardless of idleness - the `Shutdown`
    /// command's own effect. Buffered harmlessly if no lifecycle loop is running yet or at all
    /// (`Self::shutdown_tx`'s own docs).
    pub(crate) fn request_shutdown(&self) {
        let _ = self.shutdown_tx.try_send(());
    }

    pub(crate) fn track_connection(&self, stream: Stream) {
        lock(&self.connections).push(stream);
    }

    /// `SessionStopPolicy`'s own write side (`docs/architecture/decisions.md` §26): registers, or
    /// replaces, the decision `child`'s next `Stop` hook should answer with.
    pub(crate) fn register_stop_policy(
        &self,
        child: AgentId,
        decision: StopDecision,
        reason: Option<String>,
    ) {
        lock(&self.stop_policies).insert(child, (decision, reason));
    }

    /// Consumes and returns the pending policy for `child`, if any - a registered policy applies
    /// to exactly one `Stop` (`docs/architecture/decisions.md` §26).
    pub(crate) fn take_stop_policy(
        &self,
        child: &AgentId,
    ) -> Option<(StopDecision, Option<String>)> {
        lock(&self.stop_policies).remove(child)
    }

    /// Closes every live connection's socket, which ends its reader and, through the fanout,
    /// its writer. Never blocks.
    fn close_connections(&self) {
        for stream in lock(&self.connections).drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.fanout.clear();
    }
}

fn shutting_down() -> RpcError {
    RpcError::new(rpc_code::SHUTTING_DOWN, "the host is shutting down")
}

/// The in-process handle. Dropping it shuts the host down without waiting for its threads.
pub struct Host {
    inner: Arc<Inner>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    listening: Mutex<Option<listener::Listening>>,
    /// [`Self::run_lifecycle`]'s own receiver, paired with `Inner::shutdown_tx` at construction -
    /// `Some` until the first (and only meaningful) call takes it.
    shutdown_rx: Mutex<Option<std_mpsc::Receiver<()>>>,
}

impl Host {
    /// Starts the host with its own dispatch thread, for a process that has no executor of
    /// its own (the standalone host binary, tests). Every session's data-plane socket
    /// (`crate::data_plane`) binds under [`default_sockets_dir`]; [`Self::start_at`] is the
    /// explicit-directory twin production callers that already track a registry directory use.
    pub fn start() -> Result<Host, HostError> {
        Host::start_at(default_sockets_dir())
    }

    /// [`Self::start`], binding every session's data-plane socket under `sockets_dir` instead of
    /// [`default_sockets_dir`] - what `jerry-host`'s own `main` and `jerry-app`'s `HostRuntime`
    /// call, since both already resolve the exact registry directory their descriptor lives
    /// under and a session's socket belongs alongside it, not scattered to a second default.
    pub fn start_at(sockets_dir: PathBuf) -> Result<Host, HostError> {
        let (host, dispatch) = Host::start_detached(sockets_dir);
        let dispatcher = thread::Builder::new()
            .name("jerry-host-dispatch".into())
            .spawn(move || futures::executor::block_on(dispatch))
            .map_err(|source| HostError::Thread {
                name: "dispatch",
                source,
            })?;
        *lock(&host.dispatcher) = Some(dispatcher);
        Ok(host)
    }

    /// Starts the host with its dispatch loop handed to `spawn`, so an embedding process runs
    /// it on its own executor: the app spawns it on GPUI's background executor, and a GPUI
    /// test drives it deterministically. Blocking git work runs wherever `spawn` puts it.
    pub fn start_with(spawn: impl FnOnce(DispatchFuture)) -> Host {
        let (host, dispatch) = Host::start_detached(default_sockets_dir());
        spawn(dispatch);
        host
    }

    /// Builds the host and hands back its dispatch loop for the caller to spawn wherever it
    /// lives: the app puts it on GPUI's background executor, and a GPUI test drives it
    /// deterministically. Blocking git work runs wherever the loop runs. Every session's
    /// data-plane socket (`crate::data_plane`) binds under `sockets_dir` - production callers
    /// pass the same directory their registry descriptor lives under; a throwaway test host that
    /// has none in scope uses [`default_sockets_dir`] via [`Self::start`]/[`Self::start_with`].
    pub fn start_detached(sockets_dir: PathBuf) -> (Host, DispatchFuture) {
        let (jobs, mut receiver) = mpsc::unbounded::<Job>();
        let fanout = fanout::Fanout::default();
        let sessions = SessionManager::new(fanout.clone(), sockets_dir);
        let agents = sessions.agent_table();
        // Paired eagerly, here, rather than when `Self::run_lifecycle` is first called: a
        // `Shutdown` command dispatched the instant this host starts accepting connections must
        // never race an as-yet-unset sender (see `Inner::shutdown_tx`'s own docs).
        let (shutdown_tx, shutdown_rx) = std_mpsc::sync_channel(1);
        let inner = Arc::new(Inner {
            jobs: Mutex::new(Some(jobs)),
            agents,
            sessions,
            fanout,
            connections: Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
            shutdown_tx,
            stop_policies: Mutex::new(HashMap::new()),
        });
        let worker = Arc::clone(&inner);
        let dispatch: DispatchFuture = Box::pin(async move {
            use futures::StreamExt;
            while let Some(job) = receiver.next().await {
                let result = if worker.is_shutting_down() {
                    Err(shutting_down())
                } else {
                    dispatch::handle(&worker, job.call)
                };
                // The caller may have given up waiting; nothing to do about that here.
                let _ = job.reply.send(result);
            }
        });
        let host = Host {
            inner,
            dispatcher: Mutex::new(None),
            listening: Mutex::new(None),
            shutdown_rx: Mutex::new(Some(shutdown_rx)),
        };
        (host, dispatch)
    }

    pub fn agents(&self) -> AgentTable {
        self.inner.agents.clone()
    }

    /// The session table (`docs/architecture/decisions.md` §23): the same one `SessionSpawn`/
    /// `SessionResize`/`SessionKill`/`SessionsQuery` act on. An in-process caller attaches to a
    /// spawned session's data-plane adapter through [`SessionManager::handle_for`] here, never
    /// through `Call`/`Report` - see that method's own docs.
    pub fn sessions(&self) -> SessionManager {
        self.inner.sessions.clone()
    }

    pub fn client(&self) -> LocalClient {
        LocalClient {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Accepts socket clients at `socket`, first ensuring its parent directory exists (the same
    /// `ensure_private_dir` `DataPlane::bind` already calls for a per-session socket - every
    /// caller, `main.rs`'s production bind and every test's in-process one alike, needs this, not
    /// just the data plane's own). A caller that binds under a directory nothing has created yet
    /// (a fresh CI runner's own `$TMPDIR`, before anything else has touched it) used to fail here
    /// with a raw `NotFound`, invisible as anything but "every Command in this test errors". The
    /// registry descriptor should be published only after this returns, so a discoverable entry
    /// always has a listener behind it.
    pub fn listen(&self, socket: &Path) -> Result<(), HostError> {
        let mut listening = lock(&self.listening);
        if let Some(existing) = &*listening {
            return Err(HostError::AlreadyListening {
                path: existing.socket().to_path_buf(),
            });
        }
        if let Some(parent) = socket.parent() {
            jerry_core::registry::ensure_private_dir(parent)?;
        }
        *listening = Some(listener::listen(Arc::clone(&self.inner), socket)?);
        Ok(())
    }

    /// Stops the host without waiting for its threads: queued and future calls answer
    /// `SHUTTING_DOWN`, the socket file is removed, every connection is closed, and the accept
    /// and dispatch threads exit on their own. Safe on a UI thread. Idempotent.
    pub fn shutdown(&self) -> ShutdownHandles {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        // Wakes `Self::run_lifecycle` immediately if one happens to be running concurrently -
        // the same effect `Inner::request_shutdown` (the `Shutdown` command) has, so a caller
        // that tears this host down directly gets the same guarantee `Self::run_lifecycle`'s own
        // docs promise for every path, not just that one command.
        self.inner.request_shutdown();
        lock(&self.inner.jobs).take();
        // Before the fanout/listener teardown below, and before `lock(&self.dispatcher).take()`
        // orphans the dispatch loop that would otherwise still be able to answer a `SessionKill`:
        // a `TerminalPane` dropping no longer kills its own session's real process (§23's Part
        // B moved that process into this table instead), so nothing else does either unless this
        // does it here. Real, bounded per-session kills - each session's own relay thread is
        // still alive to observe and broadcast its `event/session-exited`, which a live fanout
        // is what lets any local subscriber still see.
        self.inner.sessions().shutdown_all();
        let accept = lock(&self.listening)
            .as_mut()
            .and_then(listener::Listening::stop);
        self.inner.close_connections();
        let dispatcher = lock(&self.dispatcher).take();
        ShutdownHandles { accept, dispatcher }
    }

    /// `shutdown`, then waits for the accept and dispatch threads. For tests and for a cleanup
    /// thread; never for a UI thread.
    pub fn shutdown_and_join(&self) {
        self.shutdown().join();
    }

    /// Blocks until this host should exit: no live sessions and no subscribed client
    /// (`Inner::is_idle`) for `config.linger`, or an explicit wake - the `Shutdown` command
    /// (`Inner::request_shutdown`) or `jerry host stop`. Never called by an in-process/test host
    /// (`docs/architecture/decisions.md` §24); only the standalone `jerry-host` binary's `main`
    /// runs this, then calls [`Self::shutdown_and_join`] once it returns. A no-op on a second
    /// call (`Self::shutdown_rx` already taken) - there is nothing left to lifecycle-manage.
    ///
    /// The one real timer in this crate (§16): idleness is inherently about elapsed time, so
    /// `config.poll_interval` bounds a channel wait rather than this looping on its own. An
    /// explicit wake breaks immediately, regardless of idleness - `Self::shutdown` already kills
    /// every live session the same way an idle timeout would.
    pub fn run_lifecycle(&self, config: LifecycleConfig) {
        let Some(rx) = lock(&self.shutdown_rx).take() else {
            return;
        };
        let mut idle_since: Option<Instant> = None;
        loop {
            match rx.recv_timeout(config.poll_interval) {
                Ok(()) => break,
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            }
            if self.inner.is_idle() {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= config.linger {
                    break;
                }
            } else {
                idle_since = None;
            }
        }
    }
}

/// Where a session's data-plane socket (`crate::data_plane`) binds when nothing more specific is
/// in scope (every `Host::start`/`Host::start_with` test call site, and any other throwaway
/// host): the same real runtime directory the registry itself resolves, falling back to a
/// plain temp subdirectory only if that lookup fails - an unset `$XDG_RUNTIME_DIR`/
/// `%LOCALAPPDATA%`, see `jerry_core::registry::runtime_dir`'s own docs for when that happens.
/// Matches this workspace's existing test convention of binding real, short-lived sockets under
/// the real runtime directory on platforms where a nested tempdir would not fit
/// `MAX_SOCKET_PATH_BYTES` (see e.g. `crate::listener::socket_tests::socket_path`).
pub fn default_sockets_dir() -> PathBuf {
    jerry_core::registry::runtime_dir().unwrap_or_else(|_| std::env::temp_dir().join("jerry-run"))
}

/// How long an idle [`Host`] lingers before [`Host::run_lifecycle`] lets it exit - long enough
/// for an app relaunch or a CLI call to reattach without a fresh spawn, short enough that a
/// genuinely abandoned host does not sit in the registry indefinitely.
pub const LIFECYCLE_LINGER: Duration = Duration::from_secs(5);

/// [`Host::run_lifecycle`]'s own tuning - a struct rather than two bare parameters so a caller
/// only names what it overrides. `Default` is the real production shape; a test shortens both to
/// run in milliseconds instead of seconds.
#[derive(Debug, Clone, Copy)]
pub struct LifecycleConfig {
    /// How often an idle host rechecks - not itself the linger; several polls fit inside one
    /// linger window so idleness is measured close to `linger`, not `linger + poll_interval`.
    pub poll_interval: Duration,
    pub linger: Duration,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        LifecycleConfig {
            poll_interval: Duration::from_millis(500),
            linger: LIFECYCLE_LINGER,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // The threads finish on their own; a drop must never block.
        let _ = self.shutdown();
    }
}

/// The threads `Host::shutdown` left to finish on their own; `join` waits for them.
#[must_use = "drop it to let the threads finish on their own, or join it to wait"]
pub struct ShutdownHandles {
    accept: Option<JoinHandle<()>>,
    dispatcher: Option<JoinHandle<()>>,
}

impl ShutdownHandles {
    pub fn join(self) {
        for handle in [self.accept, self.dispatcher].into_iter().flatten() {
            let _ = handle.join();
        }
    }
}

/// The app's own client: the same dispatch path a socket client takes, minus the socket.
#[derive(Clone)]
pub struct LocalClient {
    inner: Arc<Inner>,
}

impl LocalClient {
    pub async fn call(&self, call: Call) -> Result<Value, RpcError> {
        // Checked before `call` moves into `submit` below - `dispatch::is_shutdown`'s own docs
        // cover why the wake fires here, once this in-process caller has actually received its
        // result, rather than inside `dispatch::handle` itself.
        let is_shutdown = dispatch::is_shutdown(&call);
        let result = match self.inner.submit(call).await {
            Ok(result) => result,
            Err(_cancelled) => Err(RpcError::new(
                rpc_code::SHUTTING_DOWN,
                "the host dropped the request",
            )),
        };
        if is_shutdown && result.is_ok() {
            self.inner.request_shutdown();
        }
        result
    }

    pub async fn request(&self, call: Call) -> Result<Report, RpcError> {
        let value = self.call(call).await?;
        serde_json::from_value(value).map_err(|error| {
            RpcError::new(
                rpc_code::INTERNAL_ERROR,
                format!("the host's result is not a Report: {error}"),
            )
        })
    }

    /// Every notification the host emits from now on, to await in a channel-woken task.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Message> {
        self.inner.fanout.subscribe_local()
    }
}

#[cfg(test)]
mod listen_tests {
    use super::Host;
    use jerry_core::client::Client;
    use jerry_core::{AppQuery, Call, Request};
    use std::time::Duration;

    /// The regression this exists to make impossible again: unlike `DataPlane::bind`'s per-session
    /// sockets (`session::session_manager_tests::spawn_creates_its_own_sockets_directory_when_it_
    /// does_not_exist_yet`), `Host::listen`'s own control-plane bind had no directory-creation step
    /// at all - a fresh runner whose socket directory nothing had created yet (a `#[cfg(test)]`
    /// in-process `RepoHost` on a CI worker with no shared runtime dir left over from an earlier
    /// test, unlike this machine's own) failed the bind outright, so every `Call` a test then
    /// dispatched against that host answered a connection error - indistinguishable, from the
    /// dispatching side, from "nothing downstream ran" (the real Linux/macOS CI regression this
    /// test would have caught before it shipped). A definitely-fresh, never-created directory
    /// makes the bug deterministic on every platform, matching the data-plane test's own reasoning
    /// for why it does not rely on unrelated test ordering.
    #[test]
    fn listen_creates_its_own_socket_directory_when_it_does_not_exist_yet() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        // Short on purpose - the same margin-for-macOS's-own-long-$TMPDIR reasoning as the
        // data-plane test this mirrors.
        let socket = temp.path().join("nc").join("h.sock");
        assert!(
            !socket.parent().expect("parent").exists(),
            "sanity check: truly not created yet"
        );
        let host = Host::start().expect("host");
        host.listen(&socket)
            .expect("listen must succeed even when its own directory does not exist yet");

        let repo = test_support::seed_empty_repo();
        let mut client =
            Client::connect(&socket, Duration::from_secs(5)).expect("connect to the real socket");
        let report = client
            .request(&Call::human(
                repo.path(),
                Request::Query(AppQuery::Status(Default::default())),
            ))
            .expect("a real request over the newly bound socket");
        assert!(report.is_ok(), "{report:?}");

        host.shutdown_and_join();
    }
}

#[cfg(test)]
mod host_dispatch_tests {
    use super::Host;
    use futures::executor::block_on;
    use jerry_core::request::HookEvent;
    use jerry_core::wire::rpc_code;
    use jerry_core::{
        AgentId, AppCommand, AppQuery, Call, Message, Report, Request, WorktreeCreate,
    };
    use std::path::Path;
    use std::time::Duration;
    use test_support::{seed_empty_repo, seed_repo, wait_until};

    fn status() -> Request {
        Request::Query(AppQuery::Status(Default::default()))
    }

    fn outcome_path(report: &Report, field: &str) -> String {
        match report {
            Report::Ok { outcome } => outcome[field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} in {outcome}"))
                .to_owned(),
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[test]
    fn a_human_query_runs_against_the_caller_s_own_worktree() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();

        let report = block_on(client.request(Call::human(repo.path(), status()))).expect("report");

        let worktree = outcome_path(&report, "worktree_path");
        assert_eq!(
            std::fs::canonicalize(Path::new(&worktree)).expect("canonical"),
            std::fs::canonicalize(repo.path()).expect("canonical")
        );
        host.shutdown_and_join();
    }

    #[test]
    fn an_agent_is_only_who_the_host_registered_and_stays_in_its_worktree() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let id = AgentId::from("agent-1");

        let unknown = block_on(client.call(Call::agent(repo.path(), id.clone(), status())))
            .expect_err("an unregistered id is refused");
        assert_eq!(unknown.code, rpc_code::FORBIDDEN);

        host.agents()
            .register(id.clone(), repo.path().to_path_buf(), "Claude".into());
        let report = block_on(client.request(Call::agent(repo.path(), id.clone(), status())))
            .expect("registered");
        assert!(report.is_ok(), "{report:?}");

        let elsewhere = seed_empty_repo();
        let confined = block_on(client.call(Call::agent(elsewhere.path(), id.clone(), status())))
            .expect_err("outside its worktree");
        assert_eq!(confined.code, rpc_code::CONFINED);

        host.agents().forget(&id);
        let forgotten =
            block_on(client.call(Call::agent(repo.path(), id, status()))).expect_err("forgotten");
        assert_eq!(forgotten.code, rpc_code::FORBIDDEN);
        host.shutdown_and_join();
    }

    fn spawn_command(agent_id: AgentId) -> Request {
        Request::Command(AppCommand::SessionSpawn(jerry_core::SessionSpawn {
            program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
            args: if cfg!(windows) {
                vec!["/c".into(), "echo hi".into()]
            } else {
                vec!["-c".into(), "echo hi".into()]
            },
            env: Vec::new(),
            rows: 24,
            cols: 80,
            agent: Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id,
                grants: Vec::new(),
                parent: None,
            }),
        }))
    }

    /// The confinement path `SessionSpawn::agent` replaces the separate `AgentTable::register`
    /// call for: a session spawned with a real agent association is confined to its own worktree
    /// the moment it exists, through the real `command/session-spawn` dispatch - never a second,
    /// separate in-process registration step with its own window for a hook to arrive first.
    #[test]
    fn a_session_spawned_with_an_agent_is_confined_to_its_worktree_through_the_real_command_path() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let id = AgentId::from("agent-confined-1");

        let spawned = block_on(client.request(Call::human(repo.path(), spawn_command(id.clone()))))
            .expect("spawn dispatched");
        assert!(spawned.is_ok(), "{spawned:?}");

        let hook = Request::Hook(HookEvent {
            event: "PostToolUse".into(),
            payload: serde_json::json!({ "tool_name": "Edit" }),
        });
        let report = block_on(client.request(Call::agent(repo.path(), id.clone(), hook.clone())))
            .expect("the spawning worktree accepts its own agent's hook");
        assert!(report.is_ok(), "{report:?}");

        let elsewhere = seed_empty_repo();
        let confined = block_on(client.call(Call::agent(elsewhere.path(), id, hook)))
            .expect_err("a hook from outside the spawned worktree is confined");
        assert_eq!(confined.code, rpc_code::CONFINED);
        host.shutdown_and_join();
    }

    /// [`SessionSpawn::agent`]'s own denial rule: two live sessions must never answer to the same
    /// agent identity - `Report::Denied` with `agent-id-taken`, never a silent second registration
    /// that would leave confinement unable to tell which session a later hook call meant.
    #[test]
    fn a_second_spawn_reusing_a_live_agent_id_is_denied() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let id = AgentId::from("agent-reused-1");

        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let first = jerry_core::SessionSpawn {
            program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
            args: if cfg!(windows) {
                vec!["/c".into(), sleep.into()]
            } else {
                vec!["-c".into(), sleep.into()]
            },
            env: Vec::new(),
            rows: 24,
            cols: 80,
            agent: Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id: id.clone(),
                grants: Vec::new(),
                parent: None,
            }),
        };
        let spawned = block_on(client.request(Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionSpawn(first)),
        )))
        .expect("first spawn dispatched");
        let Report::Ok { outcome } = spawned else {
            panic!("expected ok, got {spawned:?}")
        };
        let first_id = outcome["id"].clone();

        let second = block_on(client.request(Call::human(repo.path(), spawn_command(id))))
            .expect("second spawn dispatched, even though it is refused");
        match second {
            Report::Denied { code, .. } => assert_eq!(code, "agent-id-taken"),
            other => panic!("expected denied, got {other:?}"),
        }

        // Cleanup: kill the still-sleeping first session so this test does not leak it.
        let kill = Request::Command(AppCommand::SessionKill(jerry_core::SessionKill {
            id: jerry_core::SessionId::from(first_id.as_str().expect("id string").to_owned()),
        }));
        let _ = block_on(client.request(Call::human(repo.path(), kill)));
        host.shutdown_and_join();
    }

    /// `AgentsQuery` stops listing an agent the moment its real session exits - no separate
    /// "forget" call needed, unlike the retired synthetic-registration mechanism this replaces -
    /// and the same agent id becomes spawnable again immediately afterwards, exactly because
    /// `SessionManager::agent_is_live`/`agent_entries` both key off `record.exit`, not a second,
    /// independent removal step that could race or be forgotten. A real `SessionKill`, not a
    /// natural exit, drives the process's end: an idle real shell on Windows never produces its
    /// own exit at all without something answering ConPTY's own startup query first (see
    /// `session::session_manager_tests::answer_cursor_position_query`'s own docs), which this
    /// test - checking only `AgentsQuery`, never draining the session's own output - never does.
    #[test]
    fn agents_query_stops_listing_an_agent_the_moment_its_real_session_exits() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let id = AgentId::from("agent-exit-1");

        // A real, still-running session at the moment of the kill below - `spawn_command`'s own
        // quick `echo hi` could have already exited on its own by then, racing the kill dispatch
        // into a `NotOwned` error instead of a clean, real kill.
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let long_running = jerry_core::SessionSpawn {
            program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
            args: if cfg!(windows) {
                vec!["/c".into(), sleep.into()]
            } else {
                vec!["-c".into(), sleep.into()]
            },
            env: Vec::new(),
            rows: 24,
            cols: 80,
            agent: Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id: id.clone(),
                grants: Vec::new(),
                parent: None,
            }),
        };
        let spawned = block_on(client.request(Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionSpawn(long_running)),
        )))
        .expect("spawn dispatched");
        let Report::Ok { outcome } = spawned else {
            panic!("expected ok, got {spawned:?}")
        };
        let session_id =
            jerry_core::SessionId::from(outcome["id"].as_str().expect("id string").to_owned());

        let listed = block_on(client.request(Call::human(repo.path(), status_agents_query())))
            .expect("agents query");
        let Report::Ok { outcome } = listed else {
            panic!("expected ok, got {listed:?}")
        };
        assert_eq!(
            outcome
                .as_array()
                .expect("array")
                .iter()
                .filter(|entry| entry["id"] == "agent-exit-1")
                .count(),
            1,
            "the live agent must be listed: {outcome:?}"
        );

        let kill = Request::Command(AppCommand::SessionKill(jerry_core::SessionKill {
            id: session_id,
        }));
        let killed =
            block_on(client.request(Call::human(repo.path(), kill))).expect("kill dispatched");
        assert!(killed.is_ok(), "{killed:?}");

        assert!(
            wait_until(Duration::from_secs(10), || {
                let listed =
                    block_on(client.request(Call::human(repo.path(), status_agents_query())))
                        .expect("agents query");
                let Report::Ok { outcome } = listed else {
                    return false;
                };
                outcome
                    .as_array()
                    .is_some_and(|entries| !entries.iter().any(|e| e["id"] == "agent-exit-1"))
            }),
            "AgentsQuery must stop listing the agent once its real process exits"
        );

        // The same id is immediately spawnable again - a dead session's own stale record must
        // never keep denying it as "already live".
        let reused = block_on(client.request(Call::human(repo.path(), spawn_command(id))))
            .expect("second spawn dispatched");
        assert!(
            reused.is_ok(),
            "a dead agent's id must be reusable, got {reused:?}"
        );
        host.shutdown_and_join();
    }

    fn status_agents_query() -> Request {
        Request::Query(AppQuery::Agents(Default::default()))
    }

    #[test]
    fn a_hook_event_is_fanned_out_to_every_subscriber() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let id = AgentId::from("agent-2");
        host.agents()
            .register(id.clone(), repo.path().to_path_buf(), "Claude".into());
        let mut events = client.subscribe();

        let hook = Request::Hook(HookEvent {
            event: "Stop".into(),
            payload: serde_json::json!({ "turn": 3 }),
        });
        let report =
            block_on(client.request(Call::agent(repo.path(), id.clone(), hook))).expect("ok");
        assert!(report.is_ok(), "{report:?}");

        let mut received = None;
        assert!(
            wait_until(Duration::from_secs(5), || {
                received = events.try_recv().ok();
                received.is_some()
            }),
            "one notification arrives"
        );
        match received.expect("received") {
            Message::Notification { method, params } => {
                assert_eq!(method, "event/hook");
                assert_eq!(params["agent"], serde_json::json!("agent-2"));
                assert_eq!(params["event"], serde_json::json!("Stop"));
                assert_eq!(params["payload"]["turn"], serde_json::json!(3));
            }
            other => panic!("expected a notification, got {other:?}"),
        }

        let anonymous = block_on(client.call(Call::human(
            repo.path(),
            Request::Hook(HookEvent {
                event: "Stop".into(),
                payload: serde_json::Value::Null,
            }),
        )))
        .expect_err("a hook without an agent identity is refused");
        assert_eq!(anonymous.code, rpc_code::FORBIDDEN);
        host.shutdown_and_join();
    }

    #[test]
    fn a_successful_worktree_create_publishes_one_worktree_created_notification() {
        let repo = seed_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let mut events = client.subscribe();

        let create = Request::Command(AppCommand::WorktreeCreate(WorktreeCreate {
            branch: "feature-notify".into(),
            from: None,
            agent: Some(jerry_core::AgentSpec::Claude),
            prompt: Some("do the thing".into()),
            orchestrator: true,
        }));
        let report =
            block_on(client.request(Call::human(repo.path(), create))).expect("dispatched");
        assert!(report.is_ok(), "{report:?}");
        let path = outcome_path(&report, "path");

        let mut received = None;
        assert!(
            wait_until(Duration::from_secs(5), || {
                received = events.try_recv().ok();
                received.is_some()
            }),
            "one notification arrives"
        );
        match received.expect("received") {
            Message::Notification { method, params } => {
                assert_eq!(method, "event/worktree-created");
                assert_eq!(params["path"], serde_json::json!(path));
                assert_eq!(params["agent"], serde_json::json!("claude"));
                assert_eq!(params["prompt"], serde_json::json!("do the thing"));
                assert_eq!(params["orchestrator"], serde_json::json!(true));
                assert_eq!(
                    params["requested_by"],
                    serde_json::json!({ "kind": "human" })
                );
            }
            other => panic!("expected a notification, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(Path::new(&path).parent().expect("parent"));
        host.shutdown_and_join();
    }

    #[test]
    fn a_refused_worktree_create_publishes_no_notification() {
        let repo = seed_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let mut events = client.subscribe();

        // Same branch twice: the second call is denied (`worktree-path-exists`) before it ever
        // touches git, and must publish nothing.
        let create = || {
            Request::Command(AppCommand::WorktreeCreate(WorktreeCreate {
                branch: "feature-dupe".into(),
                from: None,
                agent: None,
                prompt: None,
                orchestrator: false,
            }))
        };
        let first = block_on(client.request(Call::human(repo.path(), create()))).expect("first");
        let path = outcome_path(&first, "path");
        assert!(wait_until(Duration::from_secs(5), || events
            .try_recv()
            .is_ok()));

        let second =
            block_on(client.request(Call::human(repo.path(), create()))).expect("dispatched");
        assert!(
            matches!(second, Report::Denied { ref code, .. } if code == "worktree-path-exists")
        );
        assert!(
            events.try_recv().is_err(),
            "a denied command must publish nothing"
        );

        let _ = std::fs::remove_dir_all(Path::new(&path).parent().expect("parent"));
        host.shutdown_and_join();
    }

    /// `event/worktree-created`'s own `requested_by` names a real agent caller, not just
    /// `{"kind": "human"}` - the data `jerry-app`'s own `work_surface::worktree_created` reads to
    /// record the spawned agent's `parent` (`docs/architecture/decisions.md` §26), so a child
    /// agent's later `Stop` hook has somewhere real to forward to.
    #[test]
    fn a_worktree_create_requested_by_an_agent_names_that_agent_in_the_notification() {
        let repo = seed_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let requester = AgentId::from("agent-requester");
        host.agents().register(
            requester.clone(),
            repo.path().to_path_buf(),
            "Claude".into(),
        );
        let mut events = client.subscribe();

        let create = Request::Command(AppCommand::WorktreeCreate(WorktreeCreate {
            branch: "feature-agent-requested".into(),
            from: None,
            agent: Some(jerry_core::AgentSpec::Claude),
            prompt: None,
            orchestrator: true,
        }));
        let report = block_on(client.request(Call::agent(repo.path(), requester, create)))
            .expect("dispatched");
        assert!(report.is_ok(), "{report:?}");
        let path = outcome_path(&report, "path");

        let mut received = None;
        assert!(
            wait_until(Duration::from_secs(5), || {
                received = events.try_recv().ok();
                received.is_some()
            }),
            "one notification arrives"
        );
        match received.expect("received") {
            Message::Notification { method, params } => {
                assert_eq!(method, "event/worktree-created");
                assert_eq!(
                    params["requested_by"],
                    serde_json::json!({ "kind": "agent", "id": "agent-requester" })
                );
            }
            other => panic!("expected a notification, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(Path::new(&path).parent().expect("parent"));
        host.shutdown_and_join();
    }

    #[test]
    fn agents_query_answers_from_this_hosts_own_table() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();

        let empty = block_on(client.request(Call::human(
            repo.path(),
            Request::Query(AppQuery::Agents(Default::default())),
        )))
        .expect("empty table answers");
        assert_eq!(
            empty,
            Report::Ok {
                outcome: serde_json::json!([])
            }
        );

        let id = AgentId::from("agent-3");
        host.agents()
            .register(id.clone(), repo.path().to_path_buf(), "Claude".into());
        let populated = block_on(client.request(Call::human(
            repo.path(),
            Request::Query(AppQuery::Agents(Default::default())),
        )))
        .expect("populated table answers");
        let Report::Ok { outcome } = populated else {
            panic!("expected ok, got {populated:?}")
        };
        assert_eq!(outcome[0]["id"], serde_json::json!("agent-3"));
        assert_eq!(outcome[0]["kind"], serde_json::json!("Claude"));
        host.shutdown_and_join();
    }

    #[test]
    fn after_shutdown_every_call_fails_fast_and_shutdown_is_idempotent() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        host.shutdown_and_join();
        let err = block_on(client.call(Call::human(repo.path(), status()))).expect_err("down");
        assert_eq!(err.code, rpc_code::SHUTTING_DOWN);
        host.shutdown_and_join();
    }

    /// A long-running (never exits on its own) real session - the shared shape
    /// `a_second_spawn_reusing_a_live_agent_id_is_denied`/`agents_query_stops_listing_...` already
    /// used inline; pulled out here so the orchestrator-policy tests below can spawn several.
    fn long_running_spawn(agent: Option<jerry_core::SessionAgentInfo>) -> jerry_core::SessionSpawn {
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        jerry_core::SessionSpawn {
            program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
            args: if cfg!(windows) {
                vec!["/c".into(), sleep.into()]
            } else {
                vec!["-c".into(), sleep.into()]
            },
            env: Vec::new(),
            rows: 24,
            cols: 80,
            agent,
        }
    }

    fn spawn_session(
        client: &super::LocalClient,
        repo: &Path,
        agent: Option<jerry_core::SessionAgentInfo>,
    ) -> jerry_core::SessionId {
        let report = block_on(client.request(Call::human(
            repo.to_path_buf(),
            Request::Command(AppCommand::SessionSpawn(long_running_spawn(agent))),
        )))
        .expect("spawn dispatched");
        jerry_core::SessionId::from(outcome_path(&report, "id"))
    }

    /// `AttentionRaise` (`docs/architecture/decisions.md` §26): `Invocability::Allowed`, but only
    /// meaningful for an agent caller - a human has no session of their own for the host to key
    /// the event to, and the dispatcher refuses it the same way it refuses an anonymous `Hook`.
    #[test]
    fn attention_raise_needs_an_agent_identity_and_publishes_a_real_event() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();
        let id = AgentId::from("agent-attention");
        host.agents()
            .register(id.clone(), repo.path().to_path_buf(), "Claude".into());
        let mut events = client.subscribe();

        let anonymous = block_on(client.call(Call::human(
            repo.path(),
            Request::Command(AppCommand::AttentionRaise(jerry_core::AttentionRaise {
                message: "no agent behind this call".into(),
            })),
        )))
        .expect_err("a human has no session to key the event to");
        assert_eq!(anonymous.code, rpc_code::FORBIDDEN);

        let report = block_on(client.request(Call::agent(
            repo.path(),
            id.clone(),
            Request::Command(AppCommand::AttentionRaise(jerry_core::AttentionRaise {
                message: "need your input on the login flow".into(),
            })),
        )))
        .expect("dispatched");
        assert!(report.is_ok(), "{report:?}");

        let mut received = None;
        assert!(
            wait_until(Duration::from_secs(5), || {
                received = events.try_recv().ok();
                received.is_some()
            }),
            "one notification arrives"
        );
        match received.expect("received") {
            Message::Notification { method, params } => {
                assert_eq!(method, "event/attention");
                assert_eq!(params["agent_id"], serde_json::json!("agent-attention"));
                assert_eq!(
                    params["message"],
                    serde_json::json!("need your input on the login flow")
                );
                assert!(
                    params["session_id"].is_string(),
                    "a registered agent has a session id to key the event to: {params}"
                );
            }
            other => panic!("expected a notification, got {other:?}"),
        }
        host.shutdown_and_join();
    }

    /// `SessionSend` (`docs/architecture/decisions.md` §26): `Invocability::Denied` by default,
    /// opened per agent through `SessionAgentInfo::grants`; refuses a caller sending to its own
    /// session, and a dead/unknown target is a real error, not a silent no-op.
    #[test]
    fn session_send_is_gated_by_grants_and_refuses_self_and_unknown_targets() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();

        let target = spawn_session(&client, repo.path(), None);

        let ordinary = AgentId::from("agent-ordinary");
        spawn_session(
            &client,
            repo.path(),
            Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id: ordinary.clone(),
                grants: Vec::new(),
                parent: None,
            }),
        );
        let denied = block_on(client.call(Call::agent(
            repo.path(),
            ordinary,
            Request::Command(AppCommand::SessionSend(jerry_core::SessionSend {
                to: target.clone(),
                text: "hi".into(),
                submit: true,
            })),
        )))
        .expect_err("no grant, denied");
        assert_eq!(denied.code, rpc_code::FORBIDDEN);

        let orchestrator = AgentId::from("agent-orchestrator");
        let orchestrator_session = spawn_session(
            &client,
            repo.path(),
            Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id: orchestrator.clone(),
                grants: vec!["command/session-send".into()],
                parent: None,
            }),
        );
        let ok = block_on(client.request(Call::agent(
            repo.path(),
            orchestrator.clone(),
            Request::Command(AppCommand::SessionSend(jerry_core::SessionSend {
                to: target.clone(),
                text: "hi".into(),
                submit: true,
            })),
        )))
        .expect("granted send dispatched");
        assert!(ok.is_ok(), "{ok:?}");

        let to_self = block_on(client.request(Call::agent(
            repo.path(),
            orchestrator.clone(),
            Request::Command(AppCommand::SessionSend(jerry_core::SessionSend {
                to: orchestrator_session.clone(),
                text: "hi".into(),
                submit: true,
            })),
        )))
        .expect("dispatched, even though refused");
        match to_self {
            Report::Denied { code, .. } => assert_eq!(code, "session-send-self"),
            other => panic!("expected denied, got {other:?}"),
        }

        let dead = jerry_core::SessionId::from("no-such-session");
        let to_dead = block_on(client.request(Call::agent(
            repo.path(),
            orchestrator,
            Request::Command(AppCommand::SessionSend(jerry_core::SessionSend {
                to: dead,
                text: "hi".into(),
                submit: true,
            })),
        )))
        .expect("dispatched");
        assert!(
            matches!(to_dead, Report::Error { .. }),
            "expected error, got {to_dead:?}"
        );

        for id in [target, orchestrator_session] {
            let _ = block_on(client.request(Call::human(
                repo.path(),
                Request::Command(AppCommand::SessionKill(jerry_core::SessionKill { id })),
            )));
        }
        host.shutdown_and_join();
    }

    /// A `Stop` hook from a child with no registered orchestrator, or no registered policy,
    /// answers `continue`; a `SessionStopPolicy` registered for a child answers `stop` with its
    /// own reason exactly once, then reverts to `continue` - and `event/child-stopped` is
    /// published only for a child with a known parent (`docs/architecture/decisions.md` §26).
    #[test]
    fn a_stop_hooks_reply_honours_a_registered_policy_exactly_once() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let client = host.client();

        let parent = AgentId::from("agent-parent");
        let orphan = AgentId::from("agent-orphan");
        spawn_session(
            &client,
            repo.path(),
            Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id: orphan.clone(),
                grants: Vec::new(),
                parent: None,
            }),
        );
        let child = AgentId::from("agent-child");
        spawn_session(
            &client,
            repo.path(),
            Some(jerry_core::SessionAgentInfo {
                kind: "Claude".into(),
                agent_id: child.clone(),
                grants: Vec::new(),
                parent: Some(parent.clone()),
            }),
        );
        let mut events = client.subscribe();

        let stop_hook = |reason: &str| {
            Request::Hook(HookEvent {
                event: "Stop".into(),
                payload: serde_json::json!({ "reason": reason }),
            })
        };

        // No parent at all: always `continue`, and nothing is forwarded.
        let orphan_report =
            block_on(client.request(Call::agent(repo.path(), orphan, stop_hook("turn ended"))))
                .expect("dispatched");
        match orphan_report {
            Report::Ok { outcome } => {
                assert_eq!(outcome["decision"], serde_json::json!("continue"))
            }
            other => panic!("expected ok, got {other:?}"),
        }

        // A known parent, no policy registered yet: `continue`, but forwarded.
        let first = block_on(client.request(Call::agent(
            repo.path(),
            child.clone(),
            stop_hook("turn ended"),
        )))
        .expect("dispatched");
        match first {
            Report::Ok { outcome } => {
                assert_eq!(outcome["decision"], serde_json::json!("continue"))
            }
            other => panic!("expected ok, got {other:?}"),
        }
        let mut child_stopped = None;
        assert!(
            wait_until(Duration::from_secs(5), || {
                while let Ok(message) = events.try_recv() {
                    if let Message::Notification { method, params } = message {
                        if method == "event/child-stopped" {
                            child_stopped = Some(params);
                        }
                    }
                }
                child_stopped.is_some()
            }),
            "event/child-stopped must be published for a child with a known parent"
        );
        let params = child_stopped.expect("checked above");
        assert_eq!(params["child"], serde_json::json!("agent-child"));
        assert_eq!(params["reason"], serde_json::json!("turn ended"));

        // The parent pre-registers a `stop` decision for the child's *next* `Stop`.
        let registered = block_on(client.request(Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionStopPolicy(
                jerry_core::SessionStopPolicy {
                    child: child.clone(),
                    decision: jerry_core::StopDecision::Stop,
                    reason: Some("still validating the migration".into()),
                },
            )),
        )))
        .expect("policy registered");
        assert!(registered.is_ok(), "{registered:?}");

        let second = block_on(client.request(Call::agent(
            repo.path(),
            child.clone(),
            stop_hook("turn ended again"),
        )))
        .expect("dispatched");
        match second {
            Report::Ok { outcome } => {
                assert_eq!(outcome["decision"], serde_json::json!("stop"));
                assert_eq!(
                    outcome["reason"],
                    serde_json::json!("still validating the migration")
                );
            }
            other => panic!("expected ok, got {other:?}"),
        }

        // Consumed - the next `Stop` reverts to `continue`.
        let third =
            block_on(client.request(Call::agent(repo.path(), child, stop_hook("turn ended"))))
                .expect("dispatched");
        match third {
            Report::Ok { outcome } => {
                assert_eq!(outcome["decision"], serde_json::json!("continue"))
            }
            other => panic!("expected ok, got {other:?}"),
        }
        host.shutdown_and_join();
    }
}

/// GitHub issue #530's regression, discovered once every spawn started going through the host
/// (decisions.md §23's Part B): before that, dropping a `TerminalPane` dropped its `PtySession`,
/// whose `Drop` killed the child. Now `SessionManager` owns it, so nothing did on host shutdown -
/// `Host::shutdown`'s own new `sessions().shutdown_all()` call is what closes this.
#[cfg(test)]
mod host_shutdown_tests {
    use super::Host;
    use std::path::PathBuf;
    use std::time::Duration;
    use test_support::wait_until;

    /// A real process that blocks until its stdin closes - genuinely still running by the time
    /// shutdown reaches it, the same idiom `crate::session::session_manager_tests`'s own
    /// `shell_options` helper uses for a real, short-lived command.
    fn blocking_shell_options() -> jerry_pty::SpawnOptions {
        if cfg!(windows) {
            jerry_pty::SpawnOptions::new("cmd").args(["/d", "/c", "more"])
        } else {
            jerry_pty::SpawnOptions::new("sh").args(["-c", "cat >/dev/null"])
        }
    }

    /// `crate::hooks::settings_file::process_is_alive` in `jerry-app`'s exact pair - "does this
    /// pid still exist", real per platform - copied rather than shared across a crate boundary
    /// for this one test module. See that function's own docs for why each branch's FFI is sound.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn process_is_alive(pid: u32) -> bool {
        // SAFETY: `kill` with signal 0 performs only an existence/permission check. It has no
        // effect on the target process, and takes no pointers, so there is nothing to invalidate.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
    }

    /// The Windows twin of the `kill(pid, 0)` check above - see that twin's docs.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn process_is_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: `OpenProcess` takes only scalars and returns a handle (null on failure). It
        // borrows no memory from this process, so there is nothing for it to invalidate.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // `ERROR_INVALID_PARAMETER` is Win32's real "there is no process with that id". Every
            // other failure is reported as alive, because a wrong "dead" is the only answer here
            // that would let this test pass without the process really being gone.
            return std::io::Error::last_os_error().raw_os_error()
                != Some(ERROR_INVALID_PARAMETER as i32);
        }
        // SAFETY: `handle` was just returned by a successful `OpenProcess` and has not been
        // closed, so it is a valid handle this thread owns. A zero timeout makes this a poll.
        let state = unsafe { WaitForSingleObject(handle, 0) };
        // SAFETY: same handle, still owned here, closed exactly once and never used after.
        unsafe {
            let _ = CloseHandle(handle);
        }
        state != WAIT_OBJECT_0
    }

    #[test]
    fn shutting_down_the_host_kills_and_confirms_dead_every_live_session() {
        let host = Host::start().expect("host");
        let (_id, handle) = host
            .sessions()
            .spawn(PathBuf::from("/repo"), None, blocking_shell_options())
            .expect("spawn");
        let pid = handle
            .process_id()
            .expect("a real spawned session has a real pid");
        assert!(
            process_is_alive(pid),
            "sanity check: the freshly spawned process must be alive before shutdown"
        );

        host.shutdown_and_join();

        assert!(
            wait_until(Duration::from_secs(5), || !process_is_alive(pid)),
            "shutting down the host must kill and confirm dead every session it still owns a \
             real process for, not just tear down the socket/fanout around it"
        );
    }
}

/// `Host::run_lifecycle`'s own coverage (`docs/architecture/decisions.md` §24): an idle host
/// exits after its configured linger, a subscribed client keeps it alive past that, and an
/// explicit `Shutdown` command wakes it immediately regardless of idleness.
#[cfg(test)]
mod lifecycle_tests {
    use super::{Host, LifecycleConfig};
    use jerry_core::client::{Client, Stream};
    use jerry_core::wire::{read_frame, write_frame};
    use jerry_core::{AppCommand, Call, Message, Report, Request, RequestId, Shutdown};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use test_support::wait_until;

    /// A registry-directory socket path short enough for every platform, removed on drop.
    struct SocketPath {
        path: PathBuf,
        _temp: Option<tempfile::TempDir>,
    }

    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn socket_path(tag: &str) -> SocketPath {
        if cfg!(windows) {
            let dir = jerry_core::registry::runtime_dir().expect("runtime dir");
            std::fs::create_dir_all(&dir).expect("runtime dir");
            SocketPath {
                path: dir.join(format!("l-{}-{tag}.sock", std::process::id())),
                _temp: None,
            }
        } else {
            let temp = tempfile::TempDir::new().expect("tempdir");
            SocketPath {
                path: temp.path().join(format!("{tag}.sock")),
                _temp: Some(temp),
            }
        }
    }

    fn fast_config() -> LifecycleConfig {
        LifecycleConfig {
            poll_interval: Duration::from_millis(10),
            linger: Duration::from_millis(60),
        }
    }

    fn subscribe(stream: &mut Stream) {
        write_frame(
            stream,
            &Message::Request {
                id: RequestId::Number(0),
                method: "event/subscribe".into(),
                params: serde_json::Value::Null,
            },
        )
        .expect("send event/subscribe");
        read_frame(stream).expect("subscribe response");
    }

    #[test]
    fn an_idle_host_with_no_sessions_and_no_clients_exits_after_the_linger() {
        let host = Host::start().expect("host");
        let config = fast_config();
        let started = Instant::now();
        host.run_lifecycle(config);
        assert!(
            started.elapsed() >= config.linger,
            "must not exit before the configured linger elapses"
        );
        host.shutdown_and_join();
    }

    #[test]
    fn a_subscribed_client_keeps_the_host_alive_until_it_disconnects() {
        let host = Host::start().expect("host");
        let socket = socket_path("subscribed");
        host.listen(&socket.path).expect("listen");
        let mut client = Stream::connect(&socket.path).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        subscribe(&mut client);

        let config = fast_config();
        let exited = std::sync::Arc::new(AtomicBool::new(false));
        let host = &host;
        std::thread::scope(|scope| {
            let exited_writer = std::sync::Arc::clone(&exited);
            scope.spawn(move || {
                host.run_lifecycle(config);
                exited_writer.store(true, Ordering::SeqCst);
            });

            // A real time budget well past the linger: if the subscription did not keep the
            // host alive, `run_lifecycle` would already have returned.
            let stayed_alive = !wait_until(config.linger * 5, || exited.load(Ordering::SeqCst));
            assert!(
                stayed_alive,
                "a subscribed client must keep the host from exiting"
            );

            drop(client);
            assert!(
                wait_until(Duration::from_secs(5), || exited.load(Ordering::SeqCst)),
                "the host must exit once its only subscriber disconnects and the linger elapses"
            );
        });
        host.shutdown_and_join();
    }

    #[test]
    fn an_explicit_shutdown_request_wakes_the_lifecycle_loop_immediately() {
        let host = Host::start().expect("host");
        let socket = socket_path("shutdown-request");
        host.listen(&socket.path).expect("listen");
        let mut subscriber = Stream::connect(&socket.path).expect("connect");
        subscriber
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        subscribe(&mut subscriber);

        // Deliberately much longer than any bound this test waits on: only the explicit
        // `Shutdown` request below - never the linger - can make `run_lifecycle` return here.
        let config = LifecycleConfig {
            poll_interval: Duration::from_millis(10),
            linger: Duration::from_secs(60),
        };
        let exited = std::sync::Arc::new(AtomicBool::new(false));
        let host = &host;
        std::thread::scope(|scope| {
            let exited_writer = std::sync::Arc::clone(&exited);
            scope.spawn(move || {
                host.run_lifecycle(config);
                exited_writer.store(true, Ordering::SeqCst);
            });

            let mut client =
                Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
            let report = client
                .request(&Call::human(
                    "/repo",
                    Request::Command(AppCommand::Shutdown(Shutdown::default())),
                ))
                .expect("shutdown accepted");
            assert_eq!(
                report,
                Report::Ok {
                    outcome: serde_json::Value::Null
                }
            );

            assert!(
                wait_until(Duration::from_secs(5), || exited.load(Ordering::SeqCst)),
                "an explicit shutdown request must wake the lifecycle loop, still-connected \
                 subscriber and long linger notwithstanding"
            );
        });
        host.shutdown_and_join();
    }

    /// `Host::shutdown` called directly - never through the `Shutdown` command at all - must
    /// give a concurrently running `Self::run_lifecycle` the same wake, not just tear down the
    /// socket/sessions around it while that loop keeps waiting out its own (deliberately long)
    /// linger.
    #[test]
    fn calling_shutdown_directly_also_wakes_a_concurrently_running_lifecycle_loop() {
        let host = Host::start().expect("host");
        let config = LifecycleConfig {
            poll_interval: Duration::from_millis(10),
            linger: Duration::from_secs(60),
        };
        let exited = std::sync::Arc::new(AtomicBool::new(false));
        let host = &host;
        std::thread::scope(|scope| {
            let exited_writer = std::sync::Arc::clone(&exited);
            scope.spawn(move || {
                host.run_lifecycle(config);
                exited_writer.store(true, Ordering::SeqCst);
            });

            assert!(
                !wait_until(Duration::from_millis(100), || exited.load(Ordering::SeqCst)),
                "sanity check: nothing has asked the host to stop yet"
            );

            let _ = host.shutdown();
            assert!(
                wait_until(Duration::from_secs(5), || exited.load(Ordering::SeqCst)),
                "calling Host::shutdown directly must wake a concurrently running lifecycle loop"
            );
        });
    }

    /// The exact regression a real `jerry-host` process hit on Linux CI (`crates/jerry-host/
    /// tests/real_binary_spawn.rs`): the `Shutdown` command's own reply racing the teardown its
    /// own success wakes. A single pass rarely reproduces a thread-scheduling race, so this
    /// spins up a fresh `Host` and repeats the full request/response/wake sequence ~20 times -
    /// every one of them must observe a real `Report::Ok`, never a connection that closed before
    /// the reply arrived.
    #[test]
    fn a_shutdown_reply_always_arrives_before_the_socket_closes() {
        for i in 0..20 {
            let host = Host::start().expect("host");
            let socket = socket_path(&format!("shutdown-ordering-{i}"));
            host.listen(&socket.path).expect("listen");

            let host = &host;
            std::thread::scope(|scope| {
                scope.spawn(move || host.run_lifecycle(fast_config()));

                let mut client =
                    Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
                let report = client
                    .request(&Call::human(
                        "/repo",
                        Request::Command(AppCommand::Shutdown(Shutdown::default())),
                    ))
                    .unwrap_or_else(|error| {
                        panic!("iteration {i}: the shutdown reply must arrive intact, got {error}")
                    });
                assert_eq!(
                    report,
                    Report::Ok {
                        outcome: serde_json::Value::Null
                    },
                    "iteration {i}"
                );
            });
        }
    }
}
