//! `pty-core`: a clean spawn / stream / resize / kill primitive around a native
//! pseudo-terminal (PTY), built on the `portable-pty` crate (pinned to `0.9.0`,
//! matching `vendor/zed/Cargo.toml`).
//!
//! ## Scope decision: no `alacritty_terminal` in this crate
//!
//! `vendor/zed/crates/terminal/` does not use `portable_pty` at all for spawning: it drives
//! `alacritty_terminal::tty::Pty` directly and lets `alacritty_terminal::event_loop::EventLoop`
//! own the reader thread that both pumps bytes and feeds them into the `Term` grid parser in
//! one step - a composition that's `alacritty_terminal`-owned end to end and isn't separable
//! into a standalone "spawn primitive".
//!
//! So the split here is: `pty-core` owns spawning via `portable-pty`, a raw-byte output
//! stream, resize, and kill, with no knowledge of ANSI escapes or terminal grid state; `app`
//! (a later step) owns `alacritty_terminal`'s `Term` grid, driven by the raw bytes this crate
//! streams out.
//!
//! ## Platform scope: unix only, for now
//!
//! The reader-thread shutdown signaling (a self-pipe polled alongside the pty fd) and the
//! process-tree kill (process-group signals plus a `/proc` descendant walk) are
//! unix-specific. `portable-pty` itself is cross-platform, but equivalent non-blocking-
//! shutdown and tree-kill primitives on Windows (job objects, IOCP) are a distinct API
//! surface this crate has no reference implementation for - an explicit, documented scope
//! cut rather than a guess. This repo's target (a Linux/WSL2 desktop tool) is unix, so it
//! isn't a blocking gap for now.
//!
//! ## Output streaming
//!
//! Output is streamed via a background thread that `poll(2)`s the pty master's fd together
//! with a self-pipe used only for shutdown signaling, and forwards raw byte chunks over a
//! **bounded** `mpsc::sync_channel<Vec<u8>>` (see [`PtySession::output`]):
//!
//! - **The channel is bounded, not unbounded.** An unbounded channel against a fast,
//!   undrained producer (e.g. a stalled render loop, or an unfocused tab) is an unbounded
//!   memory leak - measured at ~40MB/s of RSS growth against a `yes` pipe with an earlier,
//!   unbounded version of this crate (see
//!   `output_channel_backpressures_instead_of_growing_unboundedly` in the test module). With
//!   a bounded `sync_channel`, a full channel blocks the reader thread's `send`, which stops
//!   it from calling `read` again, which lets the kernel's pty input buffer fill, which
//!   blocks the child's `write` - standard terminal backpressure.
//! - **Shutdown is a self-pipe, not "drop the master fd and hope the reader wakes up."**
//!   `MasterPty::try_clone_reader()` returns an independently `dup`'d fd, so the reader
//!   thread's blocking read does **not** unblock just because `PtySession`'s `master` handle
//!   gets dropped - that closes only one of the (at least) two open references to the pty.
//!   An earlier version relied on that closing coincidentally via a side effect: the
//!   `take_writer()` handle's `Drop` impl in `portable-pty` writes a trailing `\n` + EOT,
//!   and with local echo on, that write gets echoed back to the reader, incidentally waking
//!   its blocking read. With echo off (`stty -echo`, what most non-shell interactive
//!   programs do), that trick doesn't fire and the reader thread leaks for the process's
//!   life. The fix is a dedicated self-pipe (`filedescriptor::Pipe`): the reader thread
//!   `poll()`s `[pty_master_fd, shutdown_pipe_read_fd]` and exits deterministically the
//!   moment a byte is written to the pipe, independent of echo state or fd-closing races.
//!
//! ## Kill: `Drop` vs. `shutdown()`
//!
//! `Drop` must never block the calling thread for long - this crate exists to back a GPUI
//! terminal widget, and a multi-hundred-millisecond (or, in an earlier version, ~2s) freeze
//! on drop would freeze the UI thread. So `Drop` only sends signals (fast, non-blocking
//! syscalls) and does a single non-blocking `try_wait`; if the child hasn't already been
//! reaped by that point, it hands the child handle off to a short-lived **detached**
//! background thread that finishes `wait()`-ing on it, so `Drop` itself never blocks but the
//! child still doesn't linger as a zombie.
//!
//! [`PtySession::shutdown`] is the deterministic counterpart: it blocks until the process
//! (and everything in its tree) is confirmed dead and reaped, and the reader/writer threads
//! have been joined. Call it from a background task when teardown must fully complete before
//! proceeding (e.g. before removing a tab's last reference); rely on `Drop` alone when you
//! just want "don't leak" from a thread that can't block.
//!
//! ## Kill: process group *and* process tree
//!
//! The child is made a session/process-group leader by `portable-pty` itself (it calls
//! `setsid()` in `pre_exec` on unix), so signaling its pgid with `killpg` reaches any
//! ordinary descendant that stayed in that group (e.g. `some_cmd &` inside a non-interactive
//! shell). That's not sufficient alone: a descendant that calls `setsid()` itself (e.g.
//! `setsid sleep 100 &`, or any daemonizing tool) detaches into its own session and process
//! group, unreachable via the parent's pgid. To handle that, [`kill`](PtySession::kill) /
//! [`shutdown`](PtySession::shutdown) / `Drop` all first walk `/proc/<pid>/task/<pid>/children`
//! (Linux procfs; breadth-first, depth-capped) to snapshot the *entire* descendant set
//! **before** sending any signal - reading it after the fact races against the kernel
//! reparenting a dying process's children out from under that file - then signal both the
//! process group and every discovered descendant individually. See
//! `drop_terminates_entire_process_tree_including_escaped_grandchild` in the test module for
//! the regression case this fixes.
//!
//! ## Input writing
//!
//! [`PtySession::write_input`] hands bytes to a background writer thread over an
//! `mpsc::Sender` rather than writing directly (behind a lock) on the caller's thread - a
//! full pty write buffer (child not reading its input) would otherwise block whichever
//! thread called `write_input`, plausibly a GPUI key-handler on the main thread, and
//! serialize every other writer behind the same lock. Enqueueing onto a channel is
//! effectively non-blocking for realistic input volumes; the actual (possibly blocking)
//! `write` syscall only ever happens on the dedicated writer thread.
//!
//! ## Working directory
//!
//! `portable-pty`'s `CommandBuilder` silently falls back to the user's home directory both
//! when no cwd is set *and* when a set cwd fails an `is_dir()` check - a caller passing a
//! stale path (e.g. a deleted worktree) would silently get a process spawned in `$HOME`
//! instead of an error. [`spawn`] avoids both silent fallbacks: it defaults to
//! `std::env::current_dir()` when [`SpawnOptions::cwd`] is unset, and validates any
//! caller-supplied cwd is an existing directory before handing it to `CommandBuilder`,
//! returning [`PtyError::InvalidCwd`] otherwise.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use thiserror::Error;

