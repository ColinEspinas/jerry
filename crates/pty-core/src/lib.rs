//! `pty-core`: spawn, stream, resize and kill a native pseudo-terminal, over `portable-pty`.
//!
//! Knows nothing about ANSI escapes or grid state - `crates/app` owns those. Output streams over a
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

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
#[cfg(unix)]
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
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

pub use portable_pty::ExitStatus;

/// How many chunks the output channel buffers before the reader thread blocks, bounding buffered
/// memory to roughly this times [`READ_BUF_SIZE`].
///
/// **Also the dominant control on throughput**, which the memory framing hides. A consumer
/// draining on an interval can only take what the channel buffered in between, so against a pty
/// firehose drained every 8ms: capacity 16 gives 4.3 MB/s, capacity 256 gives 36.2 MB/s. Do not
/// lower it without measuring under a *slow* consumer - both reach ~42 MB/s when drained in a
/// tight loop, so a fast-consumer benchmark cannot see this at all.
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
    child: Option<Box<dyn Child + Send + Sync>>,
    exited: Option<ExitStatus>,
    output_rx: Receiver<Vec<u8>>,
    reader_thread: Option<JoinHandle<()>>,
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

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| PtyError::Reader(err.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| PtyError::Writer(err.to_string()))?;

    let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_CHANNEL_CAPACITY);

    // Unix polls a self-pipe alongside the pty fd for deterministic shutdown; Windows has no safe
    // equivalent, so its reader blocks in `read` until the pty closes.
    #[cfg(unix)]
    let (reader_thread, shutdown_write) = {
        let master_fd = pair.master.as_raw_fd().ok_or_else(|| {
            PtyError::Open("pty master exposed no raw file descriptor".to_string())
        })?;
        let filedescriptor::Pipe {
            read: shutdown_read,
            write: shutdown_write,
        } = filedescriptor::Pipe::new().map_err(|err| PtyError::ShutdownPipe(err.to_string()))?;
        let reader_thread = std::thread::spawn(move || {
            run_reader_loop(reader, master_fd, shutdown_read, output_tx);
        });
        (reader_thread, Some(shutdown_write))
    };
    #[cfg(windows)]
    let (reader_thread, shutdown_write) = {
        let reader_thread = std::thread::spawn(move || {
            run_reader_loop(reader, output_tx);
        });
        (reader_thread, None)
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
        child: Some(child),
        exited: None,
        output_rx,
        reader_thread: Some(reader_thread),
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

/// The reader thread: polls `[master_fd, shutdown_read]` with no timeout, forwards a chunk when
/// the master is readable, and exits the moment the shutdown pipe is - independent of echo state
/// or whether `master`/`writer` were dropped elsewhere.
#[cfg(unix)]
fn run_reader_loop(
    mut reader: Box<dyn Read + Send>,
    master_fd: RawFd,
    shutdown_read: filedescriptor::FileDescriptor,
    output_tx: mpsc::SyncSender<Vec<u8>>,
) {
    let shutdown_fd = shutdown_read.as_raw_fd();
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
        ];

        if filedescriptor::poll(&mut pfds, None).is_err() {
            break;
        }
        if pfds[1].revents != 0 {
            break;
        }
        if pfds[0].revents == 0 {
            continue;
        }

        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if output_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
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
#[cfg(windows)]
fn run_reader_loop(mut reader: Box<dyn Read + Send>, output_tx: mpsc::SyncSender<Vec<u8>>) {
    let mut buf = [0u8; READ_BUF_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if output_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
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
    /// Raw output chunks in read order, neither line-buffered nor UTF-8-validated. Bounded, so an
    /// undrained receiver backpressures the pty instead of growing memory.
    pub fn output(&self) -> &Receiver<Vec<u8>> {
        &self.output_rx
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

    /// The child's OS pid, if the platform exposes one and the session is not shut down.
    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(|child| child.process_id())
    }

    /// Polls the child's exit status without blocking; `Ok(None)` while it is still running.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, PtyError> {
        let child = self.child.as_mut().ok_or(PtyError::AlreadyShutDown)?;
        let status = child.try_wait().map_err(PtyError::Wait)?;
        if let Some(status) = &status {
            self.exited = Some(status.clone());
        }
        Ok(status)
    }

    /// Signals the child's process group and any escaped descendants with `SIGHUP` then `SIGKILL`,
    /// no grace between, and reaps the direct child if it has already exited.
    ///
    /// Non-blocking: does not wait for termination or join the threads. Use
    /// [`shutdown`](PtySession::shutdown) for that.
    #[cfg(unix)]
    pub fn kill(&mut self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        if let Some(pid) = self.process_id() {
            terminate_process_tree(pid, Duration::ZERO);
        }
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.exited = Some(status);
            }
        }
        Ok(())
    }

    /// Terminates the child's whole process tree via [`windows_terminate_process_tree`], then
    /// the direct child itself as the backstop.
    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        if let Some(pid) = self.process_id() {
            windows_terminate_process_tree(pid);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            if let Ok(Some(status)) = child.try_wait() {
                self.exited = Some(status);
            }
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
            if let Some(child) = self.child.as_mut() {
                let status = child.wait().map_err(PtyError::Wait)?;
                self.exited = Some(status);
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
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let status = child.wait().map_err(PtyError::Wait)?;
                self.exited = Some(status);
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
/// backstopping a Jerry that dies without running `Drop` lives in `crates/app`'s `job_object`
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
                if let Some(child) = self.child.as_mut() {
                    let _ = child.kill();
                }
            }

            let reaped_immediately = self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());

            match reaped_immediately {
                Some(status) => self.exited = Some(status),
                None => {
                    // `try_wait` may have run a moment before the just-SIGKILLed child died, so a
                    // detached thread finishes the `wait()` - reaped, without blocking here.
                    if let Some(mut child) = self.child.take() {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                }
            }
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
    use std::sync::mpsc::RecvTimeoutError;
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
    /// emulator a real consumer has. `crates/app`'s pane answers it from `alacritty_terminal`'s
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

    /// Reads from `session.output()` until `needle` appears in the accumulated (lossy
    /// UTF-8) output or `timeout` elapses, returning whatever was collected either way.
    /// Returns as soon as the needle is found rather than always waiting out the full
    /// timeout, so tests aren't needlessly slow.
    fn drain_until_contains(session: &PtySession, needle: &str, timeout: Duration) -> Vec<u8> {
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
            match session.output().recv_timeout(remaining) {
                Ok(chunk) => collected.extend_from_slice(&chunk),
                Err(RecvTimeoutError::Timeout) => return collected,
                Err(RecvTimeoutError::Disconnected) => return collected,
            }
        }
    }

    /// Reads until a line starts with `prefix`, returning the rest of it - so a spawned shell can
    /// report a background job's pid deterministically instead of racing `/proc` to find it.
    #[cfg(unix)]
    fn read_line_after_prefix(
        session: &PtySession,
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
            match session.output().recv_timeout(remaining) {
                Ok(chunk) => collected.push_str(&String::from_utf8_lossy(&chunk)),
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn spawns_and_reads_short_process_output() {
        let session =
            spawn(echo_command("hello-pty-core")).expect("spawning an echo command should succeed");

        let output = drain_until_contains(&session, "hello-pty-core", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello-pty-core"),
            "expected pty output to contain the echoed text, got: {text:?}"
        );
    }

    #[test]
    fn long_output_arrives_complete_and_in_order_across_chunk_boundaries() {
        // Enough lines to cross `OUTPUT_CHANNEL_CAPACITY` many times over, which is the point of
        // the test. `cmd`'s `for /l` is orders of magnitude slower per line than `seq`, so Windows
        // gets a smaller count rather than a longer timeout.
        const LINES: usize = if cfg!(windows) { 20_000 } else { 200_000 };

        let session =
            spawn(counting_command(LINES)).expect("spawning a counting command should succeed");

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
            match session.output().recv_timeout(remaining) {
                Ok(chunk) => {
                    newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
                    collected.extend_from_slice(&chunk);
                    received += 1;
                    if received.is_multiple_of(OUTPUT_CHANNEL_CAPACITY) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => break,
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
        let session = spawn(
            SpawnOptions::new("sh")
                .arg("-c")
                .arg("set -m; sleep 100 & echo GRANDCHILD:$!; exec sleep 300"),
        )
        .expect("spawning the shell pipeline should succeed");

        let direct_pid = session
            .process_id()
            .expect("a spawned unix child should report a pid");

        let grandchild_pid =
            read_line_after_prefix(&session, "GRANDCHILD:", Duration::from_secs(5))
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

        let session = spawn(
            SpawnOptions::new("sh")
                .arg("-c")
                .arg("setsid sleep 100 & echo GRANDCHILD:$!; exec sleep 300"),
        )
        .expect("spawning the shell pipeline should succeed");

        let grandchild_pid =
            read_line_after_prefix(&session, "GRANDCHILD:", Duration::from_secs(5))
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
        // Deliberately don't drain `session.output()` while `yes` floods the pty as
        // fast as it can. With an unbounded channel this measurably grows RSS (an
        // earlier version of this crate measured ~3.4MB -> ~127MB in 3s against a
        // comparable undrained producer); with the bounded `sync_channel`, the reader
        // thread blocks in `send` once the channel fills, which backpressures its
        // `read`, which fills the kernel pty buffer, which blocks `yes`'s `write` - so
        // growth should stay small and bounded.
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
        let session =
            spawn(stdin_echoing_command()).expect("spawning an echoing shell should succeed");
        assert!(!session.writer_failed());
        for _ in 0..3 {
            session
                .write_input(b"still-writing\n")
                .expect("a healthy writer must keep accepting writes");
        }
        assert!(!session.writer_failed());
        let output = drain_until_contains(&session, "still-writing", Duration::from_secs(5));
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
        let session =
            spawn(stdin_echoing_command()).expect("spawning an echoing shell should succeed");

        session
            .write_input(b"ping-pty-core\n")
            .expect("writing input to a live pty should succeed");

        let output = drain_until_contains(&session, "ping-pty-core", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("ping-pty-core"),
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
        let session = spawn(options.cwd(requested.clone()))
            .expect("spawning a shell in a real directory should succeed");
        session
            .write_input(pwd_line.as_bytes())
            .expect("writing to a freshly spawned shell should succeed");

        let output = drain_until_contains(&session, &leaf, Duration::from_secs(15));
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
        let bogus = PathBuf::from("/definitely/not/a/real/directory/pty-core-test");
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
            resolve_on_path("definitely-not-a-real-binary-xyz-pty-core-test"),
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
            "definitely-not-a-real-binary-xyz-pty-core-test",
            &[],
            &std::env::temp_dir(),
            Duration::from_secs(30),
        );
        assert!(matches!(result, Err(PtyError::Spawn(_))), "got {result:?}");
    }
}
