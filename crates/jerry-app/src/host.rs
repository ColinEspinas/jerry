//! `Hosts`: this app's real connections to the session host serving each open repository
//! (`docs/architecture/decisions.md` §24) - one real `jerry-host` process per repository in
//! production, spawn-or-connected to via `jerry_core::host_spawn`; one fresh in-process
//! `jerry_host::Host` per repository in tests (`#[cfg(test)]`-only), reached the identical way a
//! real out-of-process one would be. `AdeApp::dispatch` resolves a request's `cwd` to its
//! repository's common `.git` directory (cached) and routes through that repository's own
//! connection - never a single connection shared across every open repository. `Self::add_repo`
//! opens a connection eagerly; `Self::dispatch` opens one lazily for any `cwd` that reaches it
//! first (a worktree created directly on disk, or dispatched into before an eager open for it
//! resolved) - see `ensure_repo_host_connected`'s own docs for the rare, harmless race when both
//! happen concurrently for a repository neither has seen yet.
//!
//! Each repository connection carries its own `worktree_created`/`session_exited` event
//! subscription - two dedicated sockets in production (`crate::repo_host::subscribe_remote`), the
//! in-process fanout tapped twice in tests - feeding the same app-wide handlers regardless of
//! which repository an event came from. Not yet complete: `event/hook` is not one of the two
//! subscribed events yet, and `crate::work_surface::agents::Agents::host_agents` still points at
//! whichever repository connected most recently rather than being resolved per agent - both
//! tracked in decisions.md §24 as this cutover's own remaining steps.

