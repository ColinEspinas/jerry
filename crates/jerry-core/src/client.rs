//! A blocking client for a host socket: one connection, one request in flight, `Report` out.
//! The listener side lives in `jerry-host`; this module only knows how to speak to it.

use crate::report::Report;
use crate::request::Request;
use crate::wire::{read_frame, write_frame, FrameError, Message, RequestId, RpcError};
use serde_json::Value;
use std::io;
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
#[cfg(windows)]
pub use uds_windows::{UnixListener as Listener, UnixStream as Stream};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not connect to {}: {source}", path.display())]
    Connect {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the host answered with an error: {} ({})", .0.message, .0.code)]
    Rpc(RpcError),
    #[error("expected the response to request {expected:?}, got {got}")]
    Mismatched { expected: RequestId, got: String },
    #[error("the request's params do not serialize: {0}")]
    Params(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct Client {
    stream: Stream,
    next_id: i64,
}

impl Client {
    /// `read_timeout` bounds every wait for a response; a host that stops answering is an
    /// error, never a hang.
    pub fn connect(socket: &Path, read_timeout: Duration) -> Result<Client, ClientError> {
        let stream = Stream::connect(socket).map_err(|source| ClientError::Connect {
            path: socket.to_path_buf(),
            source,
        })?;
        stream
            .set_read_timeout(Some(read_timeout))
            .map_err(|source| ClientError::Connect {
                path: socket.to_path_buf(),
                source,
            })?;
        Ok(Client { stream, next_id: 1 })
    }

    /// Sends a request and waits for its response. A JSON-RPC error is `ClientError::Rpc`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        write_frame(
            &mut self.stream,
            &Message::Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            },
        )?;
        loop {
            match read_frame(&mut self.stream)? {
                Message::Response { id: got, result } if got == id => {
                    return result.map_err(ClientError::Rpc);
                }
                // Push events may interleave with a response; a request is idle until answered.
                Message::Notification { .. } => continue,
                other => {
                    return Err(ClientError::Mismatched {
                        expected: id,
                        got: describe(&other),
                    })
                }
            }
        }
    }

    /// A typed request, answered as a `Report`.
    pub fn request(&mut self, request: &Request) -> Result<Report, ClientError> {
        let value = self.call(&request.method().to_string(), request.params()?)?;
        Ok(serde_json::from_value(value)?)
    }

    /// Fire and forget: no id, no response.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), ClientError> {
        write_frame(
            &mut self.stream,
            &Message::Notification {
                method: method.to_owned(),
                params,
            },
        )?;
        Ok(())
    }
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
    use crate::report::Report;
    use crate::request::{AppQuery, Request};
    use crate::wire::{read_frame, rpc_code, write_frame, Message, RpcError};
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    /// A socket path short enough for every platform's `sun_path`.
    fn socket_path(tag: &str) -> (PathBuf, Option<tempfile::TempDir>) {
        if cfg!(windows) {
            let dir = crate::registry::runtime_dir().expect("runtime dir");
            std::fs::create_dir_all(&dir).expect("runtime dir exists");
            (
                dir.join(format!("c-{}-{tag}.sock", std::process::id())),
                None,
            )
        } else {
            let temp = tempfile::TempDir::new().expect("tempdir");
            (temp.path().join(format!("{tag}.sock")), Some(temp))
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
        let (path, _keep) = socket_path("push");
        let listener = Listener::bind(&path).expect("bind");
        let server = serve_one(listener);

        let mut client = Client::connect(&path, Duration::from_secs(5)).expect("connect");
        let request = Request::Query(AppQuery::Status(Default::default()));
        let report = client.request(&request).expect("answered");
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
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connecting_to_nothing_is_a_typed_error_not_a_hang() {
        let (path, _keep) = socket_path("nobody");
        let err = Client::connect(&path, Duration::from_millis(100)).expect_err("nobody listens");
        assert!(matches!(err, ClientError::Connect { .. }), "{err}");
    }
}
