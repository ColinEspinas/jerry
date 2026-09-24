//! `Hosts`: this app's real connections to the session host serving each open repository
//! (`docs/architecture/decisions.md` §24) - one real `jerry-host` process per repository in
//! production, one fresh in-process `jerry_host::Host` per repository in tests
//! (`#[cfg(test)]`-only). `AdeApp::dispatch` resolves a request's `cwd` to its repository's
//! common `.git` directory (cached) and routes through that repository's own connection - never
//! a single connection shared across every open repository. Each connection also carries its own
//! `worktree_created`/`session_exited`/`event/hook` subscriptions, feeding the same app-wide
//! handlers regardless of which repository an event came from - see `ensure_repo_host_connected`
//! for both.

use crate::repo_host::RemoteRepoHost;
use crate::root::AdeApp;
use crate::terminal::pane::SessionAdapter;
use crate::terminal::socket_adapter::SocketSessionAdapter;
use gpui::{AppContext, AsyncApp, Context, Task};
use jerry_core::wire::rpc_code;
use jerry_core::{
    AppCommand, AppQuery, Call, HookAck, HookAgentSnapshot, HooksQuery, Report, Request, RpcError,
    SessionAttach, SessionId, SessionKill, SessionRecord, SessionsQuery,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(test))]
use std::time::Duration;

/// Why a repository has no working connection right now - a visible, per-repository error state
/// (decisions.md §24), never a silent in-process fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoHostState {
    Connected,
    VersionMismatch { theirs: u32, ours: u32 },
    CannotSpawn { error: String },
}

impl RepoHostState {
    /// The one-line message [`crate::rail::render::AdeApp::render_repo_host_error_banner`] shows
    /// for a non-[`Self::Connected`] state - `None` for `Connected` itself, since that has nothing
    /// to show.
    pub(crate) fn error_message(&self) -> Option<String> {
        match self {
            RepoHostState::Connected => None,
            RepoHostState::VersionMismatch { theirs, ours } => Some(format!(
                "this repository's session host speaks protocol version {theirs}, this app \
                 speaks {ours} - restart its sessions once both match"
            )),
            RepoHostState::CannotSpawn { error } => Some(format!(
                "could not start this repository's session host: {error}"
            )),
        }
    }
}

enum Connection {
    Remote(RemoteRepoHost),
    #[cfg(test)]
    InProcess {
        /// Kept alive for as long as this `RepoHost` lives - dropping it shuts the host down.
        host: jerry_host::Host,
        client: jerry_host::LocalClient,
    },
}

/// One repository's connection - real (`Connection::Remote`) or, in tests, a throwaway in-process
/// host reached the same way. `None` connection only pairs with a non-`Connected` state.
pub(crate) struct RepoHost {
    connection: Option<Connection>,
    state: RepoHostState,
    socket: Option<PathBuf>,
    /// `worktree_created`/`session_exited`'s own draining tasks - held here (rather than
    /// detached) so they are cancelled, not orphaned, when this entry leaves `Hosts::by_repo`
    /// (§16, CLAUDE.md's entity-lifecycle rule).
    _events: Vec<Task<()>>,
}

impl RepoHost {
    /// Read by [`AdeApp::repo_host_state_for`] - `crate::rail::render`'s own per-repository error
    /// banner (`VersionMismatch`/`CannotSpawn`, with a real "Restart sessions" action).
    pub(crate) fn state(&self) -> &RepoHostState {
        &self.state
    }

    /// The socket a spawned agent's `JERRY_HOST_SOCKET` should carry - `None` unless
    /// [`Self::state`] is [`RepoHostState::Connected`]. Read by [`AdeApp::host_socket_for`].
    pub(crate) fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    fn unavailable(state: RepoHostState) -> RepoHost {
        RepoHost {
            connection: None,
            state,
            socket: None,
            _events: Vec::new(),
        }
    }

    /// Test-only: wraps an already-connected, real socket `Client` (typically to a `jerry_host::
    /// Host` this test started and is `.listen()`ing on itself, exactly like `Self::dispatch`'s
    /// own production path would reach) - for a test that needs a genuinely socket-backed
    /// connection rather than the ordinary in-process one `open_test_app` already wires up
    /// (`provenance::integration_tests`, `hooks::integration_tests`).
    #[cfg(test)]
    pub(crate) fn for_test_remote(
        client: jerry_core::client::Client,
        socket: PathBuf,
    ) -> std::io::Result<RepoHost> {
        Ok(RepoHost {
            connection: Some(Connection::Remote(RemoteRepoHost::new(client)?)),
            state: RepoHostState::Connected,
            socket: Some(socket),
            _events: Vec::new(),
        })
    }

    /// Test-only: wraps an already-started, real socket-listening in-process `jerry_host::Host` -
    /// for a test that needs `crate::hooks::flow::AdeApp::hook_injection_for`'s own lazy bring-up
    /// to find a real socket for this repository, unlike [`Self::for_test_remote`].
    #[cfg(test)]
    pub(crate) fn for_test_in_process(host: jerry_host::Host, socket: PathBuf) -> RepoHost {
        let client = host.client();
        RepoHost {
            connection: Some(Connection::InProcess { host, client }),
            state: RepoHostState::Connected,
            socket: Some(socket),
            _events: Vec::new(),
        }
    }

    /// Test-only: a repository whose connection failed or mismatched, for exercising
    /// [`Self::dispatch`]'s own error-reporting branches without a real spawn-or-connect.
    #[cfg(test)]
    pub(crate) fn for_test_unavailable(state: RepoHostState) -> RepoHost {
        RepoHost::unavailable(state)
    }

