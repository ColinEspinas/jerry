//! [`RemoteRepoHost`]: the safe bridge between a real, out-of-process `jerry-host` connection
//! (`jerry_core::client::Client`, blocking, one call in flight - decisions.md §24) and GPUI's
//! cooperative scheduler. A dedicated OS thread owns the one `Client`; every dispatch is a plain
//! blocking round trip over `std::sync::mpsc`, safe to call from anywhere - a `cx.background_spawn`
//! task, a bare OS thread, or synchronously - because that worker thread always progresses on its
//! own, never as a task GPUI's own scheduler must poll forward (decisions.md §16's amendment: a
//! `futures::executor::block_on` over an *executor-driven* future deadlocks GPUI's single-threaded
//! test scheduler; blocking on a plain `std::sync::mpsc` reply from an independent thread does not).
//! Owns nothing about *which* repository or session this connection is for - `crate::host` is
//! where that bookkeeping lives.
//!
//! Real and independently tested (below, including the exact GPUI-scheduler deadlock this module
//! exists to make impossible), but not yet wired into `crate::host::Hosts`/`AdeApp::dispatch`.
//! That per-repository bookkeeping (`Hosts { by_repo: HashMap<PathBuf, RepoHost> }`, the
//! cwd-to-common-dir cache, per-repo event consumers, `HookRuntime`'s own simplification, the
//! error-banner UI, and `SocketSessionAdapter`'s production wiring through this) is its own,
//! larger follow-up; `docs/architecture/decisions.md` §24 has the exact remaining scope.
#![allow(dead_code)]

use jerry_core::client::{Client, ClientError};
use jerry_core::wire::rpc_code;
use jerry_core::{Call, Report, RpcError};
use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread;

struct RequestJob {
    call: Call,
    reply: std_mpsc::Sender<Result<Report, RpcError>>,
}

/// A live connection to one repository's real session host. Cheap to clone: every clone shares
/// the same worker thread and its one `Client`.
#[derive(Clone)]
pub(crate) struct RemoteRepoHost {
    requests: std_mpsc::Sender<RequestJob>,
}

impl RemoteRepoHost {
    /// Spawns the worker thread that owns `client` for as long as any clone of the returned
    /// handle lives - dropped once every clone (and so every `requests` sender) is gone, which
    /// ends the thread's `for job in rx` loop and closes the connection.
    pub(crate) fn new(client: Client) -> io::Result<Self> {
        let (requests, jobs) = std_mpsc::channel::<RequestJob>();
        thread::Builder::new()
            .name("jerry-app-repo-host-requests".into())
            .spawn(move || {
                let mut client = client;
                for job in jobs {
                    let result = client.request(&job.call).map_err(client_error_to_rpc);
                    // The caller may have given up waiting (a dropped `reply_rx`); nothing to do.
                    let _ = job.reply.send(result);
                }
            })?;
        Ok(RemoteRepoHost { requests })
    }

    /// Dispatches `call` and blocks the calling thread for its reply. Safe from any context (see
    /// the module docs) - the caller decides whether that means a `cx.background_spawn` task
    /// (`crate::host::AdeApp::dispatch`) or a bare OS thread (a `SessionAdapter`'s own kill path).
    pub(crate) fn dispatch(&self, call: Call) -> Result<Report, RpcError> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        if self
            .requests
            .send(RequestJob {
                call,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(worker_gone());
        }
        reply_rx.recv().unwrap_or_else(|_| Err(worker_gone()))
    }
}

fn worker_gone() -> RpcError {
    RpcError::new(
        rpc_code::INTERNAL_ERROR,
        "this repository's request worker thread has stopped",
    )
}

fn client_error_to_rpc(error: ClientError) -> RpcError {
    match error {
        ClientError::TimedOut(_) => RpcError::new(rpc_code::TIMED_OUT, error.to_string()),
        ClientError::UnsupportedVersion { .. } => {
            RpcError::new(rpc_code::UNSUPPORTED_VERSION, error.to_string())
        }
        other => RpcError::new(rpc_code::INTERNAL_ERROR, other.to_string()),
    }
}

#[cfg(test)]
mod remote_repo_host_tests {
    use super::RemoteRepoHost;
    use jerry_core::client::Client;
    use jerry_core::{AppQuery, Call, Report, Request};
    use jerry_host::Host;
    use std::path::PathBuf;
    use std::time::Duration;
    use test_support::seed_empty_repo;

    /// A socket path short enough for every platform, removed on drop.
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
            std::fs::create_dir_all(&dir).expect("runtime dir exists");
            SocketPath {
                path: dir.join(format!("rh-{}-{tag}.sock", std::process::id())),
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

    #[test]
    fn a_dispatch_reaches_a_real_host_and_returns_its_real_report() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let socket = socket_path("dispatch");
        host.listen(&socket.path).expect("listen");
        let client = Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
        let repo_host = RemoteRepoHost::new(client).expect("worker thread");

        let report = repo_host
            .dispatch(Call::human(
                repo.path(),
                Request::Query(AppQuery::Status(Default::default())),
            ))
            .expect("the real host answers");
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
        host.shutdown_and_join();
    }

