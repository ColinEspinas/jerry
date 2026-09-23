//! The app's side of the session host: brings `jerry_host::Host` up in this process, publishes
//! one registry descriptor listing every open repository, hands agents to the host's table, and
//! dispatches Commands and Queries through it.

use crate::root::AdeApp;
use gpui::{AppContext, Context, Task};
use jerry_core::registry::{Instance, Registry, RegistryError};
use jerry_core::wire::rpc_code;
use jerry_core::{Call, Report, Request, RpcError};
use jerry_host::{AgentTable, DispatchFuture, Host, HostError, LocalClient, SessionManager};
use std::path::{Path, PathBuf};
use std::thread;

#[derive(Debug, thiserror::Error)]
pub enum HostStartError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Host(#[from] HostError),
}

pub struct HostRuntime {
    /// `None` only while `Drop` hands the host to its cleanup thread.
    host: Option<Host>,
    /// An unpublished host's dispatch loop, spawned on the first dispatch rather than at
    /// construction, so a test app that never dispatches has no extra task in flight.
    pending_dispatch: Option<DispatchFuture>,
    registry_dir: PathBuf,
    instance: Instance,
    /// Common git dirs published so far; republished as a whole when one is added.
    repos: Vec<PathBuf>,
    /// `crate::work_surface::worktree_created::spawn_consumer`'s task, started once by
    /// `AdeApp::adopt_host` - held here (rather than detached) so it is cancelled, not orphaned,
    /// when this runtime drops (§16, CLAUDE.md's entity-lifecycle rule). `None` until adoption.
    worktree_created_consumer: Option<Task<()>>,
    /// `crate::work_surface::session_exited::spawn_consumer`'s task - the same lifecycle and
    /// eager/lazy start as [`Self::worktree_created_consumer`], started alongside it at every one
    /// of its own call sites.
    session_exited_consumer: Option<Task<()>>,
}

impl HostRuntime {
    /// Blocking (registry I/O and a socket bind): call from a background task. The returned
    /// dispatch loop must be spawned by the caller, on GPUI's background executor in the app,
    /// so blocking git work never runs on the UI thread and a test drives it deterministically.
    pub fn start(registry_dir: PathBuf) -> Result<(HostRuntime, DispatchFuture), HostStartError> {
        let registry = Registry::open(registry_dir.clone())?;
        let instance = registry.allocate()?;
        let (host, dispatch) = Host::start_detached();
        host.listen(&instance.socket)?;
        let runtime = HostRuntime {
            host: Some(host),
            pending_dispatch: None,
            registry_dir,
            instance,
            repos: Vec::new(),
            worktree_created_consumer: None,
            session_exited_consumer: None,
        };
        Ok((runtime, dispatch))
    }

    pub fn start_default() -> Result<(HostRuntime, DispatchFuture), HostStartError> {
        Self::start(jerry_core::registry::runtime_dir()?)
    }

    /// A host with no socket and no registry entry: the same dispatch path, reachable only from
    /// this process. What a test app runs on, so `jerry` never finds a test instance. Its
    /// dispatch loop starts with the first dispatch.
    pub fn in_process() -> HostRuntime {
        let (host, dispatch) = Host::start_detached();
        HostRuntime {
            host: Some(host),
            pending_dispatch: Some(dispatch),
            registry_dir: PathBuf::new(),
            instance: Instance {
                name: String::new(),
                socket: PathBuf::new(),
                descriptor: PathBuf::new(),
            },
            repos: Vec::new(),
            worktree_created_consumer: None,
            session_exited_consumer: None,
        }
    }

    fn is_published(&self) -> bool {
        !self.instance.name.is_empty()
    }

    /// The slow half of publishing an already-running (`Self::in_process`) runtime: opens the
    /// registry directory (real filesystem I/O) and reserves a fresh instance name. Needs no live
    /// host reference, so it runs entirely off the UI thread; `Self::publish` does the fast
    /// remainder (the socket bind) synchronously against the runtime itself. Blocking - call from
    /// a background task.
    pub fn allocate_instance(registry_dir: PathBuf) -> Result<(PathBuf, Instance), HostStartError> {
        let registry = Registry::open(registry_dir.clone())?;
        let instance = registry.allocate()?;
        Ok((registry_dir, instance))
    }

    /// Upgrades this already-running, unpublished runtime (`Self::in_process`) to a discoverable
    /// one: binds its socket and records where it registered - the state `Self::start` builds all
    /// at once instead, for a caller with no existing runtime to upgrade (a few tests still want
    /// exactly that). A no-op, `Ok(())`, if already published - guards against a second startup
    /// attempt racing the first (decisions.md §23).
    pub fn publish(&mut self, registry_dir: PathBuf, instance: Instance) -> Result<(), HostError> {
        if self.is_published() {
            return Ok(());
        }
        let Some(host) = self.host.as_ref() else {
            // Draining toward `Drop`; nothing left to publish.
            return Ok(());
        };
        host.listen(&instance.socket)?;
        self.registry_dir = registry_dir;
        self.instance = instance;
        Ok(())
    }