    /// Dispatches `request` (already known to be for this repository) and blocks the calling
    /// thread only inside whichever background task actually reaches the connection - see
    /// `crate::repo_host`'s own docs for why that block is always safe here, including under
    /// GPUI's single-threaded test scheduler.
    fn dispatch(
        &self,
        cwd: PathBuf,
        request: Request,
        cx: &mut Context<AdeApp>,
    ) -> Task<Result<Report, RpcError>> {
        match &self.state {
            RepoHostState::Connected => {}
            RepoHostState::VersionMismatch { theirs, ours } => {
                return Task::ready(Err(RpcError::new(
                    rpc_code::UNSUPPORTED_VERSION,
                    format!(
                        "this repository's session host speaks protocol version {theirs}, this \
                         app speaks {ours} - restart its sessions once both match"
                    ),
                )));
            }
            RepoHostState::CannotSpawn { error } => {
                return Task::ready(Err(RpcError::new(rpc_code::NEEDS_HOST, error.clone())));
            }
        }
        match &self.connection {
            Some(Connection::Remote(remote)) => {
                let remote = remote.clone();
                cx.background_spawn(async move { remote.dispatch(Call::human(cwd, request)) })
            }
            #[cfg(test)]
            Some(Connection::InProcess { client, .. }) => {
                let client = client.clone();
                cx.spawn(async move |_this, _cx| client.request(Call::human(cwd, request)).await)
            }
            None => Task::ready(Err(RpcError::new(
                rpc_code::NEEDS_HOST,
                "this repository has no session host connection",
            ))),
        }
    }
}

/// Every repository this app has open, and the connection serving each - see the module docs.
#[derive(Default)]
pub(crate) struct Hosts {
    by_repo: HashMap<PathBuf, RepoHost>,
    /// Every worktree path this app has already resolved to its repository's common `.git`
    /// directory, so a repeat dispatch for the same worktree never re-shells to git.
    common_dir_of: HashMap<PathBuf, PathBuf>,
}

impl Hosts {
    fn common_dir_for(&self, cwd: &Path) -> Option<&PathBuf> {
        self.common_dir_of.get(cwd)
    }

    fn repo_host_for(&self, common_dir: &Path) -> Option<&RepoHost> {
        self.by_repo.get(common_dir)
    }

    /// The socket a spawned agent belonging to `cwd`'s repository should carry as its
    /// `JERRY_HOST_SOCKET` - `None` for a repository with no working connection yet
    /// (`RepoHostState::Connected` only), or for a `cwd` this instance has not resolved at all.
    fn socket_for(&self, cwd: &Path) -> Option<PathBuf> {
        let common_dir = self.common_dir_of.get(cwd)?;
        self.by_repo
            .get(common_dir)?
            .socket()
            .map(Path::to_path_buf)
    }
}

/// Connects to (or, in tests, starts) the real session host for the repository at `common_dir` -
/// `#[cfg(test)]`'s in-process alternative to a genuine `jerry_core::host_spawn::spawn_or_connect`
/// call, reached the identical way by everything downstream of it (`RepoHost::dispatch`). Blocking
/// work (registry I/O, a process spawn or a real socket connect) runs entirely inside `cx.
/// background_spawn`.
#[cfg_attr(test, allow(unused_variables))]
async fn connect_repo_host(common_dir: PathBuf, cx: &mut AsyncApp) -> RepoHost {
    #[cfg(test)]
    {
        start_in_process_repo_host(cx).await
    }
    #[cfg(not(test))]
    {
        connect_production_repo_host(common_dir, cx).await
    }
}

/// Starts and binds a throwaway, always-fresh in-process host for a test that has never before
/// opened this repository - `#[cfg(test)]`'s own stand-in for `connect_production_repo_host`,
/// never a real spawn-or-connect. Panics (rather than modeling `RepoHostState::CannotSpawn`) if
/// binding fails: unlike production, where a real `jerry-host` genuinely can be unreachable, a
/// throwaway host failing to bind its own socket in a test is always a fixture bug - a directory
/// nothing created yet, a path over `MAX_SOCKET_PATH_BYTES` - never a real state any test should
/// have to model or a caller should have to notice indirectly through every dispatch answering a
/// confusing error. `RepoHost::for_test_unavailable`/`RepoHost::adopt_repo_host_for_test` remain
/// the real, deliberate way to construct a broken connection for testing the error-banner paths
/// themselves.
#[cfg(test)]
async fn start_in_process_repo_host(cx: &mut AsyncApp) -> RepoHost {
    let started = cx
        .background_spawn(async move {
            let sockets_dir = jerry_host::default_sockets_dir();
            let socket = sockets_dir.join(format!(
                "th-{:x}-{:08x}.sock",
                std::process::id(),
                jerry_core::registry::fresh_u32()
            ));
            let (host, dispatch) = jerry_host::Host::start_detached(sockets_dir);
            host.listen(&socket).map(|()| (host, socket, dispatch))
        })
        .await;
    let (host, socket, dispatch) = started.unwrap_or_else(|error| {
        panic!(
            "a test's own throwaway in-process host must always be able to bind its socket - \
             this is a fixture bug, not a state any test should model: {error}"
        )
    });
    cx.background_spawn(dispatch).detach();
    let client = host.client();
    RepoHost {
        connection: Some(Connection::InProcess { host, client }),
        state: RepoHostState::Connected,
        socket: Some(socket),
        _events: Vec::new(),
    }
}

#[cfg(not(test))]
async fn connect_production_repo_host(common_dir: PathBuf, cx: &mut AsyncApp) -> RepoHost {
    let registry_dir = match jerry_core::registry::runtime_dir() {
        Ok(dir) => dir,
        Err(error) => {
            return RepoHost::unavailable(RepoHostState::CannotSpawn {
                error: error.to_string(),
            })
        }
    };
    let outcome = cx
        .background_spawn(async move {
            #[cfg(windows)]
            let breakaway_forbidden =
                || crate::job_object::breakaway_is_forbidden_for_current_process().unwrap_or(false);
            #[cfg(not(windows))]
            let breakaway_forbidden = || false;
            jerry_core::host_spawn::spawn_or_connect_with(
                registry_dir,
                &common_dir,
                Duration::from_secs(5),
                Duration::from_secs(10),
                jerry_core::jerry_binary::locate_named,
                breakaway_forbidden,
            )
        })
        .await;
    match outcome {
        Ok(jerry_core::host_spawn::Outcome::Connected { client, descriptor }) => {
            match RemoteRepoHost::new(client) {
                Ok(remote) => RepoHost {
                    connection: Some(Connection::Remote(remote)),
                    state: RepoHostState::Connected,
                    socket: Some(descriptor.socket),
                    _events: Vec::new(),
                },
                Err(error) => RepoHost::unavailable(RepoHostState::CannotSpawn {
                    error: format!("could not start this repository's own request thread: {error}"),
                }),
            }
        }
        Ok(jerry_core::host_spawn::Outcome::VersionMismatch { descriptor }) => {
            RepoHost::unavailable(RepoHostState::VersionMismatch {
                theirs: descriptor.protocol_version,
                ours: jerry_core::wire::PROTOCOL_VERSION,
            })
        }
        Err(error) => RepoHost::unavailable(RepoHostState::CannotSpawn {
            error: error.to_string(),
        }),
    }
}