#[cfg(not(unix))]
compile_error!(
    "pty-core's reader-shutdown and process-tree-kill implementation is unix-only \
     right now (this repo targets Linux/WSL2); see the crate-level docs for why."
);

pub use portable_pty::ExitStatus;

/// How many output chunks (each up to [`READ_BUF_SIZE`] bytes) the pty->caller channel
/// buffers before the reader thread blocks. Bounds worst-case buffered memory to
/// roughly `OUTPUT_CHANNEL_CAPACITY * READ_BUF_SIZE` (~1MB) regardless of how fast the
/// child produces output or how slowly the caller drains it.
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
/// Size of each read from the pty master per loop iteration of the reader thread.
const READ_BUF_SIZE: usize = 4096;
/// How long [`PtySession::shutdown`] waits for a graceful exit (after `SIGHUP`) before
/// unconditionally following up with `SIGKILL`. `Drop`'s fast path uses zero grace.
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(200);
/// Poll interval while waiting out [`SHUTDOWN_GRACE_PERIOD`].
const SHUTDOWN_GRACE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Depth cap for the `/proc` descendant-tree walk, purely defensive against
/// pathological process trees; realistic terminal workloads are nowhere near this deep.
const TREE_WALK_MAX_DEPTH: usize = 8;

/// Errors that can occur while spawning, driving, or tearing down a [`PtySession`].
///
/// Deliberately carries no `anyhow::Error`: `anyhow::Error` does not implement
/// `std::error::Error` (`anyhow-1.0.104/src/error.rs` impls `Display`/`Debug`/
/// `Deref<Target = dyn StdError>` but not `StdError` itself), so a `#[source] anyhow::Error`
/// field wouldn't compile with thiserror, and would leak an opaque dependency type into this
/// crate's public API regardless. Failures from `portable-pty`'s own `anyhow::Result`
/// returns are captured via their `Display` output as an owned `String` instead.
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
    /// Creates spawn options for `program` with a default 80x24 size and no args/env
    /// overrides. `cwd` defaults to the caller's current directory at spawn time (see
    /// the crate-level docs on working directory handling) unless [`Self::cwd`] is used.
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

