//! The app's side of the session host: brings `jerry_host::Host` up in this process, publishes
//! one registry descriptor listing every open repository, hands agents to the host's table, and
//! dispatches Commands and Queries through it.

use crate::root::AdeApp;
use gpui::{AppContext, Context, Task};
use jerry_core::registry::{Instance, Registry, RegistryError};
use jerry_core::wire::rpc_code;
use jerry_core::{Call, Report, Request, RpcError};
use jerry_host::{AgentTable, Host, HostError, LocalClient};
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
    registry_dir: PathBuf,
    instance: Instance,
    /// Common git dirs published so far; republished as a whole when one is added.
    repos: Vec<PathBuf>,
}

impl HostRuntime {
    /// Blocking (registry I/O and a socket bind): call from a background task.
    pub fn start(registry_dir: PathBuf) -> Result<HostRuntime, HostStartError> {
        let registry = Registry::open(registry_dir.clone())?;
        let instance = registry.allocate()?;
        let host = Host::start()?;
        host.listen(&instance.socket)?;
        Ok(HostRuntime {
            host: Some(host),
            registry_dir,
            instance,
            repos: Vec::new(),
        })
    }

    pub fn start_default() -> Result<HostRuntime, HostStartError> {
        Self::start(jerry_core::registry::runtime_dir()?)
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

    pub fn client(&self) -> Option<LocalClient> {
        self.host.as_ref().map(Host::client)
    }

    pub fn agents(&self) -> Option<AgentTable> {
        self.host.as_ref().map(Host::agents)
    }

    pub fn socket(&self) -> &Path {
        &self.instance.socket
    }
}

impl Drop for HostRuntime {
    /// The joins and the registry unlink happen on a cleanup thread: a `HostRuntime` is an
    /// `AdeApp` field and drops on the UI thread.
    fn drop(&mut self) {
        let Some(host) = self.host.take() else {
            return;
        };
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
    /// Brings the host up off the UI thread. Every repository open once it is up is published
    /// then; a failure is logged and every dispatch answers `NEEDS_HOST`, so nothing pretends
    /// a host exists.
    pub(crate) fn start_host(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_spawn(async move { HostRuntime::start_default() })
                .await;
            match started {
                Ok(runtime) => {
                    let _ = this.update(cx, |this, cx| this.adopt_host(runtime, cx));
                }
                Err(error) => log::warn!(
                    "jerry-host could not start; `jerry` cannot reach this instance: {error}"
                ),
            }
        })
        .detach();
    }

    /// Installs a started runtime, hands the host every agent already open, and publishes
    /// every repository open right now, including any added while the host was starting.
    pub(crate) fn adopt_host(&mut self, runtime: HostRuntime, cx: &mut Context<Self>) {
        if let Some(agents) = runtime.agents() {
            self.agents.attach_host(agents);
        }
        self.host_runtime = Some(runtime);
        let repo_paths: Vec<PathBuf> = self.repos.iter().map(|repo| repo.path.clone()).collect();
        for path in repo_paths {
            self.serve_repo_from_host(path, cx);
        }
    }

    /// Adds a repository to the descriptor, off the UI thread. A no-op before the host is up:
    /// `adopt_host` publishes everything open at that moment.
    pub(crate) fn serve_repo_from_host(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.host_runtime.is_none() {
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
        &self,
        cwd: PathBuf,
        request: Request,
        cx: &mut Context<Self>,
    ) -> Task<Result<Report, RpcError>> {
        let Some(client) = self.host_runtime.as_ref().and_then(HostRuntime::client) else {
            return Task::ready(Err(RpcError::new(
                rpc_code::NEEDS_HOST,
                "the session host is not running in this instance",
            )));
        };
        cx.spawn(async move |_this, _cx| client.request(Call::human(cwd, request)).await)
    }
}

#[cfg(test)]
mod app_dispatch_tests {
    use super::HostRuntime;
    use crate::test_support::open_test_app;
    use gpui::TestAppContext;
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
        // The host answers from a real thread, which the deterministic test scheduler must be
        // allowed to wait for.
        cx.executor().allow_parking();
        let repo = seed_empty_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let dir = registry_dir("dispatch");
        let runtime = HostRuntime::start(dir.path.clone()).expect("host");
        let socket = runtime.socket().to_path_buf();
        app.update(cx, |app, cx| app.adopt_host(runtime, cx));

        let report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.path().to_path_buf(),
                    Request::Query(AppQuery::Status(Default::default())),
                    cx,
                )
            })
            .await
            .expect("the host answers");
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