/// Resolves `cwd`'s own repository (its common `.git` directory, or `cwd` itself for a plain,
/// non-git directory - `crate::test_support::temp_root`'s own supported case) and ensures
/// [`Hosts::by_repo`] has a connection for it, opening one if this is the first time this
/// instance has ever seen this repository. Returns the resolved common directory either way, so
/// the caller can look up whatever [`RepoHost`] (or its absence) resulted. Blocking git/socket
/// work runs off the UI thread; a concurrent call for the same never-before-seen repository may
/// open two connections that race to be inserted - see `jerry_core::registry::Registry::claim`'s
/// own docs for why that race is safe rather than a caller's problem to avoid.
async fn ensure_repo_host_connected(
    this: &gpui::WeakEntity<AdeApp>,
    cwd: PathBuf,
    cx: &mut AsyncApp,
) -> PathBuf {
    // `jerry_git::git_common_dir` absolutizes `--git-common-dir`'s own (often relative) output
    // against whatever spelling of `cwd` it was given - so a worktree reached through a symlinked
    // temp-directory parent (macOS's own `/var` -> `/private/var`) and the repository's own root,
    // reached through the already-canonical spelling `crate::test_support::temp_repo` hands out,
    // resolve to two *different strings* for the same real directory. Canonicalizing here, once,
    // is what makes them collide into the same `Hosts::by_repo` key regardless of which spelling
    // of `cwd` a caller happened to pass in - `dunce::canonicalize`, not `std::fs::canonicalize`,
    // since the latter's Windows `\\?\`-prefixed form is one `git` itself rejects
    // (`crate::test_support::canonicalized`'s own identical reasoning).
    let common_dir = cx
        .background_spawn({
            let cwd = cwd.clone();
            async move {
                let resolved = jerry_git::git_common_dir(&cwd).unwrap_or_else(|_| cwd.clone());
                dunce::canonicalize(&resolved).unwrap_or(resolved)
            }
        })
        .await;
    let already_open = this
        .update(cx, |this, _cx| {
            this.hosts.common_dir_of.insert(cwd, common_dir.clone());
            this.hosts.by_repo.contains_key(&common_dir)
        })
        .unwrap_or(true);
    if already_open {
        return common_dir;
    }

    let mut repo_host = connect_repo_host(common_dir.clone(), cx).await;

    // Three dedicated event subscriptions - `worktree_created`/`session_exited`/`event/hook` each
    // filter for their own `event/*` name - opened off the UI thread for a real socket connection
    // (blocking connect + handshake), or synchronously for the in-process fanout tap
    // (`LocalClient::subscribe`, never blocking).
    let events = match &repo_host.connection {
        Some(Connection::Remote(_)) => {
            let socket = repo_host.socket.clone();
            cx.background_spawn(async move {
                let socket = socket?;
                match (
                    crate::repo_host::subscribe_remote(&socket),
                    crate::repo_host::subscribe_remote(&socket),
                    crate::repo_host::subscribe_remote(&socket),
                ) {
                    (Ok(a), Ok(b), Ok(c)) => Some((a, b, c)),
                    (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                        log::warn!(
                            "jerry-app: could not subscribe to this repository's own events: \
                             {error}"
                        );
                        None
                    }
                }
            })
            .await
        }
        #[cfg(test)]
        Some(Connection::InProcess { client, .. }) => {
            Some((client.subscribe(), client.subscribe(), client.subscribe()))
        }
        None => None,
    };

    let _ = this.update(cx, |this, cx| {
        if let Some((worktree_created_events, session_exited_events, hook_events)) = events {
            repo_host
                ._events
                .push(crate::work_surface::worktree_created::spawn_consumer(
                    worktree_created_events,
                    cx,
                ));
            repo_host
                ._events
                .push(crate::work_surface::session_exited::spawn_consumer(
                    session_exited_events,
                    cx,
                ));
            repo_host._events.push(crate::hooks::spawn_consumer(
                hook_events,
                common_dir.clone(),
                cx,
            ));
        }
        this.hosts.by_repo.insert(common_dir.clone(), repo_host);
        this.seed_hook_runtime(common_dir.clone(), cx);
    });
    common_dir
}

