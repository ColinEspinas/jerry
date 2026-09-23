//! A session's data plane (`docs/architecture/decisions.md` §24): one real AF_UNIX/named-pipe
//! socket per session, bound the moment [`crate::session::SessionManager::spawn`] starts the
//! session and torn down when [`crate::session::SessionManager::forget_session`] forgets it. From
//! the moment a client connects, the byte stream is raw and bidirectional - no JSON-RPC framing,
//! no `PtyOutput` envelope. Output produced before any client connects is buffered here, bounded
//! and drop-oldest, so a late-attaching client still sees recent output; the host closes the
//! connection once the session exits and every buffered byte has been delivered. Exactly one
//! attach at a time, enforced for real at the socket layer - a second connection while one is
//! already served is accepted and dropped; [`crate::session::SessionManager::attach`]'s own
//! `Report::Denied` is an earlier, friendlier rejection of the common case, not the guarantee.

use jerry_core::client::{Listener, Stream};
use jerry_core::registry::{fresh_u32, MAX_SOCKET_PATH_BYTES};
use jerry_pty::PtySession;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown as NetShutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

/// How many bytes of already-produced output a session buffers for a client that has not
/// attached yet - drop-oldest once full, so a pane that attaches a moment late still sees recent
/// output without this growing unbounded for a session nobody ever attaches to.
const PRE_ATTACH_BUFFER_CAP_BYTES: usize = 256 * 1024;

