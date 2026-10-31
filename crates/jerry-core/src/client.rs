//! A blocking client for a host socket: one connection, one request in flight, `Report` out.
//! The listener side lives in `jerry-host`; this module only knows how to speak to it.

use crate::call::Call;
use crate::registry::Descriptor;
use crate::report::Report;
use crate::wire::PROTOCOL_VERSION;
use crate::wire::{read_frame, write_frame, FrameError, Message, RequestId, RpcError};
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
#[cfg(windows)]
pub use uds_windows::{UnixListener as Listener, UnixStream as Stream};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not connect to {}: {source}", path.display())]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the host speaks protocol version {host}, this binary speaks {ours}")]
    UnsupportedVersion { host: u32, ours: u32 },
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the host answered with an error: {} ({})", .0.message, .0.code)]
    Rpc(RpcError),
    #[error("no response within {0:?}")]
    TimedOut(Duration),
    #[error("expected the response to request {expected:?}, got {got}")]
    Mismatched { expected: RequestId, got: String },
    /// An earlier call left the stream out of step with the host; reconnect.
    #[error("this connection is out of sync after an earlier failure")]
    Broken,
    #[error("the request's params do not serialize: {0}")]
    Params(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct Client {
    stream: Stream,
    next_id: i64,
    timeout: Duration,
    /// Set once a read stopped mid-frame or answered the wrong id: whatever is on the stream
    /// now cannot be trusted, and every later call fails fast instead of reading stale bytes.
    broken: bool,
}

impl Client {
    /// `timeout` bounds every call as a whole and every single write; a host that stops
    /// answering or stops reading is an error, never a hang.
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Client, ClientError> {
        let connect_error = |source| ClientError::Connect {
            path: socket.to_path_buf(),
            source,
        };
        let stream = Stream::connect(socket).map_err(connect_error)?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(connect_error)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(connect_error)?;
        Ok(Client {
            stream,
            next_id: 1,
            timeout,
            broken: false,
        })
    }

    /// Connects to a registry entry, refusing one published by a binary of another protocol
    /// version before a single byte is sent.
    pub fn connect_to(descriptor: &Descriptor, timeout: Duration) -> Result<Client, ClientError> {
        if descriptor.protocol_version != PROTOCOL_VERSION {
            return Err(ClientError::UnsupportedVersion {
                host: descriptor.protocol_version,
                ours: PROTOCOL_VERSION,
            });
        }
        Client::connect(&descriptor.socket, timeout)
    }

    /// Sends a request and waits for its response. A JSON-RPC error is `ClientError::Rpc`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        if self.broken {
            return Err(ClientError::Broken);
        }
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        let deadline = Instant::now() + self.timeout;
        write_frame(
            &mut self.stream,
            &Message::Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            },
        )
        .map_err(|error| self.poison(error))?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.broken = true;
                return Err(ClientError::TimedOut(self.timeout));
            }
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(|source| self.poison(FrameError::Io(source)))?;
            let message = match read_frame(&mut self.stream) {
                Ok(message) => message,
                Err(FrameError::Io(error)) if is_timeout(&error) => {
                    self.broken = true;
                    return Err(ClientError::TimedOut(self.timeout));
                }
                Err(error) => return Err(self.poison(error)),
            };
            match message {
                Message::Response { id: got, result } if got == id => {
                    return result.map_err(ClientError::Rpc);
                }
                // Push events may interleave with a response; a request is idle until answered.
                Message::Notification { .. } => continue,
                other => {
                    self.broken = true;
                    return Err(ClientError::Mismatched {
                        expected: id,
                        got: describe(&other),
                    });
                }
            }
        }
    }

    /// A typed call, answered as a `Report`.
    pub fn request(&mut self, call: &Call) -> Result<Report, ClientError> {
        let value = self.call(&call.method().to_string(), call.params()?)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Fire and forget: no id, no response.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), ClientError> {
        if self.broken {
            return Err(ClientError::Broken);
        }
        write_frame(
            &mut self.stream,
            &Message::Notification {
                method: method.to_owned(),
                params,
            },
        )
        .map_err(|error| self.poison(error))
    }

    fn poison(&mut self, error: FrameError) -> ClientError {
        self.broken = true;
        ClientError::Frame(error)
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn describe(message: &Message) -> String {
    match message {
        Message::Request { id, method, .. } => format!("request {method} ({id:?})"),
        Message::Notification { method, .. } => format!("notification {method}"),
        Message::Response { id, .. } => format!("response to {id:?}"),
    }
}

#[cfg(test)]
mod client_tests {
    use super::{Client, ClientError, Listener};
    use crate::call::Call;
    use crate::registry::Descriptor;
    use crate::report::Report;
    use crate::request::{AppQuery, Request};
    use crate::wire::{read_frame, rpc_code, write_frame, Message, RpcError};
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    /// A socket path short enough for every platform's `sun_path`, removed on drop even when
    /// the test fails.
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
            let dir = crate::registry::runtime_dir().expect("runtime dir");
            std::fs::create_dir_all(&dir).expect("runtime dir exists");
            SocketPath {
                path: dir.join(format!("c-{}-{tag}.sock", std::process::id())),
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

    /// Serves one connection: answers `query/status` with an `ok` Report after pushing one
    /// notification, and anything else with METHOD_NOT_FOUND.
    fn serve_one(listener: Listener) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            loop {
                let message = match read_frame(&mut stream) {
                    Ok(message) => message,
                    Err(_) => return,
                };
                let Message::Request { id, method, .. } = message else {
                    continue;
                };
                write_frame(
                    &mut stream,
                    &Message::Notification {
                        method: "event/heartbeat".into(),
                        params: serde_json::Value::Null,
                    },
                )
                .expect("push");
                let result = if method == "query/status" {
                    Ok(serde_json::to_value(Report::Ok {
                        outcome: serde_json::json!({ "worktree_path": "/w" }),
                    })
                    .expect("report"))
                } else {
                    Err(RpcError::new(rpc_code::METHOD_NOT_FOUND, "nope"))
                };
                write_frame(&mut stream, &Message::Response { id, result }).expect("reply");
            }
        })
    }

    #[test]
    fn a_request_gets_its_report_even_when_a_push_event_arrives_first() {
        let socket = socket_path("push");
        let listener = Listener::bind(&socket.path).expect("bind");
        let server = serve_one(listener);

        let mut client = Client::connect(&socket.path, Duration::from_secs(5)).expect("connect");
        let call = Call::human("/w", Request::Query(AppQuery::Status(Default::default())));
        let report = client.request(&call).expect("answered");
        assert_eq!(
            report,
            Report::Ok {
                outcome: serde_json::json!({ "worktree_path": "/w" })
            }
        );

        let err = client
            .call("command/nope", serde_json::Value::Null)
            .expect_err("unknown method");
        assert!(matches!(err, ClientError::Rpc(ref e) if e.code == rpc_code::METHOD_NOT_FOUND));

        drop(client);
        server
            .join()
            .expect("server exits when the client hangs up");
    }

    #[test]
    fn a_silent_host_times_out_the_whole_call_and_poisons_the_connection() {
        let socket = socket_path("silent");
        let listener = Listener::bind(&socket.path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            // Hold the connection open without ever answering, then let the test end it.
            thread::sleep(Duration::from_millis(600));
            drop(stream);
        });

        let mut client =
            Client::connect(&socket.path, Duration::from_millis(200)).expect("connect");
        let err = client
            .call("query/status", serde_json::Value::Null)
            .expect_err("nobody answers");
        assert!(matches!(err, ClientError::TimedOut(_)), "{err}");
        let err = client
            .call("query/status", serde_json::Value::Null)
            .expect_err("a poisoned client fails fast");
        assert!(matches!(err, ClientError::Broken), "{err}");
        server.join().expect("server thread");
    }

    #[test]
    fn connecting_to_nothing_is_a_typed_error_not_a_hang() {
        let socket = socket_path("nobody");
        let err =
            Client::connect(&socket.path, Duration::from_millis(100)).expect_err("nobody listens");
        assert!(matches!(err, ClientError::Connect { .. }), "{err}");
    }

    #[test]
    fn a_descriptor_from_another_protocol_version_is_refused_before_connecting() {
        let descriptor = Descriptor {
            protocol_version: crate::wire::PROTOCOL_VERSION + 1,
            pid: 1,
            started_at: 0,
            repos: Vec::new(),
            socket: PathBuf::from("/nowhere.sock"),
        };
        let err = Client::connect_to(&descriptor, Duration::from_millis(100))
            .expect_err("version mismatch");
        assert!(
            matches!(err, ClientError::UnsupportedVersion { host, ours } if host == ours + 1),
            "{err}"
        );
    }
}
