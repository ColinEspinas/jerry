//! `jerry-pty`: spawn, stream, resize and kill a native pseudo-terminal, over `portable-pty`.
//!
//! Knows nothing about ANSI escapes or grid state - `crates/jerry-app` owns those. Output streams over a
//! bounded channel so an undrained consumer backpressures the child rather than leaking; shutdown
//! is a self-pipe polled alongside the pty fd; kill snapshots the descendant tree before
//! signalling. `Drop` never blocks, [`PtySession::shutdown`] is the deterministic counterpart.
//! See `docs/architecture/decisions.md` §8, including why Windows is narrower and untested there.
//!
//! Only `#[cfg(unix)]` gets the self-pipe and the process-tree walk; `/proc` readers degrade to
//! "found nothing" on a non-Linux unix, leaving process-group signals intact.

// Only production code is held to `unwrap_used`/`expect_used` and the bare-`Command::new`
// ban (`clippy.toml`, GitHub issue #465); see `CLAUDE.md`.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)
)]

use futures::channel::mpsc as futures_mpsc;
use futures::executor::block_on;
use futures::SinkExt;
#[cfg(windows)]
use portable_pty::ChildKiller;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
#[cfg(unix)]
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use thiserror::Error;

// A GUI-launched process (Finder's `.app`, a `.desktop` entry) inherits a minimal PATH that
// omits Homebrew/nvm/asdf shims; Windows processes don't have this problem, so the module is
// unix-only rather than `#[cfg]`-gating every item inside it.
#[cfg(unix)]
mod login_shell;
#[cfg(unix)]
pub use login_shell::resolve_login_shell_path;

mod command;
pub use command::new_std_command;

mod detached;
pub use detached::new_detached_command;

pub use portable_pty::ExitStatus;

/// How many chunks the output channel buffers before the reader thread blocks, bounding buffered
/// memory to roughly this times [`READ_BUF_SIZE`].
///
/// **Also the dominant control on throughput**, which the memory framing hides. A consumer
/// draining on an interval can only take what the channel buffered in between, so against a pty
/// firehose drained every 8ms: capacity 16 gives 4.3 MB/s, capacity 256 gives 36.2 MB/s (measured
/// against the earlier `std::sync::mpsc::sync_channel` design - a consumer that instead awaits
/// every item, as [`PtySession::take_output`]'s callers do, is woken per chunk rather than per
/// tick, so throughput is no longer interval-gated at all). Do not lower it without measuring
/// under a *slow* consumer - both reach ~42 MB/s when drained in a tight loop, so a
/// fast-consumer benchmark cannot see this at all.
///
/// `futures::channel::mpsc::channel`'s own capacity is `buffer + num-senders` - each live
/// `Sender` reserves one guaranteed slot on top of these shared ones - so the real bound is this
/// constant plus one slot per sender ever alive on the channel. On unix that is the reader thread
/// alone - [`run_wait_loop`] hands its exit status to the reader instead of sending on this
/// channel itself, which is what lets the reader guarantee `Exited` arrives last (see
/// `docs/architecture/decisions.md` §8). Windows has no way to interrupt a blocked read (see that
/// entry's Windows paragraph), so its `run_wait_loop` still sends independently - two senders
/// there.
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
/// Size of each read from the pty master.
///
/// 4KiB despite `alacritty_terminal` using 1MiB, because raising it measured at ~2%: `read(2)`
/// returns what is available rather than filling the buffer, and on a pty that is governed by the
/// line discipline (p50 ~600-700 bytes, p95 4095 either way). [`OUTPUT_CHANNEL_CAPACITY`] is what
/// actually governs throughput.
const READ_BUF_SIZE: usize = 4096;
/// How long [`PtySession::shutdown`] waits after `SIGHUP` before `SIGKILL`. `Drop` uses no grace.
#[cfg(unix)]
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(200);
/// Poll interval while waiting out [`SHUTDOWN_GRACE_PERIOD`].
#[cfg(unix)]
const SHUTDOWN_GRACE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Windows post-exit grace: how long [`run_wait_loop`] requires the reader to have stayed idle
/// (not mid-delivery) before treating "nothing more is coming" as settled - see that function's
/// Windows docs. Never used in the steady state, only in the brief window after `Child::wait`
/// returns.
#[cfg(windows)]
const WINDOWS_EXIT_QUIET_WINDOW: Duration = Duration::from_millis(150);
/// Windows post-exit grace: the absolute cap on how long [`run_wait_loop`] waits for
/// [`WINDOWS_EXIT_QUIET_WINDOW`] to be satisfied before sending `Exited` regardless - covers a
/// descendant that keeps writing to the console indefinitely, which would otherwise stall exit
/// reporting forever.
#[cfg(windows)]
const WINDOWS_EXIT_GRACE_DEADLINE: Duration = Duration::from_millis(2000);
/// Poll interval while waiting out the Windows post-exit grace window.
#[cfg(windows)]
const WINDOWS_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// Depth cap for the `/proc` descendant walk, defensive against a pathological process tree.
#[cfg(unix)]
const TREE_WALK_MAX_DEPTH: usize = 8;

/// Errors from spawning, driving, or tearing down a [`PtySession`].
///
/// Carries no `anyhow::Error`: it does not implement `std::error::Error`, so thiserror cannot take
/// it as a `#[source]`, and it would leak a dependency type into this crate's public API.
/// `portable-pty`'s own failures are captured as their `Display` output instead.
#[derive(Debug, Error)]
pub enum PtyError {
    #[error("failed to open pty: {0}")]
    Open(String),
    #[error("failed to spawn command on pty: {0}")]
    Spawn(String),
    #[error("failed to clone pty reader: {0}")]
    Reader(String),
    #[error("failed to take pty writer: {0}")]
    Writer(String),
    #[error("failed to resize pty: {0}")]
    Resize(String),
    #[error("failed to create pty shutdown pipe: {0}")]
    ShutdownPipe(String),
    #[error("failed to create pty exit-signal pipe: {0}")]
    ExitPipe(String),
    #[error("pty input writer is closed")]
    WriterClosed,
    #[error("failed to wait on child process: {0}")]
    Wait(#[source] std::io::Error),
    #[error("cwd {0:?} does not exist or is not a directory")]
    InvalidCwd(PathBuf),
    #[error("failed to determine current working directory: {0}")]
    CurrentDir(#[source] std::io::Error),
    #[error("pty session was already shut down")]
    AlreadyShutDown,
    #[error("failed to signal child process: {0}")]
    Signal(String),
    #[error("`{0}` did not finish within the timeout and was killed")]
    CaptureTimeout(String),
    #[error("`{0}` printed no usable line")]
    CaptureEmpty(String),
    /// Never produced by this crate itself: `crate::terminal::pane::SessionAdapter`
    /// (`crates/jerry-app`) has two implementers, a real [`PtySession`] and
    /// `jerry-app`'s own socket-backed adapter for a session owned by an out-of-process
    /// `jerry-host` (`docs/architecture/decisions.md` §24) - the latter has no `PtySession` to
    /// report a *real* [`PtyError`] variant for, so a socket I/O failure or a failed
    /// `command/session-kill` dispatch is reported through this one instead, to satisfy the
    /// same trait's signature rather than invent a second, adapter-specific error type.
    #[error("{0}")]
    Remote(String),
}

/// One item from [`PtySession::take_output`]'s stream: either a chunk of raw bytes, or the
/// child's real exit status once [`run_wait_loop`]'s dedicated thread has reaped it. Putting
/// exit on the same channel as bytes is what lets a consumer simply await the stream to
/// completion instead of separately polling [`PtySession::try_wait`] after an inferred pty EOF.
#[derive(Debug)]
pub enum PtyOutput {
    /// Raw output, neither line-buffered nor UTF-8-validated, in read order.
    Bytes(Vec<u8>),
    /// The child process exited with this status. At most one per session. On unix this is a hard
    /// guarantee: the last item on the stream, after every `Bytes` chunk the child wrote before
    /// exiting - see [`run_wait_loop`]'s unix docs. On Windows it is a strong heuristic rather than
    /// a guarantee (see that function's Windows docs and `docs/architecture/decisions.md` §8): a
    /// trailing `Bytes` chunk can still, rarely, arrive after `Exited`.
    Exited(ExitStatus),
}

/// Describes a process to spawn on a new PTY, plus the PTY's initial size.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

impl SpawnOptions {
    /// Default 80x24, no args or env overrides, and the caller's current directory at spawn time.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn size(mut self, rows: u16, cols: u16) -> Self {
        self.rows = rows;
        self.cols = cols;
        self
    }
}

/// Resolves a bare command name against `$PATH`, so a caller can ask whether a spawn *would*
/// succeed without spawning.
///
/// Reimplements `portable-pty`'s private `CommandBuilder::search_path`, with two departures:
/// executability is read from the mode bits rather than `access(2)`, so ACLs and the process's own
/// uid/gid are not consulted; and cwd-relative candidates are never checked, since every caller
/// passes a bare name. Both walk `$PATH` in the same order, so they agree in realistic cases.
pub fn resolve_on_path(program: &str) -> Option<PathBuf> {
    resolve_in_path_var(&std::env::var_os("PATH")?, program)
}

/// Runs `program args` in `cwd` and returns the first non-empty line it prints to stdout, killing
/// it and failing if it hasn't finished within `timeout`.
///
/// Blocking, like everything else here - call it off the UI thread. For helper commands that
/// print one short line and exit; stdout is drained on its own thread, so a chattier command
/// can't deadlock against a full pipe, but nothing here bounds how much it may print.
///
/// The timeout is not defensive padding. The real motivating case, `cursor-agent create-chat`,
/// hangs indefinitely and prints nothing at all when the user isn't logged in - so a caller that
/// merely waited would hang with it.
///
/// A timed-out command is torn down by [`windows_terminate_process_tree`] plus `Child::kill` on
/// Windows, and by `Child::kill` alone on unix. The unix process-group teardown [`PtySession`]
/// uses is deliberately not reused: nothing `setsid`s this child, so a `killpg` would signal the
/// *caller's* group, Jerry included. A unix wrapper script's own child can therefore outlive the
/// kill.
pub fn capture_first_line(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<String, PtyError> {
    let mut command = crate::command::new_std_command(program);
    command
        .args(args)
        .current_dir(cwd)
        // Never inherit this process's stdin: an interactive prompt would otherwise be able to
        // block the helper forever on a terminal the user cannot see.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|err| PtyError::Spawn(format!("{program}: {err}")))?;

    let (sender, receiver) = mpsc::channel();
    // `wait_with_output` consumes the child, so the kill path below can't hold it - it keeps a
    // separate handle and signals by pid through `Child::kill`'s own stored handle instead.
    let mut kill_handle = match child.stdout.take() {
        Some(stdout) => {
            let waiter = std::thread::spawn(move || {
                let mut buffer = String::new();
                let mut stdout = stdout;
                let _ = std::io::Read::read_to_string(&mut stdout, &mut buffer);
                let _ = sender.send(buffer);
            });
            drop(waiter);
            child
        }
        None => {
            return Err(PtyError::Spawn(format!(
                "{program}: stdout was not captured"
            )))
        }
    };

    let captured = receiver.recv_timeout(timeout);
    // Whatever happened, this helper has no further purpose - a timed-out one is still running.
    // The tree kill first, because on Windows the direct kill reaches only a shim: `cursor-agent`
    // is a `.cmd` wrapping a real `node.exe`, the same orphaning GitHub issue #468 fixed for pty
    // children.
    #[cfg(windows)]
    windows_terminate_process_tree(kill_handle.id());
    let _ = kill_handle.kill();
    let _ = kill_handle.wait();

    let captured = captured.map_err(|_| PtyError::CaptureTimeout(program.to_owned()))?;
    captured
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PtyError::CaptureEmpty(program.to_owned()))
}

/// [`resolve_on_path`]'s search loop, taking `PATH` as an argument so a test can pass an isolated
/// value. Mutating the process-global var instead would need `unsafe` and would race concurrent
/// tests reading the environment.
#[cfg(unix)]
fn resolve_in_path_var(path_var: &OsStr, program: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(program);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Executable-bit check via `std::fs::Metadata`; see [`resolve_on_path`] for why not `access(2)`.
#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    const EXEC_BITS: u32 = 0o111; // owner, group, other execute bits
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & EXEC_BITS != 0)
        .unwrap_or(false)
}