impl AdeApp {
    /// Ensures the repository `cwd` belongs to has a connection, opening one if this is the
    /// first time this instance has ever seen it - a cheap no-op once one already exists
    /// (including for a second worktree of an already-open repository). Blocking git/socket work
    /// runs off the UI thread. Called eagerly from [`Self::add_repo`] so a repository's
    /// connection is already resolving before its first real dispatch, and lazily from
    /// [`Self::dispatch`]'s own cache-miss fallback for a `cwd` nothing opened this way in
    /// advance (a worktree created directly on disk, or spawned into before an eager call for it
    /// resolved) - both converge on the same connection once resolved, never open two.
    pub(crate) fn open_repo_host(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        if self.hosts.common_dir_for(&cwd).is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let _ = ensure_repo_host_connected(&this, cwd, cx).await;
        })
        .detach();
    }

    /// Decisions.md §26's own seed: ensures [`AdeApp::hook_runtime`] exists
    /// ([`Self::ensure_hook_runtime`], off the UI thread) - a relaunch that reattaches an
    /// already-running agent's session spawns nothing at all, so `Self::hook_injection_for`'s own
    /// bring-up would never otherwise run - then dispatches `HooksQuery` for `common_dir`'s
    /// repository and replays
    /// every raw entry it returns into [`crate::hooks::apply_entry`], oldest first, per agent -
    /// the same real processing a live `event/hook` notification takes
    /// ([`crate::hooks::spawn_consumer`]), so a relaunched instance's rail renders an existing
    /// agent's status exactly as it would have if this instance had been running the whole time.
    /// Acknowledges what it replayed (`HookAck`, best-effort, one per agent) so the host's own
    /// bounded inbox is pruned by real consumption. Best-effort throughout - a repository with no
    /// live host answers `NEEDS_HOST` and this simply replays nothing, and hooks genuinely
    /// unsupported on this machine leaves `hook_runtime` `None` exactly as `ensure_hook_runtime`
    /// already logs. Called from both [`ensure_repo_host_connected`] (production) and
    /// [`Self::adopt_repo_host_for_test`] (the test-only connection-adoption path), so a
    /// test-adopted host behaves identically to a real one here rather than silently skipping
    /// this step.
    pub(crate) fn seed_hook_runtime(&mut self, common_dir: PathBuf, cx: &mut Context<Self>) {
        // Both started now, run concurrently - `ensure_hook_runtime`'s own bring-up (off the UI
        // thread) and this dispatch have nothing to wait on each other for.
        let ensure = self.ensure_hook_runtime(cx);
        let seed = self.dispatch(
            common_dir.clone(),
            Request::Query(AppQuery::Hooks(HooksQuery::default())),
            cx,
        );
        cx.spawn(async move |this, cx| {
            ensure.await;
            let Ok(Report::Ok { outcome }) = seed.await else {
                return;
            };
            let Ok(snapshots) = serde_json::from_value::<Vec<HookAgentSnapshot>>(outcome) else {
                return;
            };
            for snapshot in snapshots {
                let Some(up_to) = snapshot.entries.last().map(|entry| entry.seq) else {
                    continue;
                };
                let agent_id = snapshot.status.agent_id.clone();
                let applied = this.update(cx, |this, _cx| {
                    for entry in &snapshot.entries {
                        crate::hooks::apply_entry(this, entry);
                    }
                });
                if applied.is_err() {
                    continue;
                }
                let ack = this.update(cx, |this, cx| {
                    this.dispatch(
                        common_dir.clone(),
                        Request::Command(AppCommand::HookAck(HookAck { agent_id, up_to })),
                        cx,
                    )
                });
                if let Ok(task) = ack {
                    let _ = task.await;
                }
            }
        })
        .detach();
    }

    /// Dispatches through the repository `cwd` belongs to - `NEEDS_HOST` for a repository whose
    /// connection could not be opened at all (`RepoHostState::CannotSpawn`/`VersionMismatch`
    /// answer their own, more specific codes instead - see [`RepoHost::dispatch`]).
    pub fn dispatch(
        &mut self,
        cwd: PathBuf,
        request: Request,
        cx: &mut Context<Self>,
    ) -> Task<Result<Report, RpcError>> {
        if let Some(common_dir) = self.hosts.common_dir_for(&cwd).cloned() {
            if let Some(repo_host) = self.hosts.repo_host_for(&common_dir) {
                return repo_host.dispatch(cwd, request, cx);
            }
        }
        cx.spawn(async move |this, cx| {
            let common_dir = ensure_repo_host_connected(&this, cwd.clone(), cx).await;
            let dispatched = this.update(cx, |this, cx| {
                this.hosts
                    .repo_host_for(&common_dir)
                    .map(|repo_host| repo_host.dispatch(cwd, request, cx))
            });
            match dispatched {
                Ok(Some(task)) => task.await,
                _ => Err(RpcError::new(
                    rpc_code::NEEDS_HOST,
                    "this repository's session host could not be reached",
                )),
            }
        })
    }

    /// The session table's own in-process handle for the repository `cwd` belongs to, for a
    /// caller that needs to attach to a spawned session's data-plane adapter (`SessionManager::
    /// handle_for`) rather than dispatch a Command - see `docs/architecture/decisions.md` §23.
    /// Real only for a `#[cfg(test)]` in-process repository: a production (`Connection::Remote`)
    /// one has no in-process table to reach into at all - `attach_remote_session` is
    /// `Agents::spawn_inner`'s own path for that case instead. `None` also for a `cwd` with no
    /// connection yet.
    pub fn sessions_for(&self, cwd: &Path) -> Option<jerry_host::SessionManager> {
        let common_dir = self.hosts.common_dir_for(cwd)?;
        #[cfg_attr(not(test), allow(unused))]
        let connection = &self.hosts.repo_host_for(common_dir)?.connection;
        #[cfg(test)]
        if let Some(Connection::InProcess { host, .. }) = connection {
            return Some(host.sessions());
        }
        None
    }

    /// The `#[cfg(test)]` in-process half of [`Self::control_plane_for`] - real only for a
    /// `#[cfg(test)]` in-process repository, matching [`Self::sessions_for`]'s own reasoning.
    #[cfg(test)]
    fn host_client_for(&self, cwd: &Path) -> Option<jerry_host::LocalClient> {
        let common_dir = self.hosts.common_dir_for(cwd)?;
        if let Some(Connection::InProcess { client, .. }) =
            &self.hosts.repo_host_for(common_dir)?.connection
        {
            return Some(client.clone());
        }
        None
    }

    /// The `ControlPlane` handle a newly attached `TerminalPane` needs for its own
    /// `SessionResize` dispatch (`TerminalPane::attach_session`, decisions.md §23 - resize never
    /// travels on the data plane) - `ControlPlane::Remote` for a production `Connection::Remote`
    /// repository (or a test that swapped one in via `RepoHost::for_test_remote`),
    /// `ControlPlane::InProcess` for a `#[cfg(test)]` in-process one. `None` only when the
    /// repository has no working connection at all.
    pub(crate) fn control_plane_for(
        &self,
        cwd: &Path,
    ) -> Option<crate::terminal::pane::ControlPlane> {
        if let Some(remote) = self.remote_host_for(cwd) {
            return Some(crate::terminal::pane::ControlPlane::Remote(remote));
        }
        #[cfg(test)]
        if let Some(client) = self.host_client_for(cwd) {
            return Some(crate::terminal::pane::ControlPlane::InProcess(client));
        }
        None
    }

    /// [`Hosts::socket_for`] - `crate::hooks::flow`'s own per-repository `JERRY_HOST_SOCKET`
    /// resolution.
    pub(crate) fn host_socket_for(&self, cwd: &Path) -> Option<PathBuf> {
        self.hosts.socket_for(cwd)
    }

    /// The repository `cwd` belongs to's own connection state, for `crate::rail::render`'s
    /// per-repository error banner - `None` for a repository this instance has never resolved a
    /// connection for at all (including one still resolving), which is not itself an error.
    pub(crate) fn repo_host_state_for(&self, cwd: &Path) -> Option<RepoHostState> {
        let common_dir = self.hosts.common_dir_for(cwd)?;
        Some(self.hosts.repo_host_for(common_dir)?.state().clone())
    }

    /// The real "Restart sessions" action on `crate::rail::render`'s per-repository error banner:
    /// drops the repository `cwd` belongs to's own stale entry (a `VersionMismatch`/`CannotSpawn`
    /// state has no live connection to shut down - only a genuinely `Connected` one would, and
    /// this is never offered for that state) and re-runs [`Self::open_repo_host`] as if this were
    /// the first time this instance had ever seen the repository. Also drops the `cwd` -> common
    /// dir cache entry, not just `Hosts::by_repo`'s: `Self::open_repo_host`'s own early-return
    /// guard checks that cache alone, so leaving it behind would make this a silent no-op.
    pub(crate) fn restart_repo_host(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        if let Some(common_dir) = self.hosts.common_dir_of.remove(&cwd) {
            self.hosts.by_repo.remove(&common_dir);
        }
        self.open_repo_host(cwd, cx);
    }

    /// The repository `cwd` belongs to's own [`RemoteRepoHost`], for `attach_remote_session`'s
    /// synchronous `command/session-kill` dispatch on [`SessionAdapter::shutdown`] - a plain
    /// blocking call safe from any thread (§16's amendment), never through [`Self::dispatch`]/
    /// GPUI's executor, since `shutdown` is a synchronous trait method with no `.await` of its
    /// own to run one under. Real for a production `Connection::Remote` repository, and for a
    /// test that swapped one in via `RepoHost::for_test_remote`; `None` for a `#[cfg(test)]`
    /// in-process repository (which kills through `SessionManager::kill` directly instead - see
    /// [`Self::sessions_for`]) or one with no working connection at all.
    fn remote_host_for(&self, cwd: &Path) -> Option<RemoteRepoHost> {
        let common_dir = self.hosts.common_dir_for(cwd)?;
        match &self.hosts.repo_host_for(common_dir)?.connection {
            Some(Connection::Remote(remote)) => Some(remote.clone()),
            _ => None,
        }
    }

    /// Test-only: installs `repo_host` (already connected, typically [`RepoHost::for_test_remote`]
    /// or [`RepoHost::for_test_in_process`]) as the connection for the repository `cwd` belongs
    /// to, in place of whatever `Self::open_repo_host` would otherwise resolve - see that
    /// constructor's own docs for why a test reaches for this. Also wires the same
    /// `worktree_created`/`session_exited`/`event/hook` subscriptions `ensure_repo_host_connected`
    /// would, so a test driving a hook or a worktree-created notification through `repo_host`'s
    /// own connection sees it reach `self` exactly as production would. Resolving `cwd`'s own
    /// common directory is a real (if quick) git call, acceptable here since this only ever runs
    /// in a test's own synchronous setup, never on a path this crate's own conventions ask to keep
    /// off the UI thread.
    #[cfg(test)]
    pub(crate) fn adopt_repo_host_for_test(
        &mut self,
        cwd: PathBuf,
        mut repo_host: RepoHost,
        cx: &mut Context<Self>,
    ) {
        // Canonicalized for the identical reason `ensure_repo_host_connected` is - see its own
        // docs: a symlinked temp-directory parent must not make this collide with a *different*
        // `Hosts::by_repo` entry than the one a real dispatch for the same repository resolves.
        let resolved = jerry_git::git_common_dir(&cwd).unwrap_or_else(|_| cwd.clone());
        let common_dir = dunce::canonicalize(&resolved).unwrap_or(resolved);
        wire_test_repo_host_events(&mut repo_host, common_dir.clone(), cx);
        self.hosts.common_dir_of.insert(cwd, common_dir.clone());
        self.hosts.by_repo.insert(common_dir.clone(), repo_host);
        self.seed_hook_runtime(common_dir, cx);
    }
}