    /// Two distinct repositories, each served by its own real host process (in this test, two
    /// in-process `Host`s standing in for two real `jerry-host` binaries - `RemoteRepoHost` itself
    /// cannot tell the difference, since both are reached the identical way, over a real socket):
    /// dispatching through each `RemoteRepoHost` must reach *that* repository's own host, never
    /// the other one's.
    #[test]
    fn two_repo_hosts_each_reach_their_own_independent_process() {
        let repo_a = seed_empty_repo();
        let repo_b = seed_empty_repo();

        let host_a = Host::start().expect("host a");
        let socket_a = socket_path("repo-a");
        host_a.listen(&socket_a.path).expect("listen a");
        let client_a = Client::connect(&socket_a.path, Duration::from_secs(5)).expect("connect a");
        let repo_host_a = RemoteRepoHost::new(client_a).expect("worker thread a");

        let host_b = Host::start().expect("host b");
        let socket_b = socket_path("repo-b");
        host_b.listen(&socket_b.path).expect("listen b");
        let client_b = Client::connect(&socket_b.path, Duration::from_secs(5)).expect("connect b");
        let repo_host_b = RemoteRepoHost::new(client_b).expect("worker thread b");

        let agent_a = jerry_core::AgentId::from("agent-a");
        host_a.agents().register(
            agent_a.clone(),
            repo_a.path().to_path_buf(),
            "Claude".into(),
        );
        let agent_b = jerry_core::AgentId::from("agent-b");
        host_b.agents().register(
            agent_b.clone(),
            repo_b.path().to_path_buf(),
            "Claude".into(),
        );

        let report_a = repo_host_a
            .dispatch(Call::human(
                repo_a.path(),
                Request::Query(AppQuery::Agents(Default::default())),
            ))
            .expect("host a answers");
        let Report::Ok { outcome: outcome_a } = report_a else {
            panic!("expected ok from host a")
        };
        let ids_a: Vec<String> = outcome_a
            .as_array()
            .expect("array")
            .iter()
            .map(|entry| entry["id"].as_str().expect("id").to_owned())
            .collect();
        assert_eq!(
            ids_a,
            vec!["agent-a"],
            "host a must list only its own agent"
        );

        let report_b = repo_host_b
            .dispatch(Call::human(
                repo_b.path(),
                Request::Query(AppQuery::Agents(Default::default())),
            ))
            .expect("host b answers");
        let Report::Ok { outcome: outcome_b } = report_b else {
            panic!("expected ok from host b")
        };
        let ids_b: Vec<String> = outcome_b
            .as_array()
            .expect("array")
            .iter()
            .map(|entry| entry["id"].as_str().expect("id").to_owned())
            .collect();
        assert_eq!(
            ids_b,
            vec!["agent-b"],
            "host b must list only its own agent"
        );

        host_a.shutdown_and_join();
        host_b.shutdown_and_join();
    }

    #[test]
    fn a_worker_thread_that_has_stopped_answers_a_typed_error_not_a_hang() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let socket = socket_path("stopped");
        host.listen(&socket.path).expect("listen");
        let client = Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
        let repo_host = RemoteRepoHost::new(client).expect("worker thread");

        host.shutdown_and_join();
        // The worker thread's own `client.request` now fails (the connection is gone) rather
        // than the send to it failing - still a real, typed error, not a hang.
        let err = repo_host
            .dispatch(Call::human(
                repo.path(),
                Request::Query(AppQuery::Status(Default::default())),
            ))
            .expect_err("the host is gone");
        assert_ne!(err.message, "");
    }

    /// The regression this module exists to make impossible: a synchronous, blocking dispatch
    /// (`SocketSessionAdapter::shutdown`'s own real shape, `crate::terminal::socket_adapter`) run
    /// from *inside* a `cx.background_executor().spawn` task, under GPUI's single-threaded test
    /// scheduler. The wrongly-shaped predecessor of this module blocked on a `jerry_host::
    /// LocalClient` request instead of a real `Client`/worker thread - a future that could only
    /// resolve once *another* task on that same single thread was polled forward, which the
    /// blocked poll call could never do, so `run_until_parked` hung forever
    /// (`docs/architecture/decisions.md` §16's amendment has the full mechanism). This dispatches
    /// through a real worker thread instead, so the block is on a plain `std::sync::mpsc` reply
    /// from an independent thread - safe regardless of which thread calls it.
    #[gpui::test]
    fn dispatch_is_safe_from_inside_a_background_executor_task_under_the_test_scheduler(
        cx: &mut gpui::TestAppContext,
    ) {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let socket = socket_path("gpui-safety");
        host.listen(&socket.path).expect("listen");
        let client = Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
        let repo_host = RemoteRepoHost::new(client).expect("worker thread");

        let repo_path = repo.path().to_path_buf();
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
        let outcome_writer = std::sync::Arc::clone(&outcome);
        cx.update(|cx| {
            cx.background_executor()
                .spawn(async move {
                    // The exact shape this test exists to prove safe: a plain, synchronous call
                    // (no `.await`, no `futures::executor::block_on`) inside a task's own body,
                    // blocking on a reply from a thread GPUI's scheduler does not own.
                    let report = repo_host.dispatch(Call::human(
                        repo_path,
                        Request::Query(AppQuery::Status(Default::default())),
                    ));
                    *outcome_writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
                })
                .detach();
        });

        // If this hangs, nextest's own slow-timeout kills the test rather than the whole suite -
        // the same failure mode the predecessor design reliably reproduced.
        cx.run_until_parked();

        let report = outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("the background task ran to completion, not left pending");
        assert!(matches!(report, Ok(Report::Ok { .. })), "{report:?}");

        host.shutdown_and_join();
    }
}