use crate::repo_host::RemoteRepoHost;
use crate::root::AdeApp;
use gpui::{AppContext, AsyncApp, Context, Task};
use jerry_core::wire::rpc_code;
use jerry_core::{Call, Report, Request, RpcError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// Not yet read anywhere: the per-repository error banner (`VersionMismatch`/`CannotSpawn`,
    /// with a real "Restart sessions" action) is this cutover's own remaining "UI states" step,
    /// tracked in decisions.md §24 - real, tested infrastructure ([`Hosts::entries`] iterates
    /// exactly this) ahead of its one production consumer rather than guessed at once needed.
    #[allow(dead_code)]
    pub(crate) fn state(&self) -> &RepoHostState {
        &self.state
    }

    /// The socket a spawned agent's `JERRY_HOST_SOCKET` should carry - `None` unless
    /// [`Self::state`] is [`RepoHostState::Connected`]. Not yet read anywhere: per-repository
    /// `JERRY_HOST_SOCKET` injection is this cutover's own remaining "hooks" step (decisions.md
    /// §24).
    #[allow(dead_code)]
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
    /// (`AdeApp::any_in_process_host_client_and_socket`) to find it, unlike [`Self::for_test_remote`].
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

    /// Every repository currently connected, for a UI that lists per-repository error states -
    /// [`RepoHost::state`]'s own docs: real, tested, ahead of its one production consumer
    /// (decisions.md §24's remaining "UI states" step).
    #[allow(dead_code)]
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&PathBuf, &RepoHost)> {
        self.by_repo.iter()
    }

    /// Any one connected repository's in-process client and socket, for
    /// `crate::hooks::flow::AdeApp::hook_injection_for`'s bring-up - a stand-in until hook
    /// injection becomes properly per-repository (`docs/architecture/decisions.md` §24's own
    /// remaining "hooks" step), real only for a `#[cfg(test)]` in-process repository exactly like
    /// [`AdeApp::sessions_for`]. `None` in production, and `None` before any repository connects.
    #[cfg_attr(not(test), allow(unused_variables))]
    fn any_in_process_client_and_socket(&self) -> Option<(jerry_host::LocalClient, PathBuf)> {
        self.by_repo.values().find_map(|repo_host| {
            #[cfg(test)]
            if let Some(Connection::InProcess { client, .. }) = &repo_host.connection {
                return repo_host
                    .socket
                    .clone()
                    .map(|socket| (client.clone(), socket));
            }
            None
        })
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
    let (host, socket, dispatch) = match started {
        Ok(started) => started,
        Err(error) => {
            return RepoHost::unavailable(RepoHostState::CannotSpawn {
                error: error.to_string(),
            })
        }
    };
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
    let common_dir = cx
        .background_spawn({
            let cwd = cwd.clone();
            async move { jerry_git::git_common_dir(&cwd) }
        })
        .await
        .unwrap_or_else(|_| cwd.clone());
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

    // Two dedicated event subscriptions - `worktree_created`/`session_exited` each filter for
    // their own `event/*` name - opened off the UI thread for a real socket connection (blocking
    // connect + handshake), or synchronously for the in-process fanout tap (`LocalClient::
    // subscribe`, never blocking).
    let events = match &repo_host.connection {
        Some(Connection::Remote(_)) => {
            let socket = repo_host.socket.clone();
            cx.background_spawn(async move {
                let socket = socket?;
                match (
                    crate::repo_host::subscribe_remote(&socket),
                    crate::repo_host::subscribe_remote(&socket),
                ) {
                    (Ok(a), Ok(b)) => Some((a, b)),
                    (Err(error), _) | (_, Err(error)) => {
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
            Some((client.subscribe(), client.subscribe()))
        }
        None => None,
    };

    let _ = this.update(cx, |this, cx| {
        // `#[cfg(test)]` only: a fresh in-process host's `AgentTable` is the shortcut `Agents::
        // spawn_resolved` still calls directly (this cutover's own remaining step, per the
        // module docs) - attached here so it exists before any agent in this repository spawns.
        // Overwrites a previous repository's own table for a multi-repository test, an
        // explicitly accepted narrow gap until that step lands.
        #[cfg(test)]
        if let Some(Connection::InProcess { host, .. }) = &repo_host.connection {
            this.agents.attach_host(host.agents());
        }
        if let Some((worktree_created_events, session_exited_events)) = events {
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
        }
        this.hosts.by_repo.insert(common_dir.clone(), repo_host);
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
    /// one has no in-process table to reach into at all - `Agents::spawn_inner`'s own migration
    /// to `SocketSessionAdapter` for that case is this cutover's still-pending "adapter wiring"
    /// step, tracked in decisions.md §24. `None` also for a `cwd` with no connection yet.
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

    /// The repository `cwd` belongs to's own dispatch handle, for a caller (`TerminalPane::
    /// attach_session`) that needs to reach the control plane directly for something narrower
    /// than a full [`Self::dispatch`] call - `SessionResize` (decisions.md §23).
    /// [`Self::sessions_for`]'s own reasoning: real only for a `#[cfg(test)]` in-process
    /// repository.
    pub fn host_client_for(&self, cwd: &Path) -> Option<jerry_host::LocalClient> {
        let common_dir = self.hosts.common_dir_for(cwd)?;
        #[cfg_attr(not(test), allow(unused))]
        let connection = &self.hosts.repo_host_for(common_dir)?.connection;
        #[cfg(test)]
        if let Some(Connection::InProcess { client, .. }) = connection {
            return Some(client.clone());
        }
        None
    }

    /// [`Hosts::any_in_process_client_and_socket`] - `crate::hooks::flow`'s own bring-up.
    pub(crate) fn any_in_process_host_client_and_socket(
        &self,
    ) -> Option<(jerry_host::LocalClient, PathBuf)> {
        self.hosts.any_in_process_client_and_socket()
    }

    /// Test-only: installs `repo_host` (already connected, typically [`RepoHost::for_test_remote`])
    /// as the connection for the repository `cwd` belongs to, in place of whatever `Self::
    /// open_repo_host` would otherwise resolve - see that constructor's own docs for why a test
    /// reaches for this. Resolving `cwd`'s own common directory is a real (if quick) git call,
    /// acceptable here since this only ever runs in a test's own synchronous setup, never on a
    /// path this crate's own conventions ask to keep off the UI thread.
    #[cfg(test)]
    pub(crate) fn adopt_repo_host_for_test(
        &mut self,
        cwd: PathBuf,
        repo_host: RepoHost,
        _cx: &mut Context<Self>,
    ) {
        let common_dir = jerry_git::git_common_dir(&cwd).unwrap_or_else(|_| cwd.clone());
        self.hosts.common_dir_of.insert(cwd, common_dir.clone());
        // `Self::open_repo_host`'s own attach step: without this, `Agents.host_agents` keeps
        // pointing at whatever `open_test_app` already wired up for this same repository, and a
        // registration `Agents::spawn_resolved` makes afterwards would land in that orphaned
        // table instead of the one this call is actually replacing it with.
        if let Some(Connection::InProcess { host, .. }) = &repo_host.connection {
            self.agents.attach_host(host.agents());
        }
        self.hosts.by_repo.insert(common_dir, repo_host);
    }
}

/// Where the `jerry` CLI binary lives, for hook and skill injection (decision Q8,
/// `docs/architecture/decisions.md` §17, §19): a sibling of this process's own executable, then
/// `bin/jerry` next to it, then `PATH`. Neither found means `None`, so a caller never injects a
/// command pointing at nothing.
pub fn find_jerry_binary() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    locate_jerry_binary(&current_exe, jerry_pty::resolve_on_path)
}

/// [`find_jerry_binary`], with the executable path and the `PATH` lookup injected - the seam a
/// test drives against a fake directory layout rather than this machine's real install.
fn locate_jerry_binary(
    current_exe: &Path,
    resolve_on_path: impl FnOnce(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let name = if cfg!(windows) { "jerry.exe" } else { "jerry" };
    let dir = current_exe.parent()?;
    let sibling = dir.join(name);
    if sibling.is_file() {
        return Some(sibling);
    }
    let nested = dir.join("bin").join(name);
    if nested.is_file() {
        return Some(nested);
    }
    resolve_on_path("jerry")
}

#[cfg(test)]
mod find_jerry_binary_tests {
    use super::locate_jerry_binary;
    use std::path::PathBuf;

    /// A real, empty file at `path` - `locate_jerry_binary` only accepts what really exists, so
    /// a fake layout needs a real (if empty) file at each candidate to be meaningful.
    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, b"").expect("write");
    }

    fn exe_name() -> &'static str {
        if cfg!(windows) {
            "jerry.exe"
        } else {
            "jerry"
        }
    }

    #[test]
    fn a_sibling_of_the_running_executable_wins_over_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("jerry-app-bin").join("current-exe");
        touch(&current_exe);
        let sibling = current_exe.with_file_name(exe_name());
        touch(&sibling);

        let found = locate_jerry_binary(&current_exe, |_| {
            panic!("must not fall back to PATH when a sibling exists")
        });
        assert_eq!(found, Some(sibling));
    }

    #[test]
    fn a_bin_subdirectory_next_to_the_executable_is_the_second_place_checked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("current-exe");
        touch(&current_exe);
        let nested = temp.path().join("bin").join(exe_name());
        touch(&nested);

        let found = locate_jerry_binary(&current_exe, |_| {
            panic!("must not fall back to PATH when bin/jerry exists")
        });
        assert_eq!(found, Some(nested));
    }

    #[test]
    fn path_is_the_last_resort() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("current-exe");
        touch(&current_exe);
        let on_path = PathBuf::from("/usr/local/bin/jerry");

        let found = locate_jerry_binary(&current_exe, |name| {
            assert_eq!(name, "jerry");
            Some(on_path.clone())
        });
        assert_eq!(found, Some(on_path));
    }

    #[test]
    fn none_of_the_three_existing_is_a_real_none_not_a_broken_guess() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("current-exe");
        touch(&current_exe);

        assert_eq!(locate_jerry_binary(&current_exe, |_| None), None);
    }

    #[test]
    fn an_executable_with_no_parent_directory_is_also_a_real_none() {
        assert_eq!(
            locate_jerry_binary(std::path::Path::new(""), |_| None),
            None
        );
    }
}

#[cfg(test)]
mod app_dispatch_tests {
    use super::RepoHost;
    use crate::test_support::{open_test_app, temp_repo};
    use gpui::TestAppContext;
    use jerry_core::wire::rpc_code;
    use jerry_core::{AppQuery, Report, Request};
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
}