/// Windows has no execute bit, so `%PATHEXT%` (defaulting to `.EXE`) decides instead: the bare
/// candidate first, then each extension.
///
/// Diverges from `portable-pty` in checking `is_file()` rather than `exists()`, which would match
/// a same-named directory, and in skipping an empty `PATHEXT` entry rather than panicking on it.
///
/// Caveat inherited deliberately rather than fixed: `with_extension` *replaces* an extension, so
/// `python3.11` resolves as `python3.EXE`. Upstream calls this "potentially wrong"; diverging here
/// would disagree with the algorithm [`spawn`] itself goes through.
#[cfg(windows)]
fn resolve_in_path_var(path_var: &OsStr, program: &str) -> Option<PathBuf> {
    let pathext = std::env::var_os("PATHEXT").unwrap_or_else(|| ".EXE".into());
    let extensions: Vec<String> = std::env::split_paths(&pathext)
        .filter_map(|extension| {
            let extension = extension.to_str()?.trim_start_matches('.');
            (!extension.is_empty()).then(|| extension.to_string())
        })
        .collect();

    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        for extension in &extensions {
            let candidate = candidate.with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A running process attached to a PTY.
pub struct PtySession {
    // `Option` so `shutdown`'s Windows twin can drop this before joining the reader thread:
    // that is what closes the ConPTY and unblocks the reader's `read`.
    master: Option<Box<dyn MasterPty + Send>>,
    /// Cached once at spawn, before `child` moves into [`run_wait_loop`]'s dedicated thread - a
    /// pid never changes, so reading the cache back needs no further synchronization.
    pid: Option<u32>,
    /// Signals the child without needing the `Child` handle itself, which
    /// [`Self::wait_thread`] owns exclusively for the rest of the session's life. Windows-only:
    /// unix's [`kill`](Self::kill)/[`shutdown`](Self::shutdown)/`Drop` all signal by pid
    /// (`terminate_process_tree`) instead, so this would otherwise be write-only there.
    #[cfg(windows)]
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    exited: Option<ExitStatus>,
    /// `None` once [`Self::take_output`] has been called - a session's output stream is claimed
    /// exactly once, by whichever caller is going to hold and await it independently of every
    /// other `PtySession` method (see that method's docs for why it must be owned, not borrowed).
    output_rx: Option<futures_mpsc::Receiver<PtyOutput>>,
    reader_thread: Option<JoinHandle<()>>,
    /// The one thread that ever calls [`Child::wait`] for this session - see [`run_wait_loop`]'s
    /// docs for why a second, independent poll of the same child would race it. Its `JoinHandle`
    /// carries the real exit status back, so [`Self::try_wait`]/[`Self::shutdown`] read it via
    /// `is_finished`/`join` instead of a separate shared cell.
    wait_thread: Option<JoinHandle<ExitStatus>>,
    writer_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Raised when a write fails, so [`PtySession::write_input`] stops reporting success into a
    /// broken pipe.
    writer_failed: Arc<AtomicBool>,
    writer_thread: Option<JoinHandle<()>>,
    shutdown_write: Option<filedescriptor::FileDescriptor>,
}

/// Spawns `options.program` on a freshly opened native PTY.
///
/// The parent's `pair.slave` is dropped immediately: the child inherits its own duplicate, and the
/// master sees EOF only once *every* reference to the slave side is closed - so holding it would
/// mean EOF never arrives after the child exits.
pub fn spawn(options: SpawnOptions) -> Result<PtySession, PtyError> {
    let cwd = resolve_cwd(options.cwd)?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: options.rows,
            cols: options.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| PtyError::Open(err.to_string()))?;

    let mut cmd = CommandBuilder::new(&options.program);
    cmd.args(options.args.iter());
    cmd.cwd(&cwd);
    for (key, value) in &options.env {
        cmd.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| PtyError::Spawn(err.to_string()))?;
    // Load-bearing for EOF delivery, not cleanup - see the doc comment above.
    drop(pair.slave);

    // Cached/cloned before `child` moves into `run_wait_loop`'s thread below - see
    // `PtySession::pid`/`PtySession::killer`'s own field docs for why each exists independently
    // of the `Child` handle itself.
    let pid = child.process_id();
    #[cfg(windows)]
    let killer = child.clone_killer();

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| PtyError::Reader(err.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| PtyError::Writer(err.to_string()))?;

    let (output_tx, output_rx) = futures_mpsc::channel::<PtyOutput>(OUTPUT_CHANNEL_CAPACITY);

    // Unix polls a self-pipe alongside the pty fd for deterministic shutdown, plus a second one
    // `run_wait_loop` uses to hand the reader its exit status and wake it for a final bounded
    // drain - see that function's docs. Windows has no self-pipe equivalent (`WSAPoll` accepts
    // only sockets, not a ConPTY named pipe) and no way to inspect the pipe's contents from outside
    // it either, so its wait thread still sends `Exited` on its own, timed against the reader's
    // `delivering` state instead - see `run_wait_loop`'s Windows docs.
    #[cfg(unix)]
    let (reader_thread, shutdown_write, wait_thread) = {
        let master_fd = pair.master.as_raw_fd().ok_or_else(|| {
            PtyError::Open("pty master exposed no raw file descriptor".to_string())
        })?;
        let filedescriptor::Pipe {
            read: shutdown_read,
            write: shutdown_write,
        } = filedescriptor::Pipe::new().map_err(|err| PtyError::ShutdownPipe(err.to_string()))?;
        let filedescriptor::Pipe {
            read: exit_read,
            write: exit_write,
        } = filedescriptor::Pipe::new().map_err(|err| PtyError::ExitPipe(err.to_string()))?;
        let (exit_status_tx, exit_status_rx) = mpsc::channel::<ExitStatus>();
        // Only the reader sends on `output_tx` here - it owns the whole `Sender`, not a clone -
        // which is what makes `Exited` arriving last a real guarantee rather than a race.
        let reader_thread = std::thread::spawn(move || {
            run_reader_loop(
                reader,
                master_fd,
                shutdown_read,
                exit_read,
                exit_status_rx,
                output_tx,
            )
        });
        let wait_thread =
            std::thread::spawn(move || run_wait_loop(child, exit_status_tx, exit_write));
        (reader_thread, Some(shutdown_write), wait_thread)
    };
    #[cfg(windows)]
    let (reader_thread, shutdown_write, wait_thread) = {
        // `false` ("Reading"): the reader is idle or blocked in `read`, not holding a chunk it
        // hasn't sent yet. `true` ("Delivering"): between `read` returning and the chunk actually
        // reaching `output_tx` - see `run_wait_loop`'s Windows docs for why this is what the wait
        // thread watches.
        let delivering = Arc::new(AtomicBool::new(false));
        let reader_thread = std::thread::spawn({
            let output_tx = output_tx.clone();
            let delivering = Arc::clone(&delivering);
            move || run_reader_loop(reader, output_tx, delivering)
        });
        // Takes the original sender (the reader thread above holds a clone) so it survives
        // independently of the reader - see `run_wait_loop`'s Windows docs for why this platform
        // cannot instead guarantee `Exited` as the reader's own final item.
        let wait_thread = std::thread::spawn(move || run_wait_loop(child, output_tx, delivering));
        (reader_thread, None, wait_thread)
    };

    let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>();
    let writer_failed = Arc::new(AtomicBool::new(false));
    let writer_thread = std::thread::spawn({
        let writer_failed = Arc::clone(&writer_failed);
        move || {
            run_writer_loop(writer, writer_rx, writer_failed);
        }
    });

    Ok(PtySession {
        master: Some(pair.master),
        pid,
        #[cfg(windows)]
        killer: Some(killer),
        exited: None,
        output_rx: Some(output_rx),
        reader_thread: Some(reader_thread),
        wait_thread: Some(wait_thread),
        writer_tx: Some(writer_tx),
        writer_failed,
        writer_thread: Some(writer_thread),
        shutdown_write,
    })
}

fn resolve_cwd(cwd: Option<PathBuf>) -> Result<PathBuf, PtyError> {
    match cwd {
        Some(path) => {
            if path.is_dir() {
                Ok(path)
            } else {
                Err(PtyError::InvalidCwd(path))
            }
        }
        None => std::env::current_dir().map_err(PtyError::CurrentDir),
    }
}

/// The reader thread: polls `[master_fd, shutdown_read, exit_read]` with no timeout, forwards a
/// chunk when the master is readable, exits immediately if the shutdown pipe fires (an explicit
/// [`PtySession::shutdown`]/kill in progress - the caller already has the status directly and
/// does not need it repeated on the channel), and otherwise, once `exit_read` fires, hands off to
/// [`drain_final_output`] for a bounded final drain and the `Exited` item - see that function's
/// docs for why this is what makes `Exited` a real "nothing more is coming" guarantee.
///
/// The master fd reaching real EOF/hangup - Linux reports this as an `EIO` read error, macOS as an
/// `Ok(0)` read (`filedescriptor::poll` itself differs too: macOS's `poll` is `select`-backed and
/// reports the fd as read-ready rather than `POLLHUP`, see `filedescriptor::unix::macos`) - is
/// *not* treated as the stream ending: the direct child closes its last fd to the slave (hence the
/// master hangs up) as part of exiting, which routinely happens before [`run_wait_loop`]'s
/// `Child::wait()` call returns and wakes `exit_read`. Stopping here would drop `Exited` on
/// exactly the common case this crate exists to get right - see the "a quiet child" scenario in
/// `docs/architecture/decisions.md` §8. Once the master is confirmed closed this way, the loop
/// stops polling/reading it (avoiding a busy-spin on Linux, where `POLLHUP` stays set) and waits
/// out `shutdown_read`/`exit_read` instead via [`await_exit_after_master_closed`].
#[cfg(unix)]
fn run_reader_loop(
    mut reader: Box<dyn Read + Send>,
    master_fd: RawFd,
    shutdown_read: filedescriptor::FileDescriptor,
    exit_read: filedescriptor::FileDescriptor,
    exit_status_rx: mpsc::Receiver<ExitStatus>,
    mut output_tx: futures_mpsc::Sender<PtyOutput>,
) {
    let shutdown_fd = shutdown_read.as_raw_fd();
    let exit_fd = exit_read.as_raw_fd();
    let mut buf = [0u8; READ_BUF_SIZE];
    loop {
        let mut pfds = [
            filedescriptor::pollfd {
                fd: master_fd,
                events: filedescriptor::POLLIN,
                revents: 0,
            },
            filedescriptor::pollfd {
                fd: shutdown_fd,
                events: filedescriptor::POLLIN,
                revents: 0,
            },
            filedescriptor::pollfd {
                fd: exit_fd,
                events: filedescriptor::POLLIN,
                revents: 0,
            },
        ];

        if filedescriptor::poll(&mut pfds, None).is_err() {
            break;
        }
        if pfds[1].revents != 0 {
            break;
        }
        if pfds[2].revents != 0 {
            drain_final_output(&mut reader, master_fd, &exit_status_rx, &mut output_tx);
            break;
        }
        if pfds[0].revents == 0 {
            continue;
        }

        match reader.read(&mut buf) {
            Ok(0) => {
                await_exit_after_master_closed(
                    shutdown_fd,
                    exit_fd,
                    &mut reader,
                    master_fd,
                    &exit_status_rx,
                    &mut output_tx,
                );
                break;
            }
            Ok(n) => {
                let chunk = PtyOutput::Bytes(buf[..n].to_vec());
                // Blocks this thread - and so, transitively, `read`, the kernel pty buffer, and
                // the child's own `write` - exactly like the `sync_channel` this replaces (see
                // `OUTPUT_CHANNEL_CAPACITY`'s docs and `docs/architecture/decisions.md` §8).
                if block_on(output_tx.send(chunk)).is_err() {
                    break; // the receiver was dropped; nobody is left to read further output
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // Linux's `EIO` on a hungup pty master lands here.
                await_exit_after_master_closed(
                    shutdown_fd,
                    exit_fd,
                    &mut reader,
                    master_fd,
                    &exit_status_rx,
                    &mut output_tx,
                );
                break;
            }
        }
    }
}

/// Waits for whichever of `shutdown_fd`/`exit_fd` fires once the master fd has already gone away -
/// reached from [`run_reader_loop`] on `EIO`/`Ok(0)`. Only these two fds remain, so there is
/// nothing to busy-spin on; `exit_fd` firing still routes through the ordinary
/// [`drain_final_output`] (which itself tolerates an already-closed master, finding nothing to
/// drain and going straight to the exit status), and `shutdown_fd` firing still skips `Exited`,
/// matching [`run_reader_loop`]'s own priority between the two.
#[cfg(unix)]
fn await_exit_after_master_closed(
    shutdown_fd: RawFd,
    exit_fd: RawFd,
    reader: &mut Box<dyn Read + Send>,
    master_fd: RawFd,
    exit_status_rx: &mpsc::Receiver<ExitStatus>,
    output_tx: &mut futures_mpsc::Sender<PtyOutput>,
) {
    loop {
        let mut pfds = [
            filedescriptor::pollfd {
                fd: shutdown_fd,
                events: filedescriptor::POLLIN,
                revents: 0,
            },
            filedescriptor::pollfd {
                fd: exit_fd,
                events: filedescriptor::POLLIN,
                revents: 0,
            },
        ];
        if filedescriptor::poll(&mut pfds, None).is_err() {
            return;
        }
        if pfds[0].revents != 0 {
            return;
        }
        if pfds[1].revents != 0 {
            drain_final_output(reader, master_fd, exit_status_rx, output_tx);
            return;
        }
    }
}

/// Drains whatever the child already wrote before exiting, then sends its exit status as the
/// stream's final item - called once [`run_wait_loop`] has reaped the child and woken this thread
/// via `exit_read`.
///
/// The drain is bounded by real readiness (`poll` with a zero timeout - a genuine non-blocking
/// check, not a real EOF wait) rather than the master fd's own EOF, which may never arrive: an
/// orphaned descendant that escaped the process group can keep the slave open indefinitely after
/// the direct child is gone. Stopping as soon as nothing is immediately available - rather than
/// waiting out whatever such a descendant might still write - is what lets `Exited` be reported
/// promptly instead of hanging on it.
#[cfg(unix)]
fn drain_final_output(
    reader: &mut Box<dyn Read + Send>,
    master_fd: RawFd,
    exit_status_rx: &mpsc::Receiver<ExitStatus>,
    output_tx: &mut futures_mpsc::Sender<PtyOutput>,
) {
    let mut buf = [0u8; READ_BUF_SIZE];
    loop {
        let mut pfds = [filedescriptor::pollfd {
            fd: master_fd,
            events: filedescriptor::POLLIN,
            revents: 0,
        }];
        match filedescriptor::poll(&mut pfds, Some(Duration::ZERO)) {
            Ok(ready) if ready > 0 && pfds[0].revents != 0 => {}
            _ => break, // nothing immediately available, or the poll itself failed
        }

        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if block_on(output_tx.send(PtyOutput::Bytes(buf[..n].to_vec()))).is_err() {
                    return; // the receiver was dropped; nobody is left to report the exit to
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // `run_wait_loop` sends this before waking `exit_read`, so it is already here.
    if let Ok(status) = exit_status_rx.recv() {
        let _ = block_on(output_tx.send(PtyOutput::Exited(status)));
    }
}

/// The Windows reader thread: no self-pipe is available, so it blocks in `read` until the pty
/// closes.
///
/// Killing the child does not do that - `TerminateProcess` never touches `master`, leaving this
/// blocked forever. `ClosePseudoConsole` runs from `PsuedoCon`'s `Drop`, only once every `Arc` to
/// the shared inner state is gone; [`spawn`] already dropped the slave, so `PtySession::master` is
/// the last one. Dropping it makes conhost release the output pipe's write side.
///
/// That happens explicitly in [`PtySession::shutdown`], or on `Drop` only because `master` is
/// declared *before* `reader_thread` in the struct and so drops first - field order is
/// load-bearing here, not `Drop`'s own body.
///
/// `delivering` is `false` ("Reading") before every blocking `read` call, and `true`
/// ("Delivering") from the moment `read` returns a chunk until it has actually reached
/// `output_tx` - [`run_wait_loop`]'s Windows twin watches this to decide when `Exited` is safe to
/// send.
#[cfg(windows)]
fn run_reader_loop(
    mut reader: Box<dyn Read + Send>,
    mut output_tx: futures_mpsc::Sender<PtyOutput>,
    delivering: Arc<AtomicBool>,
) {
    let mut buf = [0u8; READ_BUF_SIZE];
    loop {
        delivering.store(false, Ordering::SeqCst);
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                delivering.store(true, Ordering::SeqCst);
                let chunk = PtyOutput::Bytes(buf[..n].to_vec());
                if block_on(output_tx.send(chunk)).is_err() {
                    break; // the receiver was dropped; nobody is left to read further output
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// The one thread that ever calls [`Child::wait`] for a session: blocks until the child is really
/// reaped, hands its status to the reader thread, and returns it too so
/// [`PtySession::try_wait`]/[`PtySession::shutdown`] can read it straight off this thread's
/// `JoinHandle` instead of a second shared cell.
///
/// Sends the status over `exit_status_tx` *before* writing `exit_write`, so by the time the reader
/// wakes on that pipe the status is already there waiting - see [`drain_final_output`]. This is
/// what removes the race the reader/wait-thread split otherwise has between pty EOF and reaping
/// (`docs/architecture/decisions.md` §8's amendment): `Child::wait` is the process's own
/// authoritative exit signal, independent of pty state, and only this thread ever sends on
/// `output_tx` (via the reader), so nothing else can interleave `Exited` ahead of a `Bytes` chunk
/// the child wrote first.
#[cfg(unix)]
fn run_wait_loop(
    mut child: Box<dyn Child + Send + Sync>,
    exit_status_tx: mpsc::Sender<ExitStatus>,
    mut exit_write: filedescriptor::FileDescriptor,
) -> ExitStatus {
    let status = child
        .wait()
        .unwrap_or_else(|err| ExitStatus::with_signal(&format!("wait() failed: {err}")));
    let _ = exit_status_tx.send(status.clone());
    let _ = exit_write.write_all(&[1u8]);
    status
}

/// Windows twin of [`run_wait_loop`]: sends `Exited` on `output_tx` directly rather than handing
/// it to the reader thread, because nothing can wake that thread's blocked `read` early on this
/// platform (see this function's crate-level docs) and there is no handle to inspect the pipe's
/// contents from outside it either - `MasterPty::try_clone_reader` erases to `Box<dyn Read +
/// Send>`, and neither it nor the master (downcast to the concrete `ConPtyMasterPty`, whose fields
/// are private) exposes a raw handle a `PeekNamedPipe` call could use (verified against
/// `portable-pty-0.9.0/src/win/{conpty,psuedocon}.rs`). Even a real handle would only buy a
/// heuristic, not a hard guarantee: ConPTY translates the child's console output to VT on its own
/// thread and writes to the pipe asynchronously, so a non-empty-pipe reading race exists inside
/// ConPTY's own pipeline regardless of what jerry-pty can observe from outside it.
///
/// So this waits for the reader-idle signal that is actually observable: once `Child::wait`
/// returns, `Exited` is sent only after `delivering` has read `false` ("Reading", not mid-delivery
/// of a chunk) continuously for [`WINDOWS_EXIT_QUIET_WINDOW`] - any `true` read during that window
/// restarts it, since a chunk was in flight to `output_tx`. A reader still idle several
/// milliseconds after the child was reaped is not certain proof nothing more is coming (a
/// descendant could still be writing), which is why this is a heuristic and not the same guarantee
/// unix has, but a `ReadFile` on data already sitting in the kernel pipe buffer returns near
/// instantly, so an idle window this short is strong real evidence in the common case.
/// [`WINDOWS_EXIT_GRACE_DEADLINE`] bounds the wait so a descendant that never goes quiet cannot
/// stall exit reporting forever - past it, `Exited` is sent regardless.
#[cfg(windows)]
fn run_wait_loop(
    mut child: Box<dyn Child + Send + Sync>,
    mut output_tx: futures_mpsc::Sender<PtyOutput>,
    delivering: Arc<AtomicBool>,
) -> ExitStatus {
    let status = child
        .wait()
        .unwrap_or_else(|err| ExitStatus::with_signal(&format!("wait() failed: {err}")));

    let grace_deadline = Instant::now() + WINDOWS_EXIT_GRACE_DEADLINE;
    let mut quiet_since: Option<Instant> = None;
    while Instant::now() < grace_deadline {
        if delivering.load(Ordering::SeqCst) {
            quiet_since = None; // a chunk is in flight; the window restarts once it clears
        } else {
            let quiet_start = *quiet_since.get_or_insert_with(Instant::now);
            if quiet_start.elapsed() >= WINDOWS_EXIT_QUIET_WINDOW {
                break;
            }
        }
        std::thread::sleep(WINDOWS_EXIT_POLL_INTERVAL);
    }

    let _ = block_on(output_tx.send(PtyOutput::Exited(status.clone())));
    status
}

/// The writer thread, so the pty's possibly-blocking `write` never happens on a caller's thread.
/// Exits when its `Sender` drops or a write fails, raising `failed` first.
fn run_writer_loop(
    mut writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    failed: Arc<AtomicBool>,
) {
    while let Ok(data) = rx.recv() {
        if writer.write_all(&data).is_err() || writer.flush().is_err() {
            failed.store(true, Ordering::SeqCst);
            break;
        }
    }
}

impl PtySession {
    /// Hands over this session's output stream - raw bytes plus the child's exit status as the
    /// final item (see [`PtyOutput`]) - for the caller to hold and await independently of every
    /// other `PtySession` method. Ownership, not a borrow, because the whole point is to
    /// `.await` it across suspension points a `&mut PtySession` could never survive (a GPUI
    /// entity update closure, for one - see `crate::terminal::pane` in `crates/jerry-app`).
    ///
    /// Bounded, so an undrained receiver backpressures the pty instead of growing memory - see
    /// `OUTPUT_CHANNEL_CAPACITY`'s docs. `None` if already taken; a session's stream is claimed
    /// exactly once, immediately after [`spawn`].
    pub fn take_output(&mut self) -> Option<futures_mpsc::Receiver<PtyOutput>> {
        self.output_rx.take()
    }

    /// Enqueues `data` for the pty's input side, as if typed. The write happens on a background
    /// thread, so this does not block even when the child is not reading.
    ///
    /// `Ok` means the bytes reached the writer thread, **not** the fd - awaiting that would
    /// reintroduce the blocking this indirection avoids. It does promise the writer thread is
    /// still alive, so a caller treating `Ok` as "delivered" cannot keep succeeding into a pipe
    /// that stopped going anywhere.
    pub fn write_input(&self, data: &[u8]) -> Result<(), PtyError> {
        if self.writer_failed.load(Ordering::SeqCst) {
            return Err(PtyError::WriterClosed);
        }
        self.writer_tx
            .as_ref()
            .ok_or(PtyError::WriterClosed)?
            .send(data.to_vec())
            .map_err(|_| PtyError::WriterClosed)
    }

    /// Whether the writer thread has already failed a write; see [`Self::write_input`].
    pub fn writer_failed(&self) -> bool {
        self.writer_failed.load(Ordering::SeqCst)
    }

    /// Resizes the pty, which the kernel signals to the child (`SIGWINCH` on unix).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .as_ref()
            .ok_or(PtyError::AlreadyShutDown)?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| PtyError::Resize(err.to_string()))
    }

    /// The child's OS pid, if the platform exposes one and one was cached at spawn time.
    pub fn process_id(&self) -> Option<u32> {
        self.pid
    }

    /// Polls the child's exit status without blocking; `Ok(None)` while it is still running.
    ///
    /// Non-blocking because [`run_wait_loop`]'s dedicated thread, not this call, is what
    /// actually blocks in [`Child::wait`] - this only checks (via `JoinHandle::is_finished`)
    /// whether that thread has finished yet, and claims its result the first time it has.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, PtyError> {
        if self.exited.is_none()
            && self
                .wait_thread
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
        {
            if let Some(handle) = self.wait_thread.take() {
                self.exited = handle.join().ok();
            }
        }
        Ok(self.exited.clone())
    }

    /// Signals the child's process group and any escaped descendants with `SIGHUP` then `SIGKILL`,
    /// no grace between.
    ///
    /// Non-blocking: does not wait for termination or join the threads (the real exit status
    /// still arrives, asynchronously, as a [`PtyOutput::Exited`] item once
    /// [`run_wait_loop`]'s thread reaps it). Use [`shutdown`](PtySession::shutdown) to block for
    /// that.
    #[cfg(unix)]
    pub fn kill(&mut self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        if let Some(pid) = self.process_id() {
            terminate_process_tree(pid, Duration::ZERO);
        }
        Ok(())
    }

    /// Terminates the child's whole process tree via [`windows_terminate_process_tree`], then
    /// the direct child itself (via [`Self::killer`]) as the backstop.
    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        if let Some(pid) = self.process_id() {
            windows_terminate_process_tree(pid);
        }
        if let Some(killer) = self.killer.as_mut() {
            let _ = killer.kill();
        }
        Ok(())
    }

    /// `SIGSTOP`s the child's process group *and* any descendants that escaped it, so a caller can
    /// freeze an agent and everything it spawned - a build step, a tool call - while files are
    /// rewritten underneath it. Counterpart to [`Self::resume`].
    ///
    /// Covering escaped descendants is the point: stopping only the direct child leaves
    /// grandchildren running and still writing files, which is the hazard pausing exists to remove.
    ///
    /// A no-op, not an error, if the child has exited or the platform exposes no pid. `killpg` is
    /// the primary target and errors if it fails; individual descendants are best-effort, so one
    /// already-exited descendant cannot stop the group being paused.
    #[cfg(unix)]
    pub fn pause(&self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        let Some(pid) = self.process_id() else {
            return Ok(());
        };
        // A stopped process cannot fork, so this walk is the complete set as of the `killpg`.
        let descendants = collect_descendant_pids(pid);
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGSTOP)
            .map_err(|err| PtyError::Signal(err.to_string()))?;
        for descendant in &descendants {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*descendant as i32),
                nix::sys::signal::Signal::SIGSTOP,
            );
        }
        Ok(())
    }

    /// `SIGCONT`s what [`Self::pause`] stopped, in reverse order. Safe on a process that was never
    /// paused: `SIGCONT` on a running process is a no-op.
    #[cfg(unix)]
    pub fn resume(&self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        let Some(pid) = self.process_id() else {
            return Ok(());
        };
        // A stopped tree cannot fork, so re-walking finds exactly what `pause` froze.
        let descendants = collect_descendant_pids(pid);
        for descendant in descendants.iter().rev() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*descendant as i32),
                nix::sys::signal::Signal::SIGCONT,
            );
        }
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGCONT)
            .map_err(|err| PtyError::Signal(err.to_string()))
    }

    /// Windows has no `SIGSTOP` equivalent without job objects, so this errors rather than
    /// pretending to pause. Callers only offer the action where it can succeed.
    #[cfg(windows)]
    pub fn pause(&self) -> Result<(), PtyError> {
        Err(PtyError::Signal(
            "pausing a process is not supported on this platform".to_string(),
        ))
    }

    /// See [`Self::pause`]'s Windows twin: there is nothing to resume from either.
    #[cfg(windows)]
    pub fn resume(&self) -> Result<(), PtyError> {
        Err(PtyError::Signal(
            "resuming a process is not supported on this platform".to_string(),
        ))
    }

    /// Tears the session down deterministically: `SIGHUP`, a grace period, `SIGKILL`, block until
    /// the child is reaped, then stop and join the reader and writer threads.
    ///
    /// Blocks for up to [`SHUTDOWN_GRACE_PERIOD`] plus however long the child takes to exit, so
    /// call it from a background task. Safe to call more than once.
    #[cfg(unix)]
    pub fn shutdown(&mut self) -> Result<(), PtyError> {
        if self.exited.is_none() {
            if let Some(pid) = self.process_id() {
                terminate_process_tree(pid, SHUTDOWN_GRACE_PERIOD);
            }
            // The wait thread's only job is one blocking `Child::wait` call - joining it *is*
            // "block until reaped, then know the status", this method's whole contract.
            if let Some(handle) = self.wait_thread.take() {
                self.exited = Some(handle.join().map_err(|_| {
                    PtyError::Wait(std::io::Error::other("the pty exit-wait thread panicked"))
                })?);
            }
        }

        if let Some(mut write) = self.shutdown_write.take() {
            // Best-effort: on failure the reader just runs until the pty closes anyway.
            let _ = write.write_all(&[1u8]);
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }

        self.writer_tx = None; // closes the channel, ending the writer thread's recv loop
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }

        Ok(())
    }

    /// Kills the direct child, blocks until it is reaped, then drops the ConPTY master handle
    /// *before* joining the reader thread.
    ///
    /// That ordering is load-bearing. Nothing can interrupt this platform's blocked `read` except
    /// ConPTY closing, which needs every reference to `master` gone; `child.kill()` calls
    /// `TerminateProcess` and never touches it. Deferring the join to a detached thread instead
    /// would return promptly but leave two blocked OS threads per session alive until something
    /// else dropped the session - making `shutdown` depend on its caller's cleanup to finish.
    #[cfg(windows)]
    pub fn shutdown(&mut self) -> Result<(), PtyError> {
        if self.exited.is_none() {
            if let Some(pid) = self.process_id() {
                windows_terminate_process_tree(pid);
            }
            if let Some(killer) = self.killer.as_mut() {
                let _ = killer.kill();
            }
            // The wait thread's only job is one blocking `Child::wait` call - joining it *is*
            // "block until reaped, then know the status", this method's whole contract.
            if let Some(handle) = self.wait_thread.take() {
                self.exited = Some(handle.join().map_err(|_| {
                    PtyError::Wait(std::io::Error::other("the pty exit-wait thread panicked"))
                })?);
            }
        }

        // All three of these are load-bearing, and the order is too. `ClosePseudoConsole` runs
        // only once *every* `Arc` to the shared inner state is gone, and the writer taken from
        // `master` holds one as well. Dropping `master` while the writer thread still owns its
        // handle leaves conhost holding the output pipe, and the reader blocked in `read` forever.
        self.writer_tx = None; // closes the channel, ending the writer thread's recv loop
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }

        self.master = None;

        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }

        Ok(())
    }
}

