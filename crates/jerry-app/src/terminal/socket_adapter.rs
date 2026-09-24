//! [`SocketSessionAdapter`]: [`crate::terminal::pane::SessionAdapter`] over a session's real
//! data-plane socket (`command/session-attach`, `docs/architecture/decisions.md` §24) - the
//! second implementer of that trait besides `jerry_host::SessionHandle`'s in-process one. Never
//! calls `jerry_host::`/`jerry_pty::` beyond the trait itself: everything here is a plain socket
//! read/write plus one injected callback for `command/session-kill`
//! ([`SocketSessionAdapter::shutdown`]) - see decisions.md §16's amendment for why that closure
//! must dispatch through `crate::repo_host::RemoteRepoHost`, never `jerry_host::LocalClient`. No
//! exit signal is synthesized: the socket just ends once the host closes it. Wired into
//! production by `crate::host::attach_remote_session`.

use futures::channel::mpsc;
use futures::SinkExt;
use jerry_core::SessionId;
use jerry_pty::{PtyError, PtyOutput};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;

use jerry_core::client::Stream;

use crate::terminal::pane::SessionAdapter;

/// How many not-yet-consumed items [`SocketSessionAdapter`]'s reader thread may queue before it
/// blocks - the same shape `jerry_host`'s own relay channels use.
const OUTPUT_CHANNEL_CAPACITY: usize = 256;