/// How many not-yet-written chunks a live connection's writer may fall behind by before
/// [`DataPlane::push`] starts blocking the relay thread that calls it - the same backstop shape
/// `crate::session::RELAY_CHANNEL_CAPACITY` already uses for the control-plane relay.
const LIVE_SINK_BACKLOG: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum DataPlaneBindError {
    /// `sockets_dir` itself does not exist and could not be created - unlike the host's own
    /// control-plane socket, an unpublished (test, or not-yet-published) host's sessions have no
    /// `Registry::open` call of their own to have created it first.
    #[error(transparent)]
    Directory(#[from] jerry_core::registry::RegistryError),
    #[error("could not listen on {}: {source}", path.display())]
    Bind {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not spawn the data-plane accept thread: {0}")]
    Thread(io::Error),
}

struct Shared {
    buffer: VecDeque<u8>,
    /// `Some` for exactly as long as a client is attached and being served - registered and
    /// snapshotted atomically with `buffer` in [`serve_one`], so no byte pushed after that
    /// snapshot is ever missed or duplicated. Dropping it (here, or in [`DataPlane::mark_exited`])
    /// is what unblocks that connection's writer thread and closes the socket.
    sink: Option<std_mpsc::SyncSender<Vec<u8>>>,
}

/// One session's real, bound data-plane socket and the state its accept loop and
/// [`DataPlane::push`]/[`DataPlane::mark_exited`] (both called from the session's own relay
/// thread, `crate::session::spawn_relay`) share.
pub(crate) struct DataPlane {
    socket: PathBuf,
    shared: Mutex<Shared>,
    /// Set for as long as a real connection is being served - the socket-layer half of "exactly
    /// one attach at a time" (see the module docs).
    attached: AtomicBool,
    /// Set once the session's `Exited` item has been observed - a data point only, checked by no
    /// code here (an already-exited session may still be attached to once, to drain the buffer);
    /// documents the state honestly for anything reading it back later (`docs/architecture/
    /// decisions.md` §24's own PR2 groundwork).
    exited: AtomicBool,
    stopped: AtomicBool,
}

impl DataPlane {
    /// Binds a fresh per-session socket under `sockets_dir` and starts its accept loop, one
    /// connection served at a time. `process` is where a connected client's input bytes go.
    pub(crate) fn bind(
        sockets_dir: &Path,
        process: Arc<Mutex<PtySession>>,
    ) -> Result<Arc<DataPlane>, DataPlaneBindError> {
        // An unpublished (test) host, or a published one whose registry directory happens to
        // differ from `sockets_dir`, has no other code path that guarantees this directory
        // exists yet - unlike `Registry::open`'s own directory, nothing else creates it first.
        jerry_core::registry::ensure_private_dir(sockets_dir)?;
        let socket = sockets_dir.join(socket_file_name(std::process::id(), fresh_u32()));
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH_BYTES {
            return Err(DataPlaneBindError::Bind {
                path: socket,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "socket path is {len} bytes, over the {MAX_SOCKET_PATH_BYTES}-byte limit"
                    ),
                ),
            });
        }
        let listener = Listener::bind(&socket).map_err(|source| DataPlaneBindError::Bind {
            path: socket.clone(),
            source,
        })?;
        let data_plane = Arc::new(DataPlane {
            socket: socket.clone(),
            shared: Mutex::new(Shared {
                buffer: VecDeque::new(),
                sink: None,
            }),
            attached: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let accept_state = Arc::clone(&data_plane);
        thread::Builder::new()
            .name("jerry-host-session-data-accept".into())
            .spawn(move || {
                for connection in listener.incoming() {
                    if accept_state.stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    match connection {
                        Ok(stream) => serve_one(&accept_state, stream, &process),
                        Err(error) => {
                            log::warn!("jerry-host: data-plane accept failed: {error}");
                        }
                    }
                }
            })
            .map_err(DataPlaneBindError::Thread)?;
        Ok(data_plane)
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// How many bytes the pre-attach buffer currently holds - a deterministic, real condition a
    /// test can `wait_until` on (rather than a blind sleep) to know the relay thread has pushed
    /// something before that test's own client ever attaches.
    #[cfg(test)]
    pub(crate) fn buffered_len(&self) -> usize {
        lock(&self.shared).buffer.len()
    }

    /// Appends `chunk` to the pre-attach buffer (bounded, drop-oldest) and, when a client is
    /// currently attached, forwards it to that connection's writer thread in the same order -
    /// called only by the session's own relay thread, so pushes are never reordered against each
    /// other.
    pub(crate) fn push(&self, chunk: &[u8]) {
        let mut shared = lock(&self.shared);
        shared.buffer.extend(chunk.iter().copied());
        let overflow = shared
            .buffer
            .len()
            .saturating_sub(PRE_ATTACH_BUFFER_CAP_BYTES);
        if overflow > 0 {
            shared.buffer.drain(..overflow);
        }
        if let Some(sink) = &shared.sink {
            if sink.send(chunk.to_vec()).is_err() {
                shared.sink = None;
            }
        }
    }

    /// The session's `Exited` item has been observed: drops the live sink, if any, which is what
    /// makes a currently-attached connection's writer finish delivering whatever was already
    /// queued and then close the socket - "the host closes the socket after the last byte when
    /// the session exits" (`docs/architecture/decisions.md` §24).
    pub(crate) fn mark_exited(&self) {
        self.exited.store(true, Ordering::SeqCst);
        lock(&self.shared).sink = None;
    }

    /// Stops the accept loop and removes the socket file. Never blocks - the accept thread exits
    /// on its own once woken, mirroring `crate::listener::Listening::stop`. Idempotent.
    pub(crate) fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = Stream::connect(&self.socket);
        let _ = fs::remove_file(&self.socket);
        lock(&self.shared).sink = None;
    }
}

impl Drop for DataPlane {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A session's data-plane socket file name - two bytes longer than [`jerry_core::registry::
/// Registry::allocate`]'s own `<pid:x>-<u32:08x>.sock` (the `"d-"` prefix), which is the margin
/// [`bind`]'s own `MAX_SOCKET_PATH_BYTES` check exists to catch per-session rather than assume.
fn socket_file_name(pid: u32, fresh: u32) -> String {
    format!("d-{pid:x}-{fresh:08x}.sock")
}

/// Serves exactly one connection to completion: replays the buffer snapshot, then relays new
/// pushes and forwards the client's own bytes to `process` until either side closes. A second
/// connection arriving while one is already being served is accepted and dropped immediately -
/// the module docs' own "exactly one attach" guarantee.
fn serve_one(state: &Arc<DataPlane>, stream: Stream, process: &Arc<Mutex<PtySession>>) {
    if state.attached.swap(true, Ordering::SeqCst) {
        let _ = stream.shutdown(NetShutdown::Both);
        return;
    }
    let (mut writer_stream, mut reader_stream) = match (stream.try_clone(), stream) {
        (Ok(writer), reader) => (writer, reader),
        (Err(error), _) => {
            log::warn!("jerry-host: could not clone a data-plane connection: {error}");
            state.attached.store(false, Ordering::SeqCst);
            return;
        }
    };

    let (tx, rx) = std_mpsc::sync_channel::<Vec<u8>>(LIVE_SINK_BACKLOG);
    let snapshot: Vec<u8> = {
        let mut shared = lock(&state.shared);
        shared.sink = Some(tx);
        shared.buffer.iter().copied().collect()
    };
    if !snapshot.is_empty() && writer_stream.write_all(&snapshot).is_err() {
        lock(&state.shared).sink = None;
        state.attached.store(false, Ordering::SeqCst);
        return;
    }
    // The session had already exited by the time this connection registered: `mark_exited` ran
    // and dropped its sink strictly before this one was ever installed, so nothing will ever
    // signal this `rx` again - waiting on it would hang forever rather than close. The buffer
    // snapshot just delivered is everything this late attach will ever see.
    if state.exited.load(Ordering::SeqCst) {
        lock(&state.shared).sink = None;
        let _ = writer_stream.shutdown(NetShutdown::Both);
        let _ = reader_stream.shutdown(NetShutdown::Both);
        state.attached.store(false, Ordering::SeqCst);
        return;
    }

    let writer = thread::Builder::new()
        .name("jerry-host-session-data-writer".into())
        .spawn(move || {
            while let Ok(chunk) = rx.recv() {
                if writer_stream.write_all(&chunk).is_err() {
                    break;
                }
            }
            let _ = writer_stream.shutdown(NetShutdown::Both);
        });

    let mut buf = [0u8; 8192];
    loop {
        match reader_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if lock(process).write_input(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = reader_stream.shutdown(NetShutdown::Both);
    // Dropping the sink (if the writer hasn't already been ended by `mark_exited`) is what
    // unblocks the writer thread above so it can close and return.
    lock(&state.shared).sink = None;
    if let Ok(writer) = writer {
        let _ = writer.join();
    }
    state.attached.store(false, Ordering::SeqCst);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod socket_length_tests {
    use super::socket_file_name;
    use jerry_core::registry::{runtime_dir_for, Os, MAX_SOCKET_PATH_BYTES};
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(*v)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    /// The real regression behind PR #538's macOS failure: a *test's own* extra nested directory
    /// pushed a socket path over the limit, not a genuine production shape - but nothing had ever
    /// checked the production shape itself. Computed against each platform's own worst-case
    /// environment (the longest real `$TMPDIR`/`$XDG_RUNTIME_DIR` shape macOS/Linux are known to
    /// use, a maximal `u32` pid and a maximal `fresh_u32`) rather than a literal number, so a
    /// future change to any of `runtime_dir_for`'s directory shapes or this file name's own format
    /// fails here instead of only in a real, hard-to-reproduce CI run on one specific OS.
    #[test]
    fn every_platforms_runtime_dir_leaves_real_margin_for_a_data_plane_socket() {
        let cases = [
            (
                Os::Windows,
                vec![("LOCALAPPDATA", r"C:\Users\someone\AppData\Local")],
            ),
            (
                Os::MacOs,
                vec![
                    (
                        "TMPDIR",
                        "/private/var/folders/36/0123456789abcdefghijklmnop/T/",
                    ),
                    ("USER", "someuser"),
                ],
            ),
            (Os::Unix, vec![("XDG_RUNTIME_DIR", "/run/user/4294967295")]),
        ];
        // The file name's own worst case: an 8-hex-digit pid (`u32::MAX`) and `fresh_u32`'s own
        // `{:08x}` is already fixed-width.
        let worst_case_name = socket_file_name(u32::MAX, u32::MAX);
        for (os, pairs) in cases {
            let dir = runtime_dir_for(os, &env(&pairs)).expect("runtime dir");
            let socket = dir.join(&worst_case_name);
            let len = socket.as_os_str().len();
            assert!(
                len <= MAX_SOCKET_PATH_BYTES,
                "{os:?}: {} is {len} bytes, over the {MAX_SOCKET_PATH_BYTES}-byte limit - a real \
                 session spawn on this platform would fail to bind its data-plane socket",
                socket.display()
            );
        }
    }
}
