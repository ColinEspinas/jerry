//! The socket side: an accept thread, and per connection a reader that hands requests to the
//! dispatcher and a writer that drains the connection's bounded outbox, responses and
//! notifications alike. A frame that is not JSON-RPC closes the connection; a null-id error
//! reply is not attempted, since a malformed frame cannot be trusted to carry an id at all.

use crate::fanout::SOCKET_BACKLOG;
use crate::{HostError, Inner};
use futures::executor::block_on;
use jerry_core::client::{Listener, Stream};
use jerry_core::wire::{read_frame, rpc_code, write_frame, FrameError};
use jerry_core::{Call, Message, RpcError};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

pub(crate) struct Listening {
    socket: PathBuf,
    accept: Option<JoinHandle<()>>,
}

impl Listening {
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Wakes the accept loop by connecting once and removes the socket file. Never blocks:
    /// the accept thread exits on its own once woken, and every connection thread ends when
    /// `Inner::close_connections` shuts its socket.
    pub(crate) fn stop(&mut self) -> Option<JoinHandle<()>> {
        let _ = Stream::connect(&self.socket);
        let _ = fs::remove_file(&self.socket);
        self.accept.take()
    }
}

pub(crate) fn listen(inner: Arc<Inner>, socket: &Path) -> Result<Listening, HostError> {
    let listener = Listener::bind(socket).map_err(|source| HostError::Bind {
        path: socket.to_path_buf(),
        source,
    })?;
    let accept = thread::Builder::new()
        .name("jerry-host-accept".into())
        .spawn(move || {
            for connection in listener.incoming() {
                if inner.is_shutting_down() {
                    break;
                }
                match connection {
                    Ok(stream) => serve(Arc::clone(&inner), stream),
                    Err(error) => log::warn!("jerry-host: accept failed: {error}"),
                }
            }
        })
        .map_err(|source| HostError::Thread {
            name: "accept",
            source,
        })?;
    Ok(Listening {
        socket: socket.to_path_buf(),
        accept: Some(accept),
    })
}

fn serve(inner: Arc<Inner>, stream: Stream) {
    let (mut writer_stream, tracked) = match (stream.try_clone(), stream.try_clone()) {
        (Ok(writer), Ok(tracked)) => (writer, tracked),
        (Err(error), _) | (_, Err(error)) => {
            log::warn!("jerry-host: could not clone a connection: {error}");
            return;
        }
    };
    // The host keeps a handle so shutdown can close the socket under both threads.
    inner.track_connection(tracked);
    let (outbox, inbox) = mpsc::sync_channel::<Message>(SOCKET_BACKLOG);
    let sink = inner.fanout().subscribe_socket(outbox.clone());

    let writer = thread::Builder::new()
        .name("jerry-host-writer".into())
        .spawn(move || {
            for message in inbox {
                if let Err(error) = write_frame(&mut writer_stream, &message) {
                    log::debug!("jerry-host: connection write ended: {error}");
                    break;
                }
            }
        });
    if let Err(error) = writer {
        inner.fanout().unsubscribe(sink);
        log::warn!("jerry-host: could not spawn a writer thread: {error}");
        return;
    }

    let reader_inner = Arc::clone(&inner);
    let reader = thread::Builder::new()
        .name("jerry-host-reader".into())
        .spawn(move || {
            let inner = reader_inner;
            let mut stream = stream;
            loop {
                let message = match read_frame(&mut stream) {
                    Ok(message) => message,
                    Err(FrameError::Closed) => break,
                    Err(error) => {
                        log::debug!("jerry-host: connection read ended: {error}");
                        break;
                    }
                };
                let Message::Request { id, method, params } = message else {
                    // Clients do not push to the host in protocol version 1.
                    continue;
                };
                let result = match Call::from_wire(&method, params) {
                    Ok(call) => match block_on(inner.submit(call)) {
                        Ok(result) => result,
                        Err(_cancelled) => Err(RpcError::new(
                            rpc_code::SHUTTING_DOWN,
                            "the host dropped the request",
                        )),
                    },
                    Err(error) => Err(error),
                };
                // A response is never dropped for a full backlog: block until the writer
                // takes it, or stop if the writer is gone.
                if outbox.send(Message::Response { id, result }).is_err() {
                    break;
                }
            }
            // Both senders must go for the writer to end: this one, and the fanout's.
            inner.fanout().unsubscribe(sink);
        });
    if let Err(error) = reader {
        inner.fanout().unsubscribe(sink);
        log::warn!("jerry-host: could not spawn a reader thread: {error}");
    }
}

#[cfg(test)]
mod socket_tests {
    use crate::Host;
    use jerry_core::client::{Client, Stream};
    use jerry_core::request::HookEvent;
    use jerry_core::wire::read_frame;
    use jerry_core::{AgentId, AppQuery, Call, Message, Request};
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
            std::fs::create_dir_all(&dir).expect("runtime dir");
            SocketPath {
                path: dir.join(format!("h-{}-{tag}.sock", std::process::id())),
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
    fn a_socket_client_gets_reports_and_a_second_connection_sees_the_push_events() {
        let repo = seed_empty_repo();
        let host = Host::start().expect("host");
        let socket = socket_path("serve");
        host.listen(&socket.path).expect("listen");
        let id = AgentId::from("agent-9");
        host.agents()
            .register(id.clone(), repo.path().to_path_buf());

        // A bare connection that only listens, opened before the event is produced.
        let mut watcher = Stream::connect(&socket.path).expect("watcher connects");
        watcher
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");

        let mut client = Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
        let report = client
            .request(&Call::human(
                repo.path(),
                Request::Query(AppQuery::Status(Default::default())),
            ))
            .expect("status");
        assert!(report.is_ok(), "{report:?}");

        let hook = Request::Hook(HookEvent {
            event: "PostToolUse".into(),
            payload: serde_json::json!({ "tool_name": "Edit" }),
        });
        let report = client
            .request(&Call::agent(repo.path(), id, hook))
            .expect("hook accepted");
        assert!(report.is_ok(), "{report:?}");

        let pushed = read_frame(&mut watcher).expect("the watcher receives the event");
        assert!(
            matches!(pushed, Message::Notification { ref method, .. } if method == "event/hook"),
            "{pushed:?}"
        );

        host.shutdown_and_join();
        assert!(!socket.path.exists(), "shutdown removes the socket file");
        assert!(
            Client::connect(&socket.path, Duration::from_millis(100)).is_err(),
            "nothing listens after shutdown"
        );
        assert!(
            read_frame(&mut watcher).is_err(),
            "shutdown closes the connections that were still open"
        );
    }
}