/// Resolves `program` (a bare command name, e.g. `"claude"`) against `$PATH` via real
/// filesystem checks.
///
/// This exists so a caller that needs to know *whether a spawn would succeed* (e.g. the app
/// crate's Settings › Agents page, showing a "ready"/"not found" dot per agent binary) can
/// ask without actually spawning anything. It mirrors `portable-pty-0.9.0`'s own
/// `CommandBuilder::search_path` (unix implementation, private, so not callable directly;
/// `portable-pty-0.9.0/src/cmdbuilder.rs:416-472`), which [`spawn`] itself relies on via
/// `CommandBuilder::spawn` for a bare (non-cwd-relative) program name: for each directory in
/// `std::env::split_paths(&PATH)`, join it with `program` and check the candidate exists and
/// is executable, returning the first hit.
///
/// Two deliberate departures from that algorithm:
/// - `portable-pty` uses `nix::unistd::access(path, X_OK)`, a real `access(2)` syscall this
///   crate's callers have no `nix` dependency of their own to call. [`is_executable`]
///   instead checks the owner/group/other execute bits on
///   `std::fs::Metadata::permissions().mode()` (`S_IXUSR | S_IXGRP | S_IXOTH`) - real file
///   metadata, though unlike `access(2)` it doesn't account for ACLs or a process's specific
///   uid/gid. Judged an acceptable simplification for a status dot over adding a dependency.
/// - This never checks a cwd-relative candidate (`portable-pty`'s `is_cwd_relative_path`
///   branch, e.g. `./claude`) - every caller here passes a bare name
///   (`SessionKind::agent_binary_name` in the `app` crate only returns plain names like
///   `"claude"`/`"codex"`), so that branch is dead code for this crate's actual callers.
///
/// Because this is a second, independent implementation of the same algorithm rather than a
/// call into `portable-pty`'s own (private) function, it isn't literally guaranteed to agree
/// with [`spawn`] in every exotic case (e.g. a `PATH` entry that's itself a symlink, or
/// unusual permission-bit combinations) - but both walk the same `$PATH` in the same order
/// with the same "exists and is executable" test, so they agree in every realistic case.
pub fn resolve_on_path(program: &str) -> Option<PathBuf> {
    resolve_in_path_var(&std::env::var_os("PATH")?, program)
}

/// The search loop [`resolve_on_path`] runs, factored out so it can take a `PATH` value as a
/// plain argument instead of always reading the process environment. This lets
/// `resolve_on_path_skips_a_same_named_directory` (and any other test that needs a custom,
/// isolated `PATH`) construct an `OsString` directly and pass it here, rather than mutating
/// `std::env`'s process-global `PATH` var - which would need `unsafe` (`std::env::set_var`
/// requires it as of this workspace's edition) and would be racy against any other test
/// concurrently reading the environment (e.g. via `portable_pty`'s `get_base_env`, exercised
/// by sibling tests through real `spawn` calls), since `cargo test` runs tests concurrently.
fn resolve_in_path_var(path_var: &OsStr, program: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(program);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Real executable-bit check via `std::fs::Metadata` - see [`resolve_on_path`]'s docs for
/// why this is used instead of a real `access(2)` call.
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    const EXEC_BITS: u32 = 0o111; // owner, group, other execute bits
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & EXEC_BITS != 0)
        .unwrap_or(false)
}