/// Resolves `session_id`'s own real, attachable adapter for `Agents::spawn_resolved` to hand to a
/// pane, for a repository with no in-process table to reach into at all (`AdeApp::sessions_for`
/// answering `None` - a production `Connection::Remote`, or a test that swapped one in via
/// `RepoHost::for_test_remote`): dispatches a real `command/session-attach` for the socket, a
/// `SessionsQuery` for the session's own pid (`SocketSessionAdapter::connect`'s own docs - a
/// value only a Query can resolve, never fetched by the adapter itself), then connects
/// off the UI thread. `Err` is a real, honest attach failure the caller reports as a failed spawn,
/// never a silent no-op.
pub(crate) async fn attach_remote_session(
    this: &gpui::WeakEntity<AdeApp>,
    cwd: PathBuf,
    session_id: SessionId,
    cx: &mut AsyncApp,
) -> Result<Arc<dyn SessionAdapter>, String> {
    const APP_DROPPED: &str = "the app itself was dropped before this session could attach";

    let attach_call = this
        .update(cx, |this, cx| {
            this.dispatch(
                cwd.clone(),
                Request::Command(AppCommand::SessionAttach(SessionAttach {
                    id: session_id.clone(),
                })),
                cx,
            )
        })
        .map_err(|_| APP_DROPPED.to_string())?;
    let socket = match attach_call.await {
        Ok(Report::Ok { outcome }) => outcome
            .get("socket")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| "internal error: session-attach answered with no socket".to_string())?,
        Ok(other) => return Err(format!("could not attach to this session: {other:?}")),
        Err(error) => return Err(error.message),
    };

    // Never fatal on its own - a session whose pid could not be resolved still attaches, just
    // without `SessionsQuery::process_id` ever answering for it (the pane header falls back to
    // showing none, exactly as `SocketSessionAdapter::process_id`'s own docs describe).
    let sessions_call = this.update(cx, |this, cx| {
        this.dispatch(
            cwd.clone(),
            Request::Query(AppQuery::Sessions(SessionsQuery::default())),
            cx,
        )
    });
    let process_id = match sessions_call {
        Ok(task) => match task.await {
            Ok(Report::Ok { outcome }) => serde_json::from_value::<Vec<SessionRecord>>(outcome)
                .ok()
                .and_then(|records| records.into_iter().find(|record| record.id == session_id))
                .and_then(|record| record.process_id),
            _ => None,
        },
        Err(_) => None,
    };

    let remote = this
        .update(cx, |this, _cx| this.remote_host_for(&cwd))
        .map_err(|_| APP_DROPPED.to_string())?
        .ok_or_else(|| {
            "internal error: this repository has no remote session host to kill through".to_string()
        })?;
    let kill_id = session_id.clone();
    let kill_cwd = cwd.clone();
    let kill = move || -> Result<(), String> {
        match remote.dispatch(Call::human(
            kill_cwd.clone(),
            Request::Command(AppCommand::SessionKill(SessionKill {
                id: kill_id.clone(),
            })),
        )) {
            Ok(Report::Ok { .. }) => Ok(()),
            Ok(other) => Err(format!("{other:?}")),
            Err(error) => Err(error.message),
        }
    };

    cx.background_spawn(async move {
        SocketSessionAdapter::connect(&socket, session_id, process_id, kill)
            .map(|adapter| Arc::new(adapter) as Arc<dyn SessionAdapter>)
            .map_err(|error| error.to_string())
    })
    .await
}