pub(crate) struct SocketSessionAdapter {
    id: SessionId,
    /// [`Self::write_input`]'s own non-blocking enqueue - an unbounded channel a dedicated writer
    /// thread drains onto the real socket, mirroring `jerry_pty::PtySession::write_input`'s own
    /// documented contract exactly (`SessionAdapter::write_input`'s docs): a caller on the GPUI
    /// foreground thread must never block on a slow or stalled peer. A direct, synchronous socket
    /// write here was this module's first, wrong draft - it blocked the pane's own output task
    /// (itself running as part of the same foreground poll `TestAppContext::run_until_parked`
    /// drives) answering a cursor-position report back to the pty, and deadlocked every UI test
    /// that spawns a real session and lets it reach an interactive prompt.
    input_tx: std_mpsc::Sender<Vec<u8>>,
    /// A clone of the connection, held only for [`Self::shutdown`] to close - the writer thread
    /// owns the one it actually writes through.
    shutdown_stream: Mutex<Stream>,
    output: Mutex<Option<mpsc::Receiver<PtyOutput>>>,
    /// From `SessionsQuery`, resolved by the caller before construction - this type dispatches
    /// nothing itself, so a value that can only come from a Query is given, not fetched (see the
    /// module docs).
    process_id: Option<u32>,
    /// `command/session-kill`, dispatched by the caller-supplied closure so this module never
    /// depends on `jerry_core::client`/`HostRuntime` directly - only on the one string a failure
    /// reports. Blocking, matching `SessionHandle::shutdown`'s own contract.
    kill: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl SocketSessionAdapter {
    /// Connects to `socket` (a `command/session-attach` outcome) and starts relaying its bytes as
    /// [`PtyOutput::Bytes`] items - channel-woken, never polled. `kill` is called exactly once, by
    /// [`Self::shutdown`], to dispatch `command/session-kill` before this adapter closes its own
    /// connection.
    pub(crate) fn connect(
        socket: &Path,
        id: SessionId,
        process_id: Option<u32>,
        kill: impl Fn() -> Result<(), String> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let stream = Stream::connect(socket)?;
        let mut reader = stream.try_clone()?;
        let mut writer = stream.try_clone()?;
        let shutdown_stream = stream;

        let (mut tx, rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
        thread::Builder::new()
            .name("jerry-app-session-data-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = PtyOutput::Bytes(buf[..n].to_vec());
                            if futures::executor::block_on(tx.send(chunk)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // The host closed the connection (a real disconnect, or the session's own exit -
                // see the module docs for why no synthetic `Exited` is sent here).
            })?;

        let (input_tx, input_rx) = std_mpsc::channel::<Vec<u8>>();
        thread::Builder::new()
            .name("jerry-app-session-data-writer".into())
            .spawn(move || {
                for chunk in input_rx {
                    if writer.write_all(&chunk).is_err() {
                        break;
                    }
                }
            })?;

        Ok(SocketSessionAdapter {
            id,
            input_tx,
            shutdown_stream: Mutex::new(shutdown_stream),
            output: Mutex::new(Some(rx)),
            process_id,
            kill: Box::new(kill),
        })
    }
}

impl SessionAdapter for SocketSessionAdapter {
    fn id(&self) -> &SessionId {
        &self.id
    }

    /// Enqueues onto the writer thread; never blocks on the socket itself (see
    /// [`Self::input_tx`]'s own docs). The one real, honest failure is the writer thread having
    /// already stopped (the peer closed its own read side).
    fn write_input(&self, data: &[u8]) -> Result<(), PtyError> {
        self.input_tx
            .send(data.to_vec())
            .map_err(|_| PtyError::Remote("the session's writer thread has stopped".to_string()))
    }

    fn take_output(&self) -> Option<mpsc::Receiver<PtyOutput>> {
        lock(&self.output).take()
    }

    fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// Pausing a session's real process is not yet a control-plane command an out-of-process
    /// session can ask its host for (`docs/architecture/decisions.md` §24) - a real, honest error
    /// rather than a silent no-op that would leave the "Pause now" action looking like it worked.
    fn pause(&self) -> Result<(), PtyError> {
        Err(PtyError::Remote(
            "pausing an out-of-process session is not yet supported".to_string(),
        ))
    }

    fn resume(&self) -> Result<(), PtyError> {
        Err(PtyError::Remote(
            "resuming an out-of-process session is not yet supported".to_string(),
        ))
    }

    /// `command/session-kill`, then closes this adapter's own connection - the host closes its
    /// own end once the kill takes effect and the session's real exit is observed, exactly as an
    /// ordinary process exit does (`crate::data_plane`, in `jerry-host`).
    fn shutdown(&self) -> Result<(), PtyError> {
        let killed = (self.kill)().map_err(PtyError::Remote);
        // Always closes this adapter's own connection, even when the kill dispatch itself
        // failed: a failed `command/session-kill` must not also leave a live client believing
        // the connection is still usable.
        let _ = lock(&self.shutdown_stream).shutdown(std::net::Shutdown::Both);
        killed
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod socket_session_adapter_tests {
    use super::SocketSessionAdapter;
    use crate::terminal::pane::SessionAdapter;
    use jerry_core::client::{Listener, Stream};
    use jerry_core::SessionId;
    use jerry_pty::PtyOutput;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use test_support::wait_until;

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
                path: dir.join(format!("sa-{}-{tag}.sock", std::process::id())),
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

    fn recv_timeout(rx: &mut futures::channel::mpsc::Receiver<PtyOutput>) -> Option<PtyOutput> {
        let mut item = None;
        wait_until(Duration::from_secs(5), || match rx.try_recv() {
            Ok(value) => {
                item = Some(value);
                true
            }
            Err(ref error) if error.is_closed() => true,
            Err(_empty) => false,
        });
        item
    }

    /// Binds a real listener, standing in for `jerry-host`'s own per-session one (`crate::
    /// data_plane`) - no real host involved, matching this type's own unit-test scope - and
    /// starts accepting on a dedicated thread. The caller connects its own adapter, then joins
    /// the returned handle for the server-side end of that same connection.
    fn spawn_acceptor(tag: &str) -> (SocketPath, thread::JoinHandle<Stream>) {
        let socket = socket_path(tag);
        let listener = Listener::bind(&socket.path).expect("bind");
        let accept = thread::spawn(move || listener.accept().expect("accept").0);
        (socket, accept)
    }

    #[test]
    fn bytes_from_the_socket_arrive_as_pty_output_bytes_in_order() {
        let (socket, accept) = spawn_acceptor("bytes-in-order");
        let adapter =
            SocketSessionAdapter::connect(&socket.path, SessionId::from("session-9"), None, || {
                Ok(())
            })
            .expect("connect");
        assert_eq!(adapter.id(), &SessionId::from("session-9"));
        let mut server = accept.join().expect("accept thread");

        server.write_all(b"hello ").expect("write first chunk");
        server.write_all(b"world").expect("write second chunk");

        let mut output = adapter.take_output().expect("output stream, once");
        assert!(
            adapter.take_output().is_none(),
            "a second take must answer None"
        );

        let mut collected = Vec::new();
        while collected.len() < b"hello world".len() {
            match recv_timeout(&mut output) {
                Some(PtyOutput::Bytes(chunk)) => collected.extend_from_slice(&chunk),
                other => panic!("expected more bytes, got {other:?}"),
            }
        }
        assert_eq!(collected, b"hello world");
    }

    #[test]
    fn write_input_reaches_the_socket_and_shutdown_calls_kill_then_closes() {
        let (socket, accept) = spawn_acceptor("write-and-kill");
        let killed = Arc::new(AtomicBool::new(false));
        let killed_writer = Arc::clone(&killed);
        let adapter = SocketSessionAdapter::connect(
            &socket.path,
            SessionId::from("session-10"),
            None,
            move || {
                killed_writer.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("connect");
        let mut server = accept.join().expect("accept thread");

        adapter.write_input(b"typed input").expect("write_input");
        let mut buf = [0u8; 32];
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let n = server.read(&mut buf).expect("read what was written");
        assert_eq!(&buf[..n], b"typed input");

        adapter.shutdown().expect("shutdown");
        assert!(killed.load(Ordering::SeqCst), "kill must have been called");
        assert!(
            wait_until(Duration::from_secs(5), || {
                let mut probe = [0u8; 1];
                matches!(server.read(&mut probe), Ok(0))
            }),
            "shutdown must close the adapter's own connection"
        );
    }

    #[test]
    fn a_kill_failure_is_reported_and_the_socket_is_still_closed() {
        let (socket, accept) = spawn_acceptor("kill-failure");
        let adapter = SocketSessionAdapter::connect(
            &socket.path,
            SessionId::from("session-11"),
            None,
            || Err("the host refused session-kill".to_string()),
        )
        .expect("connect");
        let mut server = accept.join().expect("accept thread");

        let err = adapter.shutdown().expect_err("kill failed");
        assert!(err.to_string().contains("the host refused session-kill"));
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        assert!(
            wait_until(Duration::from_secs(5), || {
                let mut probe = [0u8; 1];
                matches!(server.read(&mut probe), Ok(0))
            }),
            "the connection is still closed even when the kill dispatch itself failed"
        );
    }

    #[test]
    fn the_stream_simply_ends_when_the_host_closes_it_no_synthetic_exit() {
        let (socket, accept) = spawn_acceptor("host-closes");
        let adapter = SocketSessionAdapter::connect(
            &socket.path,
            SessionId::from("session-12"),
            None,
            || Ok(()),
        )
        .expect("connect");
        let server = accept.join().expect("accept thread");
        let mut output = adapter.take_output().expect("output stream");

        drop(server);

        assert!(
            wait_until(Duration::from_secs(5), || matches!(
                output.try_recv(),
                Err(ref error) if error.is_closed()
            )),
            "the stream must end, not hang, once the host closes its side"
        );
    }
}