/// Terminates `pid` and every descendant, synchronously: `taskkill /T` walks the parent-pid
/// chain, the closest Windows equivalent of the unix process-group signal plus descendant walk,
/// without job-object `unsafe` FFI in this crate (the process-wide kill-on-close job object
/// backstopping a Jerry that dies without running `Drop` lives in `crates/jerry-app`'s `job_object`
/// module - GitHub issue #482; this walk covers the in-session kill paths). Best-effort by nature (a
/// descendant that re-parented is missed; one that exited already is fine) - but without it a
/// bare `TerminateProcess` on the direct child orphans every grandchild, which is how an npm
/// `.cmd`-shim agent's real `node.exe` survived kills (GitHub issue #468) and how a discarded
/// worktree's directory stayed open-handled through its own deletion (GitHub issue #470).
/// `taskkill` ships with every Windows since XP; a failure to run it is logged and the direct
/// kill still proceeds.
#[cfg(windows)]
fn windows_terminate_process_tree(pid: u32) {
    match crate::new_std_command("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output()
    {
        Ok(_) => {}
        Err(err) => log::warn!("taskkill /T /F /PID {pid} could not run: {err}"),
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exited.is_none() {
            #[cfg(unix)]
            if let Some(pid) = self.process_id() {
                // Zero grace: `Drop` must not block the caller.
                terminate_process_tree(pid, Duration::ZERO);
            }
            // Fire-and-forget `taskkill /T` (spawned, never waited - `Drop` must not block),
            // then the direct kill as the backstop; see `windows_terminate_process_tree`.
            // Stdio nulled so the detached child can't hold inherited pipes open - under
            // `cargo nextest` an inherited stdout is reported as the test leaking.
            #[cfg(windows)]
            {
                if let Some(pid) = self.process_id() {
                    let _ = crate::new_std_command("taskkill")
                        .args(["/T", "/F", "/PID", &pid.to_string()])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
                if let Some(killer) = self.killer.as_mut() {
                    let _ = killer.kill();
                }
            }

            // `self.wait_thread` (if not already claimed by `try_wait`/`shutdown`) still owns
            // `child` and is blocked in exactly one `Child::wait` call - dropping the handle here
            // without joining detaches that thread rather than blocking `Drop` on it, the same
            // "reap on a thread nobody waits for" outcome the old `child.take()` fallback below
            // used to build by hand.
        }

        if let Some(mut write) = self.shutdown_write.take() {
            let _ = write.write_all(&[1u8]);
        }
        // Not joined here: joining would block the caller, and both threads exit on their own now
        // the process is dying and the shutdown byte is written. Dropping the handles detaches.
    }
}

/// Reads the current direct children of `pid` from Linux's `/proc/<pid>/task/<pid>/children`.
/// Best-effort: returns an empty list if the file can't be read (process already gone,
/// a unix without procfs, permissions, etc.) rather than erroring - this is used for cleanup,
/// where "found nothing to additionally clean up" is an acceptable fallback.
#[cfg(all(unix, not(target_os = "macos")))]
fn child_pids_of(pid: u32) -> Vec<u32> {
    let path = PathBuf::from(format!("/proc/{pid}/task/{pid}/children"));
    std::fs::read_to_string(&path)
        .map(|contents| {
            contents
                .split_whitespace()
                .filter_map(|token| token.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the current direct children of `pid` from macOS's `libproc`, which has no `/proc` for
/// the branch above to read. Best-effort in exactly the same way: an empty list, never an
/// error, when the process is already gone or cannot be queried.
///
/// `proc_listchildpids` returns the number of pids it wrote and truncates *silently* when the
/// buffer is too small - it reports the capacity it filled with no error of any kind - so a
/// completely full buffer is retried at double the capacity rather than trusted to be complete.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn child_pids_of(pid: u32) -> Vec<u32> {
    // A parent with more direct children than this is pathological, not a real agent process
    // tree; the doubling below stops here rather than growing without bound.
    const MAX_CAPACITY: usize = 4096;

    let Ok(ppid) = libc::pid_t::try_from(pid) else {
        return Vec::new();
    };

    let mut capacity = 64usize;
    loop {
        let mut buffer: Vec<libc::pid_t> = vec![0; capacity];
        let buffer_bytes = capacity * std::mem::size_of::<libc::pid_t>();

        // SAFETY: `proc_listchildpids` writes at most `buffersize` bytes through the buffer
        // pointer, which addresses a live, uniquely borrowed `Vec` allocation of exactly
        // `buffer_bytes` bytes for the whole call and is not retained by the callee. The cast
        // is the one Apple's own header requires - the parameter is a bare `void *`. It reports
        // how many pids it wrote, or 0 on failure, and never a negative count.
        let written = unsafe {
            libc::proc_listchildpids(
                ppid,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer_bytes as libc::c_int,
            )
        };

        let written = usize::try_from(written).unwrap_or(0).min(capacity);
        if written < capacity || capacity == MAX_CAPACITY {
            buffer.truncate(written);
            return buffer
                .into_iter()
                .filter_map(|child| u32::try_from(child).ok())
                .collect();
        }
        capacity = (capacity * 2).min(MAX_CAPACITY);
    }
}

/// Breadth-first, depth-capped walk of `root_pid`'s descendant tree via [`child_pids_of`]. Must
/// be called *before* signaling anything: reading it after a process starts dying races
/// against the kernel reparenting its children out from under it.
#[cfg(unix)]
fn collect_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut discovered = Vec::new();
    let mut visited: HashSet<u32> = HashSet::new();
    visited.insert(root_pid);

    let mut frontier = vec![root_pid];
    for _ in 0..TREE_WALK_MAX_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for pid in frontier {
            for child in child_pids_of(pid) {
                if visited.insert(child) {
                    discovered.push(child);
                    next.push(child);
                }
            }
        }
        frontier = next;
    }
    discovered
}

/// Whether `pid` is still a live (or zombie-but-unreaped) process, via the portable POSIX
/// `kill(pid, 0)` existence probe - no procfs, so this answers the same on every unix.
///
/// `EPERM` counts as "exists": the kernel only reports it for a process that is genuinely
/// there but not signalable by this user, and treating that as gone would make
/// [`terminate_process_tree`]'s grace loop declare victory over something still running.
#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None) {
        Ok(()) => true,
        Err(errno) => errno == nix::errno::Errno::EPERM,
    }
}

/// Terminates `root_pid`'s process group *and* any descendants that escaped it: `SIGHUP`, up to
/// `grace` of polling for voluntary exit, then `SIGKILL`.
///
/// Never blocks on `waitpid`. Individual signal errors are ignored - the job is that nothing
/// survives, not reporting which of many targets could be signalled.
#[cfg(unix)]
fn terminate_process_tree(root_pid: u32, grace: Duration) {
    let descendants = collect_descendant_pids(root_pid);
    let pgid = nix::unistd::Pid::from_raw(root_pid as i32);

    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGHUP);
    for pid in &descendants {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(*pid as i32),
            nix::sys::signal::Signal::SIGHUP,
        );
    }

    if !grace.is_zero() {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            let all_gone = !pid_exists(root_pid) && descendants.iter().all(|pid| !pid_exists(*pid));
            if all_gone {
                break;
            }
            std::thread::sleep(SHUTDOWN_GRACE_POLL_INTERVAL);
        }
    }

    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    for pid in &descendants {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(*pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(test)]
mod pty_session_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// How long a real child process is given to reach a state before a test calls it a
    /// failure. Generous: it has to survive a full-suite run where other tests' own child
    /// processes are competing for the same cores. `test_support::wait_until` returns as soon as
    /// the condition holds, so an idle machine pays none of it.
    ///
    /// `#[cfg(unix)]` because every one of its use sites is: the process-tree teardown and
    /// SIGSTOP/SIGCONT state assertions this bounds have no Windows twin (see this module's own
    /// header). Without the gate it is dead code on Windows - which nothing caught until clippy
    /// started running on that target.
    #[cfg(unix)]
    const TEARDOWN_DEADLINE: Duration = Duration::from_secs(5);

    /// Prints `text` and exits.
    fn echo_command(text: &str) -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd.exe").arg("/c").arg("echo").arg(text)
        } else {
            SpawnOptions::new("echo").arg(text)
        }
    }

    /// Prints every integer from 1 to `n` on its own line, then exits.
    fn counting_command(n: usize) -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd.exe")
                .arg("/c")
                .arg(format!("for /l %i in (1,1,{n}) do @echo %i"))
        } else {
            SpawnOptions::new("seq").arg("1").arg(n.to_string())
        }
    }

    /// Stays alive and lets whatever is written to the pty come back out of it - `cat` on unix,
    /// and on Windows the shell itself, since ConPTY does the echoing rather than the child.
    fn stdin_echoing_command() -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd.exe")
        } else {
            SpawnOptions::new("cat")
        }
    }

    /// Stays alive for a couple of seconds without needing input.
    fn long_lived_command() -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd.exe")
        } else {
            SpawnOptions::new("sleep").arg("2")
        }
    }

    /// Exits successfully, immediately.
    fn quick_exit_command() -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions::new("cmd.exe").arg("/c").arg("exit")
        } else {
            SpawnOptions::new("true")
        }
    }

    /// ConPTY's startup Device Status Report query. It emits this and then produces no child
    /// output at all until something answers, because it is waiting to learn where the cursor is.
    #[cfg(windows)]
    const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

    /// The minimal valid cursor position report: row 1, column 1.
    #[cfg(windows)]
    const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

    /// Answers [`CURSOR_POSITION_QUERY`] the first time it appears, standing in for the terminal
    /// emulator a real consumer has. `crates/jerry-app`'s pane answers it from `alacritty_terminal`'s
    /// `Term::device_status`; a test holding a bare [`PtySession`] has no emulator, so without
    /// this every read on this platform blocks until the harness kills it.
    #[cfg(windows)]
    fn answer_cursor_position_query(session: &PtySession, seen: &[u8], answered: &mut bool) {
        if !*answered
            && seen
                .windows(CURSOR_POSITION_QUERY.len())
                .any(|window| window == CURSOR_POSITION_QUERY)
        {
            let _ = session.write_input(CURSOR_POSITION_REPORT);
            *answered = true;
        }
    }

    /// No-op off Windows: only ConPTY withholds output pending this answer, and injecting the
    /// report into a unix shell's stdin would have it try to run `^[[1;1R` as a command.
    #[cfg(not(windows))]
    fn answer_cursor_position_query(_session: &PtySession, _seen: &[u8], _answered: &mut bool) {}

    /// Drops CSI/OSC escape sequences, so an assertion can compare the text a shell printed
    /// rather than the cursor moves and colour changes ConPTY wraps it in.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch != '\u{1b}' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                // CSI: parameters and intermediates, then one final byte in `@`..=`~`.
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: runs to BEL or the ST that follows an ESC.
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        }
        out
    }

    /// `test_support::wait_until` over a non-blocking `try_recv`, standing in for
    /// `std::sync::mpsc::Receiver::recv_timeout` - the async channel has no blocking-with-timeout
    /// receive of its own.
    fn recv_timeout(
        rx: &mut futures_mpsc::Receiver<PtyOutput>,
        timeout: Duration,
    ) -> Option<PtyOutput> {
        let mut item = None;
        test_support::wait_until(timeout, || match rx.try_recv() {
            Ok(value) => {
                item = Some(value);
                true
            }
            Err(err) if err.is_closed() => true, // channel closed with nothing left; give up
            Err(_empty) => false,
        });
        item
    }

    /// Reads from `rx` until `needle` appears in the accumulated (lossy UTF-8) output or
    /// `timeout` elapses, returning whatever was collected either way. Returns as soon as the
    /// needle is found rather than always waiting out the full timeout, so tests aren't
    /// needlessly slow. A trailing [`PtyOutput::Exited`] or a closed channel ends the drain the
    /// same way a timeout would - there is nothing further to collect either way.
    fn drain_until_contains(
        session: &PtySession,
        rx: &mut futures_mpsc::Receiver<PtyOutput>,
        needle: &str,
        timeout: Duration,
    ) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut answered = false;
        let deadline = Instant::now() + timeout;
        loop {
            answer_cursor_position_query(session, &collected, &mut answered);
            if String::from_utf8_lossy(&collected).contains(needle) {
                return collected;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return collected;
            }
            match recv_timeout(rx, remaining) {
                Some(PtyOutput::Bytes(chunk)) => collected.extend_from_slice(&chunk),
                Some(PtyOutput::Exited(_)) | None => return collected,
            }
        }
    }

    /// Reads until a line starts with `prefix`, returning the rest of it - so a spawned shell can
    /// report a background job's pid deterministically instead of racing `/proc` to find it.
    #[cfg(unix)]
    fn read_line_after_prefix(
        rx: &mut futures_mpsc::Receiver<PtyOutput>,
        prefix: &str,
        timeout: Duration,
    ) -> Option<String> {
        let mut collected = String::new();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = collected.find(prefix) {
                let rest = &collected[pos + prefix.len()..];
                if let Some(end) = rest.find(['\r', '\n']) {
                    return Some(rest[..end].to_string());
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match recv_timeout(rx, remaining) {
                Some(PtyOutput::Bytes(chunk)) => {
                    collected.push_str(&String::from_utf8_lossy(&chunk))
                }
                Some(PtyOutput::Exited(_)) | None => return None,
            }
        }
    }

    #[test]
    fn spawns_and_reads_short_process_output() {
        let mut session = spawn(echo_command("hello-jerry-pty"))
            .expect("spawning an echo command should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");

        let output =
            drain_until_contains(&session, &mut rx, "hello-jerry-pty", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello-jerry-pty"),
            "expected pty output to contain the echoed text, got: {text:?}"
        );
    }

    #[test]
    fn long_output_arrives_complete_and_in_order_across_chunk_boundaries() {
        // Enough lines to cross `OUTPUT_CHANNEL_CAPACITY` many times over, which is the point of
        // the test. `cmd`'s `for /l` is orders of magnitude slower per line than `seq`, so Windows
        // gets a smaller count rather than a longer timeout.
        const LINES: usize = if cfg!(windows) { 20_000 } else { 200_000 };

        let mut session =
            spawn(counting_command(LINES)).expect("spawning a counting command should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");

        let mut collected = Vec::new();
        let mut received = 0usize;
        let mut answered = false;
        // Counted incrementally: rescanning the whole buffer per chunk would be quadratic. ConPTY
        // does not disconnect promptly when the child exits, so without this the loop would always
        // wait out the full deadline on Windows.
        let mut newlines = 0usize;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            answer_cursor_position_query(&session, &collected, &mut answered);
            if newlines >= LINES {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match recv_timeout(&mut rx, remaining) {
                Some(PtyOutput::Bytes(chunk)) => {
                    newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
                    collected.extend_from_slice(&chunk);
                    received += 1;
                    if received.is_multiple_of(OUTPUT_CHANNEL_CAPACITY) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                // Guaranteed the stream's last item on unix (see `PtyOutput::Exited`'s docs), so
                // ending the drain here is a real assertion that nothing was still in flight -
                // Windows has no such guarantee, so it keeps draining past this item instead.
                Some(PtyOutput::Exited(_)) => {
                    if cfg!(unix) {
                        break;
                    }
                }
                None => break,
            }
        }

        // ConPTY prefixes the child's first line with its own mode-setting and title sequences.
        let text = strip_ansi(&String::from_utf8(collected).expect("counted output is ASCII"));
        let normalized = text.replace('\r', "");
        let lines: Vec<&str> = normalized.trim_end_matches('\n').split('\n').collect();

        assert_eq!(
            lines.len(),
            LINES,
            "expected every one of the {LINES} lines to arrive exactly once; got {} (first \
             line {:?}, last line {:?})",
            lines.len(),
            lines.first(),
            lines.last(),
        );
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(
                *line,
                (index + 1).to_string(),
                "line {index} is out of order or corrupted - the byte stream was reordered, \
                 truncated or duplicated somewhere around a chunk boundary",
            );
        }
    }

    /// The two answers [`pid_exists`] has to get right for every teardown assertion below to
    /// mean anything: a process that is genuinely running, and one that has already been
    /// reaped.
    ///
    /// unix-only: uses `pid_exists` directly, which is a `#[cfg(unix)]` helper function (see
    /// the crate-level "Platform scope" docs).
    #[cfg(unix)]
    #[test]
    fn pid_exists_separates_a_live_process_from_a_reaped_one() {
        assert!(
            pid_exists(std::process::id()),
            "this very test process is unambiguously alive"
        );

        // `ChildGuard` even though this child is expected to exit on its own and is reaped
        // explicitly below: if the assertion above ever fails, the unwind must still not leave a
        // process behind (`docs/testing.md`'s teardown rule).
        let mut command = std::process::Command::new("true");
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child =
            test_support::ChildGuard::spawn(&mut command).expect("spawning `true` should succeed");
        let pid = child.id();
        child.wait().expect("reaping `true` should succeed");

        assert!(
            !pid_exists(pid),
            "pid {pid} was reaped, so it should read as gone rather than alive"
        );
    }

    // unix-only: uses pid_exists/is_executable directly, which are #[cfg(unix)]
    // helper functions (see the crate-level "Platform scope" docs).
    #[cfg(unix)]
    #[test]
    fn drop_kills_child_and_it_does_not_become_an_orphan() {
        let session = spawn(SpawnOptions::new("sleep").arg("100"))
            .expect("spawning `sleep 100` should succeed");

        let pid = session
            .process_id()
            .expect("a spawned unix child should report a pid");

        assert!(
            pid_exists(pid),
            "child pid {pid} should be alive immediately after spawn"
        );

        drop(session);

        assert!(
            test_support::wait_until(TEARDOWN_DEADLINE, || !pid_exists(pid)),
            "child pid {pid} was still alive {TEARDOWN_DEADLINE:?} after PtySession was dropped \
             - orphaned process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_terminates_entire_process_tree_including_escaped_grandchild() {
        // A process that escapes the group via its own `setsid()` is unreachable by `killpg`
        // alone. The shell reports its pid over the pty rather than us racing `/proc` for it.
        let mut session = spawn(
            SpawnOptions::new("sh")
                .arg("-c")
                .arg("set -m; sleep 100 & echo GRANDCHILD:$!; exec sleep 300"),
        )
        .expect("spawning the shell pipeline should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");

        let direct_pid = session
            .process_id()
            .expect("a spawned unix child should report a pid");

        let grandchild_pid = read_line_after_prefix(&mut rx, "GRANDCHILD:", Duration::from_secs(5))
            .and_then(|line| line.trim().parse::<u32>().ok())
            .expect("shell should report the detached grandchild's pid over the pty");

        assert!(
            pid_exists(direct_pid),
            "direct child {direct_pid} should be alive right after spawn"
        );
        assert!(
            pid_exists(grandchild_pid),
            "detached grandchild {grandchild_pid} should be alive before drop"
        );
        // `pid_exists` alone would also be satisfied by an unreaped zombie, which is what this
        // test degraded into on macOS before; the walk only lists a pid the kernel still
        // reports as a child, and it is the mechanism actually under test here.
        assert!(
            collect_descendant_pids(direct_pid).contains(&grandchild_pid),
            "the descendant walk should discover the detached grandchild {grandchild_pid} \
             under direct child {direct_pid} - without it, `Drop` has no way to reach it"
        );

        drop(session);

        assert!(
            test_support::wait_until(TEARDOWN_DEADLINE, || !pid_exists(direct_pid)
                && !pid_exists(grandchild_pid)),
            "direct child ({direct_pid}) or its escaped grandchild ({grandchild_pid}) was still \
             alive {TEARDOWN_DEADLINE:?} after PtySession was dropped - orphaned process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_reaps_child_deterministically_without_lingering_zombie() {
        let mut session = spawn(SpawnOptions::new("sleep").arg("100"))
            .expect("spawning `sleep 100` should succeed");
        let pid = session
            .process_id()
            .expect("a spawned unix child should report a pid");

        session.shutdown().expect("shutdown should succeed");

        assert!(
            !pid_exists(pid),
            "child pid {pid} should be fully reaped (not even a zombie) immediately \
             after shutdown() returns"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pause_really_stops_the_process_and_resume_really_restarts_it() {
        fn proc_state(pid: u32) -> String {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .expect("reading /proc/<pid>/status should succeed while the process is alive");
            status
                .lines()
                .find_map(|line| line.strip_prefix("State:"))
                .map(|rest| rest.trim().to_string())
                .expect("State: line should be present")
        }

        let session = spawn(SpawnOptions::new("sleep").arg("100")).expect("spawning `sleep 100`");
        let pid = session
            .process_id()
            .expect("a spawned unix child should report a pid");

        let steady = |pid| {
            let state = proc_state(pid);
            state.starts_with('S') || state.starts_with('R')
        };

        // Give the kernel a moment to settle the freshly spawned process into a steady
        // running/sleeping state before asserting anything about it.
        assert!(
            test_support::wait_until(TEARDOWN_DEADLINE, || steady(pid)),
            "process {pid} never reached a steady state - last observed state: {:?}",
            proc_state(pid)
        );

        session.pause().expect("pause should succeed");
        assert!(
            test_support::wait_until(TEARDOWN_DEADLINE, || proc_state(pid).starts_with('T')),
            "process {pid} never reached the real kernel-reported stopped state (State: T) after \
             pause() - last observed state: {:?}",
            proc_state(pid)
        );

        session.resume().expect("resume should succeed");
        assert!(
            test_support::wait_until(TEARDOWN_DEADLINE, || steady(pid)),
            "process {pid} never left the real kernel-reported stopped state after resume() - \
             last observed state: {:?}",
            proc_state(pid)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pause_and_resume_really_reach_an_escaped_grandchild_process_too() {
        fn proc_state(pid: u32) -> String {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .expect("reading /proc/<pid>/status should succeed while the process is alive");
            status
                .lines()
                .find_map(|line| line.strip_prefix("State:"))
                .map(|rest| rest.trim().to_string())
                .expect("State: line should be present")
        }

        fn wait_for_state(pid: u32, prefix: char, what: &str) {
            assert!(
                test_support::wait_until(TEARDOWN_DEADLINE, || proc_state(pid).starts_with(prefix)),
                "process {pid} never reached {what} - last observed state: {:?}",
                proc_state(pid)
            );
        }

        let mut session = spawn(
            SpawnOptions::new("sh")
                .arg("-c")
                .arg("setsid sleep 100 & echo GRANDCHILD:$!; exec sleep 300"),
        )
        .expect("spawning the shell pipeline should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");

        let grandchild_pid = read_line_after_prefix(&mut rx, "GRANDCHILD:", Duration::from_secs(5))
            .and_then(|line| line.trim().parse::<u32>().ok())
            .expect("shell should report the detached grandchild's pid over the pty");

        wait_for_state(
            grandchild_pid,
            'S',
            "a real steady sleeping state before pause",
        );

        session.pause().expect("pause should succeed");
        wait_for_state(
            grandchild_pid,
            'T',
            "the real kernel-reported stopped state (State: T) after pause() - a plain \
             kill(pid, SIGSTOP) against only the direct child would never reach this escaped \
             grandchild at all",
        );

        session.resume().expect("resume should succeed");
        wait_for_state(
            grandchild_pid,
            'S',
            "a real steady sleeping state again after resume()",
        );
    }

    #[cfg(unix)]
    #[test]
    fn pause_and_resume_are_a_harmless_no_op_once_the_child_has_already_exited() {
        let mut session = spawn(SpawnOptions::new("true")).expect("spawning `true`");
        assert!(
            test_support::wait_until(TEARDOWN_DEADLINE, || session
                .try_wait()
                .expect("try_wait should not error")
                .is_some()),
            "`true` never exited"
        );
        session
            .pause()
            .expect("pause on an already-exited child must be a harmless no-op");
        session
            .resume()
            .expect("resume on an already-exited child must be a harmless no-op");
    }

    /// The channel-native replacement for the old EOF-then-poll `try_wait` dance: a quick child's
    /// real exit status must arrive as a [`PtyOutput::Exited`] item on the same stream as its
    /// bytes, on every platform - this is what lets a consumer simply await the stream instead of
    /// separately polling `try_wait` after inferring EOF (`docs/architecture/decisions.md` §8's
    /// amendment).
    #[test]
    fn a_quick_exit_is_delivered_as_an_exited_item_on_the_output_channel() {
        let mut session =
            spawn(quick_exit_command()).expect("spawning a quick-exit command should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");

        // On Windows, ConPTY's own startup cursor-position query blocks the child's entire
        // output (and, transitively, its exit) until something answers it - see
        // `answer_cursor_position_query`'s docs. A bare consumer that only drains bytes without
        // answering would make this test hang for the same reason a real, un-instrumented
        // caller would need to answer it via a real terminal emulator.
        let mut collected = Vec::new();
        let mut answered = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        let exited = loop {
            answer_cursor_position_query(&session, &collected, &mut answered);
            match recv_timeout(&mut rx, deadline.saturating_duration_since(Instant::now())) {
                Some(PtyOutput::Exited(status)) => break Some(status),
                Some(PtyOutput::Bytes(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    continue;
                }
                None => break None,
            }
        };

        assert!(
            exited.is_some(),
            "the child's real exit status must arrive on the output channel, not just be \
             inferable from the channel closing"
        );
        assert!(
            exited.expect("checked above").success(),
            "a clean `exit 0`/`true` must report success"
        );
    }

    // Pins the ordering guarantee `PtyOutput::Exited`'s docs make: a hard guarantee on unix, a
    // strong heuristic on Windows (see `run_wait_loop`'s docs on each platform) - both platforms
    // are expected to hold it here, so the assertions below are identical; only the spawned
    // command and the ConPTY cursor-query handshake (a no-op off Windows) differ.
    #[test]
    fn exited_is_always_the_last_item_after_every_byte_the_child_wrote() {
        // The race this guards against (when it existed) was probabilistic - a single spawn
        // reliably passed even on the old, racy design, so this repeats many independent spawns
        // rather than trusting one to catch it.
        const ITERATIONS: usize = 50;
        const MARKER: &str = "jerry-pty-exit-ordering-marker";
        // How long to keep watching for a stray `Bytes` chunk after `Exited` arrives. Real time,
        // not `deadline`'s remaining budget: on Windows the reader thread stays alive and
        // `output_tx` open well past `Exited` (nothing unblocks its `read` early - see
        // `run_wait_loop`'s Windows docs), so reusing the full per-iteration deadline here would
        // just wait out a channel close that was never coming, rather than actually watching for
        // a violation.
        const POST_EXIT_WATCH: Duration = Duration::from_millis(300);

        for iteration in 0..ITERATIONS {
            let mut session = spawn(if cfg!(windows) {
                SpawnOptions::new("cmd.exe")
                    .arg("/c")
                    .arg(format!("echo {MARKER}"))
            } else {
                SpawnOptions::new("sh")
                    .arg("-c")
                    .arg(format!("printf '%s' '{MARKER}'"))
            })
            .expect("spawning a printing command should succeed");
            let mut rx = session
                .take_output()
                .expect("a freshly spawned session must still have its output stream");

            let mut collected = Vec::new();
            let mut exited_status = None;
            let mut answered = false;
            // Generous relative to `WINDOWS_EXIT_GRACE_DEADLINE`'s own worst case, so a run under
            // heavy machine load fails on a real ordering violation, not on exhausting a tight
            // per-iteration budget before the (already-bounded) exit signal had a chance to land.
            let mut deadline = Instant::now() + Duration::from_secs(10);
            loop {
                answer_cursor_position_query(&session, &collected, &mut answered);
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match recv_timeout(&mut rx, remaining) {
                    Some(PtyOutput::Bytes(chunk)) => {
                        assert!(
                            exited_status.is_none(),
                            "iteration {iteration}: a `Bytes` chunk arrived after `Exited`"
                        );
                        collected.extend_from_slice(&chunk);
                    }
                    Some(PtyOutput::Exited(status)) => {
                        assert!(
                            exited_status.is_none(),
                            "iteration {iteration}: more than one `Exited` item arrived"
                        );
                        exited_status = Some(status);
                        deadline = deadline.min(Instant::now() + POST_EXIT_WATCH);
                    }
                    None => break,
                }
            }

            assert!(
                exited_status.is_some(),
                "iteration {iteration}: the child's exit status never arrived"
            );
            let text = String::from_utf8_lossy(&collected);
            assert!(
                text.contains(MARKER),
                "iteration {iteration}: expected the marker to have arrived before `Exited`, \
                 got: {text:?}"
            );
        }
    }

    // Unix-only by subject, not just by binary: the mechanism under test is the self-pipe that
    // wakes a reader blocked on a pty whose line discipline has echo turned off. ConPTY has no
    // `stty`, no caller-visible line discipline to turn off, and no self-pipe - its reader is
    // woken by closing the ConPTY handle instead, which `shutdown_reaps_child_deterministically`
    // already covers on that platform.
    #[cfg(unix)]
    #[test]
    fn shutdown_joins_reader_thread_even_with_local_echo_disabled() {
        // With echo off - how most interactive programs run their ptys - the writer's
        // EOT-on-drop is no longer bounced back to wake the reader's blocked `read`, and the
        // independently-dup'd reader fd survives dropping the master. `shutdown()` must still
        // return promptly, via the self-pipe rather than that coincidence.
        let mut session = spawn(SpawnOptions::new("sh").arg("-c").arg("stty -echo; cat"))
            .expect("spawning `sh -c 'stty -echo; cat'` should succeed");

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = session.shutdown();
            let _ = done_tx.send(result.is_ok());
        });

        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(true) => {}
            Ok(false) => panic!("shutdown() returned an error"),
            Err(_) => panic!(
                "shutdown() did not return within 5s - the reader thread likely leaked \
                 (fd-leak regression)"
            ),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn output_channel_backpressures_instead_of_growing_unboundedly() {
        fn read_self_rss_kb() -> u64 {
            let status = std::fs::read_to_string("/proc/self/status")
                .expect("reading /proc/self/status should succeed");
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|rest| rest.trim().trim_end_matches(" kB").parse::<u64>().ok())
                .expect("VmRSS should be present and parseable in /proc/self/status")
        }

        let _session = spawn(SpawnOptions::new("yes")).expect("spawning `yes` should succeed");

        let rss_before = read_self_rss_kb();
        // Deliberately never call `take_output` while `yes` floods the pty as fast as it can, so
        // nothing ever drains the channel. With an unbounded channel this measurably grows RSS
        // (an earlier version of this crate measured ~3.4MB -> ~127MB in 3s against a comparable
        // undrained producer); with the bounded async channel, the reader thread blocks in
        // `send` once the channel fills, which backpressures its `read`, which fills the kernel
        // pty buffer, which blocks `yes`'s `write` - so growth should stay small and bounded.
        //
        // `stays_false` rather than a bare sleep-then-measure: it holds the same 500ms window
        // open while asserting the bound *continuously*, so a transient spike is caught too.
        assert!(
            test_support::stays_false(Duration::from_millis(500), || {
                read_self_rss_kb().saturating_sub(rss_before) >= 20_000
            }),
            "RSS grew by {} kB while an undrained `yes` pipe ran for 500ms - the output channel \
             does not appear to be backpressuring (expected growth bounded by the channel \
             capacity, well under 20MB)",
            read_self_rss_kb().saturating_sub(rss_before)
        );
    }

    #[test]
    fn a_live_session_reports_a_healthy_writer_and_keeps_accepting_writes() {
        let mut session =
            spawn(stdin_echoing_command()).expect("spawning an echoing shell should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");
        assert!(!session.writer_failed());
        for _ in 0..3 {
            session
                .write_input(b"still-writing\n")
                .expect("a healthy writer must keep accepting writes");
        }
        assert!(!session.writer_failed());
        let output =
            drain_until_contains(&session, &mut rx, "still-writing", Duration::from_secs(5));
        assert!(String::from_utf8_lossy(&output).contains("still-writing"));
    }

    #[test]
    fn a_shut_down_session_refuses_writes() {
        let mut session =
            spawn(stdin_echoing_command()).expect("spawning an echoing shell should succeed");
        session.shutdown().expect("shutdown");
        assert!(matches!(
            session.write_input(b"too late"),
            Err(PtyError::WriterClosed)
        ));
    }

    #[test]
    fn write_input_is_echoed_back_by_the_pty_line_discipline() {
        // A cooked-mode pty echoes writes back through the reader regardless of the child; `cat`
        // just keeps the session alive long enough to see it.
        let mut session =
            spawn(stdin_echoing_command()).expect("spawning an echoing shell should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");

        session
            .write_input(b"ping-jerry-pty\n")
            .expect("writing input to a live pty should succeed");

        let output =
            drain_until_contains(&session, &mut rx, "ping-jerry-pty", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("ping-jerry-pty"),
            "expected written input to be echoed back through the pty, got: {text:?}"
        );
    }

    #[test]
    fn resize_on_a_live_session_succeeds() {
        let session =
            spawn(long_lived_command()).expect("spawning a long-lived command should succeed");

        session
            .resize(40, 120)
            .expect("resizing a live pty session should not error");

        let size_after = session
            .master
            .as_ref()
            .expect("session should still have a live master handle")
            .get_size()
            .expect("get_size should succeed after a resize");
        assert_eq!(size_after.rows, 40);
        assert_eq!(size_after.cols, 120);
    }

    #[test]
    fn spawn_reports_typed_error_for_nonexistent_program() {
        // A match, not `.expect_err(..)`: that needs the `Ok` type to be `Debug`, and
        // `PtySession`'s fields are trait objects.
        match spawn(SpawnOptions::new("definitely-not-a-real-binary-xyz")) {
            Err(err) => assert!(matches!(err, PtyError::Spawn(_))),
            Ok(_) => panic!("spawning a nonexistent program should have failed"),
        }
    }

    /// An *interactive* shell, kept alive, plus the line to write to it to make it print its
    /// working directory. A `/c`-style one-shot races ConPTY teardown on Windows: the child can
    /// exit before its output is pumped, leaving only ConPTY's own `ESC[6n` probe behind.
    fn pwd_shell_command() -> (SpawnOptions, &'static str) {
        if cfg!(windows) {
            (SpawnOptions::new("cmd.exe"), "cd\r\n")
        } else {
            (SpawnOptions::new("sh"), "pwd\n")
        }
    }

    #[test]
    fn a_spawned_shell_really_starts_in_the_requested_cwd() {
        let dir = tempfile::tempdir().expect("creating a temp dir should succeed");
        let requested = dir.path().to_path_buf();
        let leaf = requested
            .file_name()
            .expect("a temp dir always has a final component")
            .to_string_lossy()
            .into_owned();

        let (options, pwd_line) = pwd_shell_command();
        let mut session = spawn(options.cwd(requested.clone()))
            .expect("spawning a shell in a real directory should succeed");
        let mut rx = session
            .take_output()
            .expect("a freshly spawned session must still have its output stream");
        session
            .write_input(pwd_line.as_bytes())
            .expect("writing to a freshly spawned shell should succeed");

        let output = drain_until_contains(&session, &mut rx, &leaf, Duration::from_secs(15));
        let text = strip_ansi(&String::from_utf8_lossy(&output));

        // Substring, not line-splitting: ConPTY runs the prompt and the command's output together
        // without a newline between them. Either spelling counts - `/var` is a symlink to
        // `/private/var` on macOS, so a shell there reports the resolved form of what we passed.
        let resolved = std::fs::canonicalize(&requested).expect("canonicalizing the temp dir");
        let accepted = [
            requested.display().to_string(),
            resolved.display().to_string(),
        ];
        assert!(
            accepted.iter().any(|path| text.contains(path)),
            "the shell must start in the directory it was given, not silently somewhere else \
             (portable-pty drops a cwd it cannot use and lets the child inherit ours). \
             Expected one of {accepted:?}, got: {text:?}"
        );
    }

    #[test]
    fn spawn_rejects_nonexistent_cwd_instead_of_silently_falling_back_to_home() {
        let bogus = PathBuf::from("/definitely/not/a/real/directory/jerry-pty-test");
        match spawn(quick_exit_command().cwd(bogus.clone())) {
            Err(PtyError::InvalidCwd(path)) => assert_eq!(path, bogus),
            Err(other) => panic!("expected PtyError::InvalidCwd, got a different error: {other}"),
            Ok(_) => panic!("expected spawn to reject a nonexistent cwd, but it succeeded"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_on_path_finds_a_real_binary_and_the_path_is_itself_real() {
        let resolved = resolve_on_path("sh").expect("`sh` should be found on PATH");
        assert!(
            resolved.is_file(),
            "resolved path {resolved:?} should be a real, existing file"
        );
        assert!(
            is_executable(&resolved),
            "resolved path {resolved:?} should be executable"
        );
        assert!(resolved.is_absolute(), "resolved path should be absolute");
    }

    #[test]
    fn resolve_on_path_returns_none_for_a_binary_that_does_not_exist() {
        assert_eq!(
            resolve_on_path("definitely-not-a-real-binary-xyz-jerry-pty-test"),
            None
        );
    }

    #[test]
    fn resolve_on_path_skips_a_same_named_directory() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let fake_dir_binary = tmp.path().join("not-really-a-binary");
        std::fs::create_dir(&fake_dir_binary).expect("mkdir");

        // A real, isolated PATH - the real process PATH (so a genuine same-named binary
        // elsewhere on it still wouldn't wrongly match first) with the tempdir prepended,
        // so the search actually walks through the directory-shaped candidate before
        // (if ever) finding a real one.
        let real_path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(
            std::iter::once(tmp.path().to_path_buf()).chain(std::env::split_paths(&real_path)),
        )
        .expect("joining PATH entries should not fail for real filesystem paths");

        let result = resolve_in_path_var(&joined, "not-really-a-binary");

        assert_eq!(
            result, None,
            "a same-named directory on PATH must never be returned as a resolved binary"
        );
    }
}

#[cfg(test)]
mod capture_tests {
    use super::{capture_first_line, PtyError};
    use std::time::{Duration, Instant};

    /// A shell invocation that runs `script`, spelled for whichever shell this platform really
    /// has - these tests are about [`capture_first_line`], not about which shell exists.
    fn shell(script: &str) -> (&'static str, Vec<&str>) {
        if cfg!(windows) {
            ("cmd", vec!["/c", script])
        } else {
            ("sh", vec!["-c", script])
        }
    }

    #[test]
    fn the_first_line_a_command_prints_is_what_comes_back() {
        let (program, args) = shell("echo 51a6b5fa-fbf4-4116-b44c-cd3e0aa35a5e");
        let line = capture_first_line(
            program,
            &args,
            &std::env::temp_dir(),
            Duration::from_secs(30),
        )
        .expect("a shell echo should be captured");
        assert_eq!(line, "51a6b5fa-fbf4-4116-b44c-cd3e0aa35a5e");
    }

    #[test]
    fn a_command_that_never_finishes_is_killed_at_the_timeout() {
        // The real motivating case: `cursor-agent create-chat` hangs forever, printing nothing,
        // when the user is not logged in. Without the timeout the caller hangs with it.
        // Spawned directly rather than through a shell: `Child::kill` reaches only the process it
        // started, so a shell wrapper would be killed while its own sleeping child lived on.
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("ping", vec!["-n", "30", "127.0.0.1"])
        } else {
            ("sleep", vec!["30"])
        };
        let started = Instant::now();
        let result = capture_first_line(
            program,
            &args,
            &std::env::temp_dir(),
            Duration::from_millis(300),
        );
        assert!(
            matches!(result, Err(PtyError::CaptureTimeout(_))),
            "a command that outlives its timeout must report the timeout, got {result:?}"
        );
        // Asserted by effect rather than by stopwatch precision: the point is that it returned
        // long before the command's own 30s, not that it returned at exactly 300ms.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must actually cut the wait short"
        );
    }

    #[test]
    fn a_command_that_prints_nothing_is_not_mistaken_for_a_result() {
        let (program, args) = shell("exit 0");
        let result = capture_first_line(
            program,
            &args,
            &std::env::temp_dir(),
            Duration::from_secs(30),
        );
        assert!(
            matches!(result, Err(PtyError::CaptureEmpty(_))),
            "silence must be an error, never an empty id, got {result:?}"
        );
    }

    #[test]
    fn a_binary_that_does_not_exist_fails_instead_of_waiting() {
        let result = capture_first_line(
            "definitely-not-a-real-binary-xyz-jerry-pty-test",
            &[],
            &std::env::temp_dir(),
            Duration::from_secs(30),
        );
        assert!(matches!(result, Err(PtyError::Spawn(_))), "got {result:?}");
    }
}