/// [`AdeApp::adopt_repo_host_for_test`]'s own event wiring - a synchronous twin of
/// `ensure_repo_host_connected`'s, since a test-adopted connection is always already fully
/// connected (no async spawn-or-connect to await first).
#[cfg(test)]
fn wire_test_repo_host_events(
    repo_host: &mut RepoHost,
    common_dir: PathBuf,
    cx: &mut Context<AdeApp>,
) {
    let events = match &repo_host.connection {
        Some(Connection::Remote(_)) => repo_host.socket.clone().and_then(|socket| {
            match (
                crate::repo_host::subscribe_remote(&socket),
                crate::repo_host::subscribe_remote(&socket),
                crate::repo_host::subscribe_remote(&socket),
            ) {
                (Ok(a), Ok(b), Ok(c)) => Some((a, b, c)),
                _ => None,
            }
        }),
        Some(Connection::InProcess { client, .. }) => {
            Some((client.subscribe(), client.subscribe(), client.subscribe()))
        }
        None => None,
    };
    if let Some((worktree_created_events, session_exited_events, hook_events)) = events {
        repo_host
            ._events
            .push(crate::work_surface::worktree_created::spawn_consumer(
                worktree_created_events,
                cx,
            ));
        repo_host
            ._events
            .push(crate::work_surface::session_exited::spawn_consumer(
                session_exited_events,
                cx,
            ));
        repo_host
            ._events
            .push(crate::hooks::spawn_consumer(hook_events, common_dir, cx));
    }
}

/// Where the `jerry` CLI binary lives, for hook and skill injection (decision Q8,
/// `docs/architecture/decisions.md` §17, §19, §26): `jerry_core::jerry_binary::locate` (a sibling
/// of this process's own executable, then `bin/jerry` next to it, then one directory up - a cargo
/// test binary's own exe lives in `target/<profile>/deps/`, not `target/<profile>/`, and unix's
/// own `deps/` copy of a `[[bin]]` target is hash-suffixed, unlike Windows's, so only that third
/// tier finds a real one from a unix test binary), then `PATH`. Neither found means `None`, so a
/// caller never injects a command pointing at nothing.
pub fn find_jerry_binary() -> Option<PathBuf> {
    locate_jerry_binary(jerry_core::jerry_binary::locate, jerry_pty::resolve_on_path)
}