    /// Records `repo_common_dir` as served and returns what a background task needs to write
    /// the descriptor: the registry is a path, so the write itself never touches the UI thread.
    pub fn serve(&mut self, repo_common_dir: PathBuf) -> (PathBuf, Instance, Vec<PathBuf>) {
        if !self.repos.contains(&repo_common_dir) {
            self.repos.push(repo_common_dir);
        }
        (
            self.registry_dir.clone(),
            self.instance.clone(),
            self.repos.clone(),
        )
    }

    /// The dispatch loop still to be spawned, if this host was built unpublished.
    pub fn take_pending_dispatch(&mut self) -> Option<DispatchFuture> {
        self.pending_dispatch.take()
    }

    pub fn client(&self) -> Option<LocalClient> {
        self.host.as_ref().map(Host::client)
    }

    pub fn agents(&self) -> Option<AgentTable> {
        self.host.as_ref().map(Host::agents)
    }

    /// The session table (`docs/architecture/decisions.md` §23): what `TerminalPane` attaches to
    /// via `SessionManager::handle_for` once its own `SessionSpawn` dispatch resolves.
    pub fn sessions(&self) -> Option<SessionManager> {
        self.host.as_ref().map(Host::sessions)
    }

    pub fn socket(&self) -> &Path {
        &self.instance.socket
    }

    /// Installs the `event/worktree-created` consumer task, replacing (and so cancelling) any
    /// earlier one - the caller runs this at most once per runtime in practice, but it stays
    /// correct even if that ever changes.
    pub(crate) fn set_worktree_created_consumer(&mut self, task: Task<()>) {
        self.worktree_created_consumer = Some(task);
    }

    /// Whether the consumer above is already running - `Self::dispatch`'s own lazy-start check.
    pub(crate) fn worktree_created_consumer_is_running(&self) -> bool {
        self.worktree_created_consumer.is_some()
    }

    /// [`Self::set_worktree_created_consumer`], for `event/session-exited` - always started
    /// alongside it, so `Self::worktree_created_consumer_is_running` is the one shared gate both
    /// consumers' call sites check.
    pub(crate) fn set_session_exited_consumer(&mut self, task: Task<()>) {
        self.session_exited_consumer = Some(task);
    }
}

impl Drop for HostRuntime {
    /// The joins and the registry unlink happen on a cleanup thread: a `HostRuntime` is an
    /// `AdeApp` field and drops on the UI thread.
    fn drop(&mut self) {
        let Some(host) = self.host.take() else {
            return;
        };
        if !self.is_published() {
            let _ = host.shutdown();
            return;
        }
        let registry_dir = self.registry_dir.clone();
        let instance = self.instance.clone();
        let spawned = thread::Builder::new()
            .name("jerry-host-cleanup".into())
            .spawn(move || {
                host.shutdown_and_join();
                if let Ok(registry) = Registry::open(registry_dir) {
                    let _ = registry.remove(&instance);
                }
            });
        if spawned.is_err() {
            // No thread to hand it to: the non-blocking shutdown still runs when `host` drops
            // here, leaving only the descriptor for the next discovery to sweep.
            log::warn!("jerry-host: could not spawn the cleanup thread; shutting down inline");
        }
    }
}

/// Writes the descriptor for `(registry_dir, instance, repos)`, as handed out by
/// [`HostRuntime::serve`]. Blocking.
fn publish(
    registry_dir: PathBuf,
    instance: Instance,
    repos: Vec<PathBuf>,
) -> Result<(), RegistryError> {
    Registry::open(registry_dir)?.publish(&instance, &repos)?;
    Ok(())
}