/// A running process attached to a PTY.
///
/// See the crate-level docs for the design of output streaming, input writing, and the
/// `Drop` vs. [`shutdown`](PtySession::shutdown) split.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    exited: Option<ExitStatus>,
    output_rx: Receiver<Vec<u8>>,
    reader_thread: Option<JoinHandle<()>>,
    writer_tx: Option<mpsc::Sender<Vec<u8>>>,
    writer_thread: Option<JoinHandle<()>>,
    shutdown_write: Option<filedescriptor::FileDescriptor>,
}

/// Spawns `options.program` on a freshly opened native PTY.
///
/// API surface: `native_pty_system()`, `PtySystem::openpty`, `PtyPair { slave, master }`,
/// `SlavePty::spawn_command`, `MasterPty::{try_clone_reader, take_writer, resize, get_size,
/// as_raw_fd}`, `Child`/`ChildKiller::{kill, wait, try_wait, process_id}` from
/// `portable-pty-0.9.0`; `filedescriptor` (`Pipe`, `FileDescriptor`, `poll`, `pollfd`,
/// `POLLIN`) and `nix::sys::signal::{kill, killpg}` / `nix::unistd::Pid` from
/// `filedescriptor-0.8.3`/`nix-0.28.0`.
///
/// The parent's copy of `pair.slave` is dropped immediately after spawning: on unix the
/// spawned child inherits its own duplicate of the slave-side file descriptors, and the
/// master's reader only observes EOF once *every* open reference to the slave side is
/// closed, so keeping `pair.slave` alive here would prevent EOF from ever being observed
/// after the child exits.
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
    // See the doc comment above: this drop is load-bearing for EOF delivery, not cleanup.
    drop(pair.slave);

    let master_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| PtyError::Open("pty master exposed no raw file descriptor".to_string()))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| PtyError::Reader(err.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| PtyError::Writer(err.to_string()))?;

    let filedescriptor::Pipe {
        read: shutdown_read,
        write: shutdown_write,
    } = filedescriptor::Pipe::new().map_err(|err| PtyError::ShutdownPipe(err.to_string()))?;

    let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_CHANNEL_CAPACITY);
    let reader_thread = std::thread::spawn(move || {
        run_reader_loop(reader, master_fd, shutdown_read, output_tx);
    });

    let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>();
    let writer_thread = std::thread::spawn(move || {
        run_writer_loop(writer, writer_rx);
    });

    Ok(PtySession {
        master: pair.master,
        child: Some(child),
        exited: None,
        output_rx,
        reader_thread: Some(reader_thread),
        writer_tx: Some(writer_tx),
        writer_thread: Some(writer_thread),
        shutdown_write: Some(shutdown_write),
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

/// Body of the background reader thread: blocks in `poll(2)` on `[master_fd,
/// shutdown_read]` (no timeout - the self-pipe is what wakes it), reads and forwards a
/// chunk when the master is readable, and exits the moment the shutdown pipe becomes
/// readable, regardless of echo state or whether `master`/`writer` have been dropped
/// elsewhere. See the crate-level docs for why this replaced an fd-closure-based design.
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

/// Body of the background writer thread: serializes writes from [`PtySession::write_input`]
/// so the pty's actual (possibly blocking) `write` syscall never happens on a caller's
/// thread. Exits once its `Sender` is dropped (channel disconnected) or a write fails.
fn run_writer_loop(mut writer: Box<dyn Write + Send>, rx: mpsc::Receiver<Vec<u8>>) {
    while let Ok(data) = rx.recv() {
        if writer.write_all(&data).is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
}

impl PtySession {
    /// The channel of raw output byte chunks read from the pty, in read order. Chunks
    /// are not line-buffered or UTF-8-validated; that's left to a higher layer (e.g. an
    /// ANSI/grid parser in the `app` crate). The channel is bounded (see
    /// [`OUTPUT_CHANNEL_CAPACITY`]): an undrained receiver backpressures the pty rather
    /// than growing memory unboundedly.
    pub fn output(&self) -> &Receiver<Vec<u8>> {
        &self.output_rx
    }

    /// Enqueues `data` to be written to the pty's input side (i.e. as if typed by a
    /// user or piped in by another process). The actual write happens on a dedicated
    /// background thread, so this call does not block on pty I/O even if the child
    /// isn't currently reading its input.
    pub fn write_input(&self, data: &[u8]) -> Result<(), PtyError> {
        self.writer_tx
            .as_ref()
            .ok_or(PtyError::WriterClosed)?
            .send(data.to_vec())
            .map_err(|_| PtyError::WriterClosed)
    }

    /// Resizes the pty. Propagates the resize down to the kernel, which will notify the
    /// child (e.g. via `SIGWINCH` on unix).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| PtyError::Resize(err.to_string()))
    }

    /// The child process's OS pid, if the platform exposes one and the session hasn't
    /// been [`shutdown`](PtySession::shutdown) already.
    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(|child| child.process_id())
    }

    /// Polls the child's exit status without blocking. Returns `Ok(None)` if it's still
    /// running.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, PtyError> {
        let child = self.child.as_mut().ok_or(PtyError::AlreadyShutDown)?;
        let status = child.try_wait().map_err(PtyError::Wait)?;
        if let Some(status) = &status {
            self.exited = Some(status.clone());
        }
        Ok(status)
    }

    /// Immediately (non-blocking) signals the child's process group and any escaped
    /// descendants (see the crate-level docs on process-tree kill) with `SIGHUP` then
    /// `SIGKILL`, with no grace period between them, and opportunistically reaps the
    /// direct child if it has already exited. Does not wait for termination to
    /// complete or join the reader/writer threads; use [`shutdown`](PtySession::shutdown)
    /// when you need that guarantee.
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

    /// Deterministically tears the session down: signals the child's process tree
    /// (`SIGHUP`, a bounded grace period, then `SIGKILL`), blocks until the direct
    /// child is reaped, signals the reader thread to stop via the shutdown pipe and
    /// joins it, then closes the writer channel and joins the writer thread. Meant to
    /// be called from a background task (it can block for up to
    /// [`SHUTDOWN_GRACE_PERIOD`] plus however long the child takes to actually exit
    /// after `SIGKILL`, which in practice is fast but is not itself bounded by this
    /// function). Safe to call more than once.
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
            // Best-effort: if this fails the reader thread simply won't notice the
            // shutdown signal and will keep running until the pty otherwise closes;
            // it is not load-bearing for the child having already been killed above.
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
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exited.is_none() {
            if let Some(pid) = self.process_id() {
                // Zero grace: Drop must not block the caller (see crate-level docs).
                terminate_process_tree(pid, Duration::ZERO);
            }

            let reaped_immediately = self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());

            match reaped_immediately {
                Some(status) => self.exited = Some(status),
                None => {
                    // SIGKILL was just sent; the child will die essentially immediately,
                    // but `try_wait` may have run a moment too early to observe it. Hand
                    // the handle to a short-lived detached thread that finishes `wait()`
                    // so it gets reaped instead of lingering as a zombie, without making
                    // *this* call block.
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
        // Reader/writer `JoinHandle`s are intentionally not joined here - the process
        // is dead or dying and the shutdown byte has been written, so both threads will
        // notice and exit on their own shortly; joining would block the caller. Their
        // `JoinHandle`s simply drop as the rest of `self`'s fields go out of scope
        // below, which detaches (does not block on, and does not kill) the OS threads.
    }
}

/// Reads the current direct children of `pid` from Linux's `/proc/<pid>/task/<pid>/children`.
/// Best-effort: returns an empty list if the file can't be read (process already gone,
/// non-Linux unix, permissions, etc.) rather than erroring - this is used for cleanup,
/// where "found nothing to additionally clean up" is an acceptable fallback.
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

/// Breadth-first, depth-capped walk of `root_pid`'s descendant tree via `/proc`. Must be
/// called *before* signaling anything: reading it after a process starts dying races
/// against the kernel reparenting its children out from under `children`.
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

fn pid_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// Terminates `root_pid`'s process group *and* any descendants that escaped it (e.g. via
/// their own `setsid()`) - see the crate-level docs. Sends `SIGHUP` to everything found,
/// optionally waits up to `grace` (polling, not a fixed sleep) for voluntary exit, then
/// unconditionally follows up with `SIGKILL`. Pure signal-sends plus, at most, a bounded
/// poll loop: never blocks on `waitpid`. Errors from individual `kill`/`killpg` calls
/// (e.g. `ESRCH` because something already exited) are intentionally ignored - this
/// function's job is "make a best effort to ensure nothing survives," not to report
/// which of potentially many targets could or couldn't be signaled.
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
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

    /// Reads from `session.output()` until `needle` appears in the accumulated (lossy
    /// UTF-8) output or `timeout` elapses, returning whatever was collected either way.
    /// Returns as soon as the needle is found rather than always waiting out the full
    /// timeout, so tests aren't needlessly slow.
    fn drain_until_contains(session: &PtySession, needle: &str, timeout: Duration) -> Vec<u8> {
        let mut collected = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
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

    /// Reads from `session.output()` until a line starting with `prefix` appears,
    /// returning the rest of that line (trimmed of the newline). Used to have a spawned
    /// shell report a background job's pid back to us deterministically, instead of
    /// racing on `/proc` to discover it.
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
        let session = spawn(SpawnOptions::new("echo").arg("hello-pty-core"))
            .expect("spawning `echo hello-pty-core` should succeed");

        let output = drain_until_contains(&session, "hello-pty-core", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello-pty-core"),
            "expected pty output to contain the echoed text, got: {text:?}"
        );
    }

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

        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_exists(pid) {
            assert!(
                Instant::now() < deadline,
                "child pid {pid} was still alive {:?} after PtySession was dropped - orphaned process",
                deadline.elapsed()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn drop_terminates_entire_process_tree_including_escaped_grandchild() {
        // Regression test for a process that escapes the child's process group by
        // calling `setsid()` itself (e.g. a daemonizing subprocess): `killpg` on the
        // direct child's pgid alone cannot reach it. The shell reports the detached
        // grandchild's pid back to us over the pty so we don't have to race /proc to
        // discover it ourselves.
        let session = spawn(
            SpawnOptions::new("sh")
                .arg("-c")
                .arg("setsid sleep 100 & echo GRANDCHILD:$!; exec sleep 300"),
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

        drop(session);

        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_exists(direct_pid) || pid_exists(grandchild_pid) {
            assert!(
                Instant::now() < deadline,
                "direct child ({direct_pid}) or its escaped grandchild ({grandchild_pid}) was \
                 still alive after PtySession was dropped - orphaned process"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

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

    #[test]
    fn shutdown_joins_reader_thread_even_with_local_echo_disabled() {
        // Regression test for the fd-leak bug: an earlier version of this crate assumed
        // dropping the pty master would unblock the reader thread's blocking read, but
        // the reader holds an independently-dup'd fd (`try_clone_reader`) that survives
        // that just fine. It only appeared to work because the writer's own EOT-on-drop
        // trick got echoed back to the reader when local echo was on. With echo
        // disabled (as most real interactive programs run their ptys), the old reader
        // thread - and its fd - would leak for the life of the process. `shutdown()`
        // should still return promptly via the self-pipe/poll based shutdown signal,
        // regardless of echo state.
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

        // Bound to `_session` (not used by name) but kept alive for the duration of the
        // measurement window below - its `Drop` at the end of the test is what tears
        // `yes` down.
        let _session = spawn(SpawnOptions::new("yes")).expect("spawning `yes` should succeed");

        let rss_before = read_self_rss_kb();
        // Deliberately don't drain `session.output()` while `yes` floods the pty as
        // fast as it can. With an unbounded channel this measurably grows RSS (an
        // earlier version of this crate measured ~3.4MB -> ~127MB in 3s against a
        // comparable undrained producer); with the bounded `sync_channel`, the reader
        // thread blocks in `send` once the channel fills, which backpressures its
        // `read`, which fills the kernel pty buffer, which blocks `yes`'s `write` - so
        // growth should stay small and bounded.
        std::thread::sleep(Duration::from_millis(500));
        let rss_after = read_self_rss_kb();

        let growth_kb = rss_after.saturating_sub(rss_before);
        assert!(
            growth_kb < 20_000,
            "RSS grew by {growth_kb} kB while an undrained `yes` pipe ran for 500ms - \
             the output channel does not appear to be backpressuring (expected growth \
             bounded by the channel capacity, well under 20MB)"
        );
    }

    #[test]
    fn write_input_is_echoed_back_by_the_pty_line_discipline() {
        // A pty in cooked mode echoes what's written to it back out through the reader,
        // independent of whatever the child program does. `cat` here just keeps the pty
        // session alive long enough to observe the echo.
        let session = spawn(SpawnOptions::new("cat")).expect("spawning `cat` should succeed");

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
            spawn(SpawnOptions::new("sleep").arg("2")).expect("spawning `sleep 2` should succeed");

        session
            .resize(40, 120)
            .expect("resizing a live pty session should not error");

        let size_after = session
            .master
            .get_size()
            .expect("get_size should succeed after a resize");
        assert_eq!(size_after.rows, 40);
        assert_eq!(size_after.cols, 120);
    }

    #[test]
    fn spawn_reports_typed_error_for_nonexistent_program() {
        // `PtySession` intentionally doesn't implement `Debug` (its fields are trait
        // objects), so this is written as a match rather than `.expect_err(..)`, which
        // requires the `Ok` type to be `Debug`.
        match spawn(SpawnOptions::new("definitely-not-a-real-binary-xyz")) {
            Err(err) => assert!(matches!(err, PtyError::Spawn(_))),
            Ok(_) => panic!("spawning a nonexistent program should have failed"),
        }
    }

    #[test]
    fn spawn_rejects_nonexistent_cwd_instead_of_silently_falling_back_to_home() {
        let bogus = PathBuf::from("/definitely/not/a/real/directory/pty-core-test");
        match spawn(SpawnOptions::new("true").cwd(bogus.clone())) {
            Err(PtyError::InvalidCwd(path)) => assert_eq!(path, bogus),
            Err(other) => panic!("expected PtyError::InvalidCwd, got a different error: {other}"),
            Ok(_) => panic!("expected spawn to reject a nonexistent cwd, but it succeeded"),
        }
    }

    /// `sh` is as close to a guaranteed-present binary as a unix test environment gets
    /// (this crate is unix-only already - see the crate-level `compile_error!`, and every
    /// other test here already spawns real coreutils/shell binaries like `echo`/`sleep`/
    /// `sh` unconditionally). Real end-to-end proof that [`resolve_on_path`] finds a real
    /// binary and returns a path that is itself real (exists, is a file, is executable) -
    /// not just "found something".
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

    /// The exact real, non-panicking "not found" case the app's Settings › Agents page
    /// depends on for a genuinely-absent binary (e.g. `codex` on a dev machine that never
    /// installed it) to show a real "not found" status rather than silently panicking or
    /// fabricating a `ready` state.
    #[test]
    fn resolve_on_path_returns_none_for_a_binary_that_does_not_exist() {
        assert_eq!(
            resolve_on_path("definitely-not-a-real-binary-xyz-pty-core-test"),
            None
        );
    }

    /// A directory that happens to share the binary's name must not be mistaken for it -
    /// this is exactly the `candidate.is_dir()` guard `portable-pty`'s own `search_path`
    /// has (see [`resolve_on_path`]'s docs); the equivalent guard here is `is_file()`,
    /// checked directly against a real temporary directory of that name to prove it isn't
    /// just an untested claim in a comment.
    ///
    /// Builds a real, isolated `PATH` value and passes it directly to
    /// [`resolve_in_path_var`] rather than mutating the real process-global `PATH` via
    /// `std::env::set_var` - that would need `unsafe` and would be genuinely racy against
    /// `cargo test`'s default concurrent test execution (see [`resolve_in_path_var`]'s
    /// docs), for zero benefit: the search loop under test never reads `std::env` itself.
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