/// [`find_jerry_binary`], with both lookups injected - the seam a test drives deterministically.
/// `jerry_core::jerry_binary::locate`'s own sibling/`bin/`/one-directory-up tiers already have
/// their own tests in that crate; this composition is the only thing this crate owns here.
fn locate_jerry_binary(
    locate: impl FnOnce() -> Option<PathBuf>,
    resolve_on_path: impl FnOnce(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    locate().or_else(|| resolve_on_path("jerry"))
}

#[cfg(test)]
mod find_jerry_binary_tests {
    use super::locate_jerry_binary;
    use std::path::PathBuf;

    /// `jerry_core::jerry_binary::locate`'s own sibling/`bin/`/one-directory-up tiers already have
    /// their own tests in that crate (`crates/jerry-core/src/jerry_binary.rs`); this module proves
    /// only the composition this crate itself owns - `locate`, then `PATH`.
    #[test]
    fn a_real_locate_result_wins_over_path() {
        let found = locate_jerry_binary(
            || Some(PathBuf::from("/real/jerry")),
            |_| panic!("must not fall back to PATH when locate already found one"),
        );
        assert_eq!(found, Some(PathBuf::from("/real/jerry")));
    }

    #[test]
    fn path_is_the_last_resort() {
        let on_path = PathBuf::from("/usr/local/bin/jerry");

        let found = locate_jerry_binary(
            || None,
            |name| {
                assert_eq!(name, "jerry");
                Some(on_path.clone())
            },
        );
        assert_eq!(found, Some(on_path));
    }

    #[test]
    fn neither_tier_is_a_real_none_not_a_broken_guess() {
        assert_eq!(locate_jerry_binary(|| None, |_| None), None);
    }
}

#[cfg(test)]
mod app_dispatch_tests {
    use super::RepoHost;
    use crate::test_support::{open_test_app, temp_repo};
    use gpui::TestAppContext;
    use jerry_core::wire::rpc_code;
    use jerry_core::{AppCommand, AppQuery, Report, Request, SessionSpawn, SessionsQuery};
    use std::path::PathBuf;

    #[gpui::test]
    async fn the_app_dispatches_a_query_through_its_repos_own_host_and_reads_the_report(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        let report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("the repo's own host answers");
        match report {
            Report::Ok { outcome } => {
                let worktree = PathBuf::from(outcome["worktree_path"].as_str().expect("path"));
                assert_eq!(
                    std::fs::canonicalize(worktree).expect("canonical"),
                    std::fs::canonicalize(repo.path()).expect("canonical")
                );
            }
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[gpui::test]
    async fn two_open_repositories_dispatch_through_two_different_hosts(cx: &mut TestAppContext) {
        let repo_a = temp_repo();
        let repo_b = temp_repo();
        let (app, cx) = open_test_app(cx, repo_a.to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.add_repo(repo_b.to_path_buf(), cx);
        });
        cx.run_until_parked();

        let report_a = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo_a.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("repo a's own host answers");
        let Report::Ok { outcome: outcome_a } = report_a else {
            panic!("expected ok for repo a")
        };
        assert_eq!(
            std::fs::canonicalize(outcome_a["worktree_path"].as_str().expect("path"))
                .expect("canonical"),
            std::fs::canonicalize(repo_a.path()).expect("canonical"),
            "repo a's dispatch must answer from repo a's own host, not repo b's"
        );

        let report_b = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo_b.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("repo b's own host answers");
        let Report::Ok { outcome: outcome_b } = report_b else {
            panic!("expected ok for repo b")
        };
        assert_eq!(
            std::fs::canonicalize(outcome_b["worktree_path"].as_str().expect("path"))
                .expect("canonical"),
            std::fs::canonicalize(repo_b.path()).expect("canonical"),
            "repo b's dispatch must answer from repo b's own host, not repo a's"
        );
    }

    /// `Self::adopt_repo_host_for_test`/[`crate::host::RepoHost::for_test_remote`] swapped in for
    /// the ordinary in-process default `open_test_app` wires up - proving `AdeApp::dispatch`
    /// itself is agnostic to which `Connection` variant serves a repository, the same real socket
    /// path production takes (`RemoteRepoHost`, `crate::repo_host`'s own tests, at a lower level).
    #[gpui::test]
    async fn dispatch_reaches_a_repository_served_over_a_real_remote_connection(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        let host = jerry_host::Host::start().expect("host");
        let socket_dir = jerry_core::registry::runtime_dir().expect("runtime dir");
        let socket = socket_dir.join(format!(
            "rh-{:x}-{:08x}.sock",
            std::process::id(),
            jerry_core::registry::fresh_u32()
        ));
        host.listen(&socket).expect("listen");
        let client =
            jerry_core::client::Client::connect(&socket, std::time::Duration::from_secs(5))
                .expect("connect to the real socket");
        let repo_host =
            RepoHost::for_test_remote(client, socket).expect("wrap the real connection");
        app.update(cx, |app, cx| {
            app.adopt_repo_host_for_test(repo.to_path_buf(), repo_host, cx);
        });

        let report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("the real remote connection answers");
        match report {
            Report::Ok { outcome } => {
                assert_eq!(
                    std::fs::canonicalize(outcome["worktree_path"].as_str().expect("path"))
                        .expect("canonical"),
                    std::fs::canonicalize(repo.path()).expect("canonical")
                );
            }
            other => panic!("expected ok, got {other:?}"),
        }
        host.shutdown_and_join();
    }

    /// A repository whose real host speaks a different protocol version is a visible,
    /// per-repository error - `UNSUPPORTED_VERSION`, never a silent fallback to some other
    /// connection - decisions.md §24's own "no in-process fallback on version mismatch" rule.
    #[gpui::test]
    async fn a_version_mismatched_repository_answers_unsupported_version_not_silently(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.adopt_repo_host_for_test(
                repo.to_path_buf(),
                crate::host::RepoHost::for_test_unavailable(
                    crate::host::RepoHostState::VersionMismatch {
                        theirs: 99,
                        ours: jerry_core::wire::PROTOCOL_VERSION,
                    },
                ),
                cx,
            );
        });

        let err = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect_err("a version mismatch must never silently answer");
        assert_eq!(err.code, rpc_code::UNSUPPORTED_VERSION);
    }

    /// [`crate::host::RepoHostState::CannotSpawn`] - the other visible, per-repository error
    /// state, for a repository whose real host could not be reached or spawned at all.
    #[gpui::test]
    async fn a_repository_that_could_not_spawn_a_host_answers_needs_host_not_silently(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.adopt_repo_host_for_test(
                repo.to_path_buf(),
                crate::host::RepoHost::for_test_unavailable(
                    crate::host::RepoHostState::CannotSpawn {
                        error: "no jerry-host binary was found".to_string(),
                    },
                ),
                cx,
            );
        });

        let err = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect_err("a repository that could not spawn a host must never silently answer");
        assert_eq!(err.code, rpc_code::NEEDS_HOST);
        assert!(err.message.contains("no jerry-host binary was found"));
    }

    /// A `cwd` never opened through `add_repo` (a worktree created directly on disk - `crate::
    /// merge::flow`'s own real-worktree fixtures do exactly this) still gets a real connection,
    /// lazily, the moment something dispatches against it - not a permanent `NEEDS_HOST` just
    /// because no call site happened to open it first.
    #[gpui::test]
    async fn dispatch_lazily_opens_a_connection_for_a_cwd_never_explicitly_opened(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        let never_opened = temp_repo();
        let report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    never_opened.to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("a real connection opens lazily rather than answering NEEDS_HOST forever");
        match report {
            Report::Ok { outcome } => {
                assert_eq!(
                    std::fs::canonicalize(outcome["worktree_path"].as_str().expect("path"))
                        .expect("canonical"),
                    std::fs::canonicalize(never_opened.path()).expect("canonical")
                );
            }
            other => panic!("expected ok, got {other:?}"),
        }
    }

    /// The defensive half of `ensure_repo_host_connected`'s own `dunce::canonicalize` step: a
    /// worktree reached through a real symlinked parent - standing in for macOS's own ambient
    /// `/var` -> `/private/var` (this test makes its own, so the invariant is checked on every
    /// platform, not only wherever an ambient symlink happens to differ) - must dispatch through
    /// the *same* host entry the repository's own canonical path already opened, never a second,
    /// independent one: a session spawned through the symlinked path must be visible through a
    /// `SessionsQuery` against the canonical path too.
    #[gpui::test]
    async fn a_worktree_path_reached_through_a_symlinked_parent_dispatches_through_the_same_host(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        let symlink_parent = tempfile::TempDir::new().expect("tempdir");
        let symlinked_repo = symlink_parent.path().join("repo-via-symlink");
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(repo.path(), &symlinked_repo).is_ok();
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(repo.path(), &symlinked_repo).is_ok();
        if !created {
            // A sandboxed or unprivileged environment may refuse real symlink creation
            // (`SeCreateSymbolicLinkPrivilege` on Windows without Developer Mode) - a graceful
            // skip, not a hard failure, matching `hooks::integration_tests::skip_or_fail`'s own
            // reasoning for an environment-dependent precondition this test does not control.
            eprintln!(
                "skipping a_worktree_path_reached_through_a_symlinked_parent_dispatches_through_the_same_host: \
                 could not create a real symlink in this environment"
            );
            return;
        }

        #[cfg(windows)]
        let idle_command = (PathBuf::from("cmd"), Vec::new());
        #[cfg(not(windows))]
        let idle_command = (PathBuf::from("sh"), Vec::new());
        let (program, args) = idle_command;
        let spawn_report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    symlinked_repo.clone(),
                    Request::Command(AppCommand::SessionSpawn(SessionSpawn {
                        program,
                        args,
                        env: Vec::new(),
                        rows: 24,
                        cols: 80,
                        agent: None,
                    })),
                    cx,
                )
            })
            .await
            .expect("spawn through the symlinked path must succeed");
        let Report::Ok { outcome } = spawn_report else {
            panic!("expected ok, got {spawn_report:?}")
        };
        let session_id = outcome["id"]
            .as_str()
            .expect("the outcome carries a real session id")
            .to_owned();

        let sessions_report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Query(AppQuery::Sessions(SessionsQuery::default())),
                    cx,
                )
            })
            .await
            .expect("query through the repository's own canonical path must succeed");
        let Report::Ok { outcome } = sessions_report else {
            panic!("expected ok, got {sessions_report:?}")
        };
        let ids: Vec<String> = outcome
            .as_array()
            .expect("array")
            .iter()
            .map(|record| record["id"].as_str().expect("id").to_owned())
            .collect();
        assert!(
            ids.contains(&session_id),
            "a session spawned through a symlinked path to this repository must be visible \
             through the repository's own canonical path too - proving both resolved to the \
             same host; {ids:?} did not contain {session_id}"
        );
    }

    /// The identical invariant the symlink test above checks, exercised through a path
    /// `dunce::canonicalize` resolves without needing any OS-level symlink privilege - a trailing
    /// `.` component, still the exact same real directory. Kept alongside the symlink test rather
    /// than instead of it: this one runs unconditionally on every platform (including a
    /// sandboxed Windows environment that refuses real symlink creation, where the symlink test
    /// above gracefully skips), so the fix has at least one always-real proof here even where the
    /// symlink case cannot be exercised.
    #[gpui::test]
    async fn a_worktree_path_with_a_redundant_component_dispatches_through_the_same_host(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();

        let redundant_path = repo.path().join(".");

        #[cfg(windows)]
        let idle_command = (PathBuf::from("cmd"), Vec::new());
        #[cfg(not(windows))]
        let idle_command = (PathBuf::from("sh"), Vec::new());
        let (program, args) = idle_command;
        let spawn_report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    redundant_path,
                    Request::Command(AppCommand::SessionSpawn(SessionSpawn {
                        program,
                        args,
                        env: Vec::new(),
                        rows: 24,
                        cols: 80,
                        agent: None,
                    })),
                    cx,
                )
            })
            .await
            .expect("spawn through the redundant-component path must succeed");
        let Report::Ok { outcome } = spawn_report else {
            panic!("expected ok, got {spawn_report:?}")
        };
        let session_id = outcome["id"]
            .as_str()
            .expect("the outcome carries a real session id")
            .to_owned();

        let sessions_report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Query(AppQuery::Sessions(SessionsQuery::default())),
                    cx,
                )
            })
            .await
            .expect("query through the repository's own canonical path must succeed");
        let Report::Ok { outcome } = sessions_report else {
            panic!("expected ok, got {sessions_report:?}")
        };
        let ids: Vec<String> = outcome
            .as_array()
            .expect("array")
            .iter()
            .map(|record| record["id"].as_str().expect("id").to_owned())
            .collect();
        assert!(
            ids.contains(&session_id),
            "a session spawned through a redundant-component path to this repository must be \
             visible through the repository's own canonical path too - proving both resolved to \
             the same host; {ids:?} did not contain {session_id}"
        );
    }
}