impl AdeApp {
    /// Publishes the host `Self::new_with_settings` already installed (`HostRuntime::in_process`,
    /// unpublished) so `jerry` can find this instance - the slow half of startup
    /// (`HostRuntime::allocate_instance`'s registry I/O) runs off the UI thread; a failure is
    /// logged and every dispatch keeps answering `NEEDS_HOST`, so nothing pretends a host exists.
    pub(crate) fn start_host(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let registry_dir = match jerry_core::registry::runtime_dir() {
                Ok(dir) => dir,
                Err(error) => {
                    log::warn!("jerry-host could not resolve its registry directory: {error}");
                    return;
                }
            };
            let allocated = cx
                .background_spawn(async move { HostRuntime::allocate_instance(registry_dir) })
                .await;
            match allocated {
                Ok((registry_dir, instance)) => {
                    let _ =
                        this.update(cx, |this, cx| this.publish_host(registry_dir, instance, cx));
                }
                Err(error) => log::warn!(
                    "jerry-host could not start; `jerry` cannot reach this instance: {error}"
                ),
            }
        })
        .detach();
    }

    /// Binds the already-running host's socket, starts the `event/worktree-created` consumer
    /// (mirroring `Self::adopt_host`'s own eager-for-published reasoning - the socket is live now,
    /// so a real agent could connect and dispatch at any moment), and publishes every repository
    /// already open, including any added while the registry work above was in flight.
    pub(crate) fn publish_host(
        &mut self,
        registry_dir: PathBuf,
        instance: Instance,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.host_runtime.as_mut() else {
            return;
        };
        if let Err(error) = runtime.publish(registry_dir, instance) {
            log::warn!(
                "jerry-host could not bind its socket; `jerry` cannot reach this instance: {error}"
            );
            return;
        }
        if let Some(client) = runtime.client() {
            let task = crate::work_surface::worktree_created::spawn_consumer(client.clone(), cx);
            runtime.set_worktree_created_consumer(task);
            let task = crate::work_surface::session_exited::spawn_consumer(client, cx);
            runtime.set_session_exited_consumer(task);
        }
        let repo_paths: Vec<PathBuf> = self.repos.iter().map(|repo| repo.path.clone()).collect();
        for path in repo_paths {
            self.serve_repo_from_host(path, cx);
        }
    }

    /// Installs a started runtime, hands the host every agent already open, and publishes
    /// every repository open right now, including any added while the host was starting.
    pub(crate) fn adopt_host(&mut self, mut runtime: HostRuntime, cx: &mut Context<Self>) {
        if let Some(agents) = runtime.agents() {
            self.agents.attach_host(agents);
        }
        // A published host's socket is already listening by this point (`HostRuntime::start`),
        // so a real agent could connect and dispatch `WorktreeCreate` at any moment - the
        // consumer starts right here, eagerly, or a real notification could arrive before
        // anything subscribes. An unpublished (test-only) host has no socket at all, so nothing
        // outside `Self::dispatch` can ever reach it; starting the consumer there instead, lazily
        // on first dispatch, mirrors `HostRuntime::pending_dispatch`'s own reasoning exactly - a
        // test app that never dispatches carries no extra pending future, which the deterministic
        // test scheduler's interleaving is sensitive to (see
        // `sidebar::render::virtualization_tests::
        // file_tree_row_and_header_actions_clear_the_real_scrollbar`, which a second such future
        // broke).
        if runtime.is_published() {
            if let Some(client) = runtime.client() {
                let task =
                    crate::work_surface::worktree_created::spawn_consumer(client.clone(), cx);
                runtime.set_worktree_created_consumer(task);
                let task = crate::work_surface::session_exited::spawn_consumer(client, cx);
                runtime.set_session_exited_consumer(task);
            }
        }
        self.host_runtime = Some(runtime);
        let repo_paths: Vec<PathBuf> = self.repos.iter().map(|repo| repo.path.clone()).collect();
        for path in repo_paths {
            self.serve_repo_from_host(path, cx);
        }
    }

    /// Adds a repository to the descriptor, off the UI thread. A no-op before the host is up
    /// (`adopt_host` publishes everything open at that moment) and for an unpublished host.
    pub(crate) fn serve_repo_from_host(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self
            .host_runtime
            .as_ref()
            .is_some_and(HostRuntime::is_published)
        {
            return;
        }
        cx.spawn(async move |this, cx| {
            let common = cx
                .background_spawn({
                    let path = path.clone();
                    async move { jerry_git::git_common_dir(&path) }
                })
                .await;
            let common = match common {
                Ok(common) => common,
                Err(error) => {
                    log::warn!("jerry-host: {} is not served: {error}", path.display());
                    return;
                }
            };
            let handed = this.update(cx, |this, _cx| {
                this.host_runtime
                    .as_mut()
                    .map(|runtime| runtime.serve(common))
            });
            let Ok(Some((dir, instance, repos))) = handed else {
                return;
            };
            if let Err(error) = cx
                .background_spawn(async move { publish(dir, instance, repos) })
                .await
            {
                log::warn!("jerry-host: could not publish {}: {error}", path.display());
            }
        })
        .detach();
    }

    /// Dispatches through the host. `cwd` is the worktree the action is requested from; the
    /// host derives the repository from it.
    pub fn dispatch(
        &mut self,
        cwd: PathBuf,
        request: Request,
        cx: &mut Context<Self>,
    ) -> Task<Result<Report, RpcError>> {
        if let Some(dispatch) = self
            .host_runtime
            .as_mut()
            .and_then(HostRuntime::take_pending_dispatch)
        {
            cx.background_spawn(dispatch).detach();
        }
        let Some(client) = self.host_runtime.as_ref().and_then(HostRuntime::client) else {
            return Task::ready(Err(RpcError::new(
                rpc_code::NEEDS_HOST,
                "the session host is not running in this instance",
            )));
        };
        // The lazy half of `Self::adopt_host`'s eager-for-published/lazy-for-test split: an
        // unpublished host's consumer starts here, on this first real dispatch, rather than at
        // adoption - see that method's own docs for why.
        let already_running = self
            .host_runtime
            .as_ref()
            .is_some_and(HostRuntime::worktree_created_consumer_is_running);
        if !already_running {
            let task = crate::work_surface::worktree_created::spawn_consumer(client.clone(), cx);
            if let Some(runtime) = self.host_runtime.as_mut() {
                runtime.set_worktree_created_consumer(task);
            }
            let task = crate::work_surface::session_exited::spawn_consumer(client.clone(), cx);
            if let Some(runtime) = self.host_runtime.as_mut() {
                runtime.set_session_exited_consumer(task);
            }
        }
        cx.spawn(async move |_this, _cx| client.request(Call::human(cwd, request)).await)
    }

    /// The session table's own in-process handle, for a caller that needs to attach to a
    /// spawned session's data-plane adapter (`SessionManager::handle_for`) rather than dispatch a
    /// Command - see `docs/architecture/decisions.md` §23. `None` only in the same narrow window
    /// `Self::dispatch`'s own `NEEDS_HOST` case covers.
    pub fn sessions(&self) -> Option<SessionManager> {
        self.host_runtime.as_ref().and_then(HostRuntime::sessions)
    }

    /// The host's own dispatch handle, for a caller (`TerminalPane::attach_session`) that needs
    /// to reach the control plane directly for something narrower than a full `Self::dispatch`
    /// call - `SessionResize` (decisions.md §23).
    pub fn host_client(&self) -> Option<LocalClient> {
        self.host_runtime.as_ref().and_then(HostRuntime::client)
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
    use super::HostRuntime;
    use crate::test_support::open_test_app;
    use gpui::{AppContext, TestAppContext};
    use jerry_core::wire::rpc_code;
    use jerry_core::{AppQuery, Report, Request};
    use std::path::PathBuf;
    use std::time::Duration;
    use test_support::{seed_empty_repo, wait_until};

    /// A registry directory of this test's own, short enough for every platform's `sun_path`,
    /// removed on drop even when the test fails.
    struct RegistryDir {
        path: PathBuf,
        _temp: Option<tempfile::TempDir>,
    }

    impl Drop for RegistryDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn registry_dir(tag: &str) -> RegistryDir {
        if cfg!(windows) {
            let path = jerry_core::registry::runtime_dir()
                .expect("runtime dir")
                .join(format!("a-{:x}-{tag}", std::process::id()));
            RegistryDir { path, _temp: None }
        } else {
            let temp = tempfile::TempDir::new().expect("tempdir");
            RegistryDir {
                path: temp.path().join("r"),
                _temp: Some(temp),
            }
        }
    }

    #[gpui::test]
    async fn the_app_dispatches_a_query_through_its_host_and_reads_the_report(
        cx: &mut TestAppContext,
    ) {
        let repo = seed_empty_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        let report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.path().to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("the in-process host answers");
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
    async fn a_published_host_removes_its_socket_when_the_runtime_is_dropped(
        cx: &mut TestAppContext,
    ) {
        let repo = seed_empty_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let dir = registry_dir("dispatch");
        let (runtime, dispatch) = HostRuntime::start(dir.path.clone()).expect("host");
        let socket = runtime.socket().to_path_buf();
        app.update(cx, |app, cx| {
            cx.background_spawn(dispatch).detach();
            app.adopt_host(runtime, cx);
        });
        assert!(socket.exists());

        app.update(cx, |app, _cx| app.host_runtime = None);
        assert!(
            wait_until(Duration::from_secs(5), || !socket.exists()),
            "dropping the runtime removes its socket from a cleanup thread"
        );
    }

    #[gpui::test]
    async fn without_a_host_a_dispatch_says_so_instead_of_pretending(cx: &mut TestAppContext) {
        let repo = seed_empty_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| app.host_runtime = None);
        let err = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.path().to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect_err("no host");
        assert_eq!(err.code, rpc_code::NEEDS_HOST);
    }
}
