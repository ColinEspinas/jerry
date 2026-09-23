//! The session host: the one place a `Call` is authorized and executed. Owns the dispatch
//! thread, the socket listener, the table of agents it spawned, and the notification fan-out.
//! Every task here wakes on a channel, never on a timer. Sessions and the hook store are not
//! here yet; a request needing them is answered `NEEDS_HOST`. Zero `gpui`.

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod dispatch;
mod fanout;
mod listener;

use futures::channel::{mpsc, oneshot};
use jerry_core::client::Stream;
use jerry_core::wire::rpc_code;
use jerry_core::{AgentId, Call, Message, Report, RpcError};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

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
}

/// One agent this host is tracking: the worktree it's confined to, and which CLI it runs -
/// `kind` is a free-form label (`jerry-app`'s own `AgentKind::label()`, e.g. `"Claude"`), never
/// interpreted here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub worktree: PathBuf,
    pub kind: String,
}

/// The agents this host spawned, by the identity it injected as `JERRY_AGENT_ID`. Shared by the
/// app, which registers and forgets, and the dispatcher, which classifies callers against it and
/// answers `AgentsQuery` from it.
#[derive(Clone, Default)]
pub struct AgentTable(Arc<Mutex<HashMap<AgentId, AgentRecord>>>);

impl AgentTable {
    pub fn register(&self, id: AgentId, worktree: PathBuf, kind: String) {
        lock(&self.0).insert(id, AgentRecord { worktree, kind });
    }

    pub fn forget(&self, id: &AgentId) {
        lock(&self.0).remove(id);
    }

    pub fn worktree_of(&self, id: &AgentId) -> Option<PathBuf> {
        lock(&self.0).get(id).map(|record| record.worktree.clone())
    }

    pub fn len(&self) -> usize {
        lock(&self.0).len()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.0).is_empty()
    }

    /// Every agent this host is tracking right now, for `AgentsQuery` - the dispatcher's own
    /// answer for a request `execute_locally` can never resolve on its own (§15).
    pub fn list(&self) -> Vec<(AgentId, AgentRecord)> {
        lock(&self.0)
            .iter()
            .map(|(id, record)| (id.clone(), record.clone()))
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
    fanout: fanout::Fanout,
    /// One handle per live socket connection, so shutdown can close them under their threads.
    connections: Mutex<Vec<Stream>>,
    shutting_down: AtomicBool,
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

    pub(crate) fn fanout(&self) -> &fanout::Fanout {
        &self.fanout
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub(crate) fn track_connection(&self, stream: Stream) {
        lock(&self.connections).push(stream);
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
}

impl Host {
    /// Starts the host with its own dispatch thread, for a process that has no executor of
    /// its own (the standalone host binary, tests).
    pub fn start() -> Result<Host, HostError> {
        let (host, dispatch) = Host::start_detached();
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
        let (host, dispatch) = Host::start_detached();
        spawn(dispatch);
        host
    }

    /// Builds the host and hands back its dispatch loop for the caller to spawn wherever it
    /// lives: the app puts it on GPUI's background executor, and a GPUI test drives it
    /// deterministically. Blocking git work runs wherever the loop runs.
    pub fn start_detached() -> (Host, DispatchFuture) {
        let (jobs, mut receiver) = mpsc::unbounded::<Job>();
        let inner = Arc::new(Inner {
            jobs: Mutex::new(Some(jobs)),
            agents: AgentTable::default(),
            fanout: fanout::Fanout::default(),
            connections: Mutex::new(Vec::new()),
            shutting_down: AtomicBool::new(false),
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
        };
        (host, dispatch)
    }

    pub fn agents(&self) -> AgentTable {
        self.inner.agents.clone()
    }

    pub fn client(&self) -> LocalClient {
        LocalClient {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Accepts socket clients at `socket`. The registry descriptor should be published only
    /// after this returns, so a discoverable entry always has a listener behind it.
    pub fn listen(&self, socket: &Path) -> Result<(), HostError> {
        let mut listening = lock(&self.listening);
        if let Some(existing) = &*listening {
            return Err(HostError::AlreadyListening {
                path: existing.socket().to_path_buf(),
            });
        }
        *listening = Some(listener::listen(Arc::clone(&self.inner), socket)?);
        Ok(())
    }

    /// Stops the host without waiting for its threads: queued and future calls answer
    /// `SHUTTING_DOWN`, the socket file is removed, every connection is closed, and the accept
    /// and dispatch threads exit on their own. Safe on a UI thread. Idempotent.
    pub fn shutdown(&self) -> ShutdownHandles {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        lock(&self.inner.jobs).take();
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
        match self.inner.submit(call).await {
            Ok(result) => result,
            Err(_cancelled) => Err(RpcError::new(
                rpc_code::SHUTTING_DOWN,
                "the host dropped the request",
            )),
        }
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
}