#[cfg(test)]
mod in_process_test_host_socket_length_tests {
    use jerry_core::registry::{runtime_dir_for, Os, MAX_SOCKET_PATH_BYTES};
    use std::ffi::OsString;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    /// `start_in_process_repo_host`'s own `th-<pid>-<fresh>.sock` naming, worst case, against
    /// every platform's runtime directory shape - the same margin check `jerry-host`'s own
    /// `data_plane::socket_length_tests` runs for its data-plane sockets, applied here since this
    /// test-only control socket is one byte longer (`"th-"` vs `"d-"`) and had never itself been
    /// checked.
    #[test]
    fn every_platforms_runtime_dir_leaves_real_margin_for_the_in_process_test_hosts_own_socket() {
        let cases = [
            (
                Os::Windows,
                vec![("LOCALAPPDATA", r"C:\Users\someone\AppData\Local")],
            ),
            (
                Os::MacOs,
                vec![
                    (
                        "TMPDIR",
                        "/private/var/folders/36/0123456789abcdefghijklmnop/T/",
                    ),
                    ("USER", "someuser"),
                ],
            ),
            (Os::Unix, vec![("XDG_RUNTIME_DIR", "/run/user/4294967295")]),
        ];
        let worst_case_name = format!("th-{:x}-{:08x}.sock", u32::MAX, u32::MAX);
        for (os, pairs) in cases {
            let dir = runtime_dir_for(os, &env(&pairs)).expect("runtime dir");
            let socket = dir.join(&worst_case_name);
            let len = socket.as_os_str().len();
            assert!(
                len <= MAX_SOCKET_PATH_BYTES,
                "{os:?}: {} is {len} bytes, over the {MAX_SOCKET_PATH_BYTES}-byte limit - a \
                 test's own in-process repo host would fail to bind its control socket",
                socket.display()
            );
        }
    }
}
