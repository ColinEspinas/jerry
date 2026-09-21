//! The socket side: an accept thread, and per connection a reader that hands requests to the
//! dispatcher and a writer that drains the connection's outbox, responses and notifications
//! alike. A frame that is not JSON-RPC closes the connection; a null-id error reply is not
//! attempted, since a malformed frame cannot be trusted to carry an id at all.

use crate::{HostError, Inner};
use futures::executor::block_on;
use jerry_core::client::{Listener, Stream};
use jerry_core::wire::{read_frame, write_frame, FrameError};
use jerry_core::{Call, Message};
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

    /// Unblocks the accept thread by connecting once, joins it, and removes the socket file.
    /// The host has already raised its shutting-down flag, so the accept loop exits on wake.
    pub(crate) fn stop(mut self) {
        let _ = Stream::connect(&self.socket);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let _ = fs::remove_file(&self.socket);
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
    let mut writer_stream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(error) => {
            log::warn!("jerry-host: could not clone a connection: {error}");
            return;
        }
    };
    let (outbox, inbox) = mpsc::channel::<Message>();
    inner.fanout().subscribe_socket(outbox.clone());

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
        log::warn!("jerry-host: could not spawn a writer thread: {error}");
        return;
    }

    let reader = thread::Builder::new()
        .name("jerry-host-reader".into())
        .spawn(move || {
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
                        Err(_cancelled) => Err(jerry_core::RpcError::new(
                            jerry_core::wire::rpc_code::SHUTTING_DOWN,
                            "the host dropped the request",
                        )),
                    },
                    Err(error) => Err(error),
                };
                if outbox.send(Message::Response { id, result }).is_err() {
                    break;
                }
            }
            // Dropping the outbox ends the writer once it has drained.
        });
    if let Err(error) = reader {
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

        host.shutdown();
        assert!(!socket.path.exists(), "shutdown removes the socket file");
        assert!(
            Client::connect(&socket.path, Duration::from_millis(100)).is_err(),
            "nothing listens after shutdown"
        );
    }
}
