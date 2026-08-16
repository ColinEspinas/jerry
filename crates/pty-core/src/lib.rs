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
//! ## Platform scope: full on unix, narrower (but real) on Windows
//!
//! The reader-thread shutdown signaling (a self-pipe polled alongside the pty fd) and the
//! process-tree kill (process-group signals plus a `/proc` descendant walk) are `#[cfg(unix)]`.
//! `child_pids_of`/`pid_exists` read Linux's `/proc` specifically; on a non-Linux unix (e.g.
//! macOS) they already degrade gracefully to "found nothing" (see their own docs), so
//! `terminate_process_tree` still sends real signals to the child's process group there, just
//! without the escaped-grandchild walk.
//!
//! Windows (`#[cfg(windows)]` - deliberately not the broader `#[cfg(not(unix))]`, see below)
//! gets a real, but intentionally narrower, equivalent rather than a blanket refusal to
//! compile: [`PtySession::kill`]/`Drop` call `portable_pty::Child`'s own cross-platform
//! `kill()` (a safe wrapper this crate doesn't need `unsafe` to call), which terminates the
//! direct child only - no process-group or descendant-tree kill, since neither concept exists
//! on Windows without job objects (a distinct, `unsafe`-FFI-heavy API surface this project's
//! own no-`unsafe` rule rules out here). That means a killed child's own grandchildren (e.g. a
//! subprocess an agent CLI spawned) can survive as real orphans on Windows - a genuine,
//! tracked regression relative to the unix path, not fixed here (see [`PtySession::kill`]'s
//! `#[cfg(windows)]` docs).
//!
//! The reader thread also can't be proactively interrupted the way `#[cfg(unix)]`'s self-pipe
//! does: `filedescriptor` 0.8.3's Windows `poll_impl` is backed by `WSAPoll`, which only
//! accepts sockets, and a ConPTY master handle is a named pipe, not a socket - so there is no
//! safe self-pipe-style signal available. `run_reader_loop`'s `#[cfg(windows)]` variant just
//! blocks in `read` until the pty naturally closes (EOF/error) - see that function's own docs
//! for exactly when that happens: only once `PtySession::master` itself is dropped, NOT merely
//! once the child is killed/reaped. That has two real, load-bearing consequences fixed in this
//! revision:
//!
//! - **Process-exit detection can't rely on the output channel disconnecting.** On unix, a
//!   dead child's reader thread hits real EOF quickly and its dropped `output_tx` is what the
//!   rest of the app (`crate::terminal_pane` in the `app` crate) reacts to. On Windows that
//!   channel disconnect may never happen - a killed/reaped process leaves `master` untouched,
//!   so the reader thread stays blocked in `read` indefinitely. Callers on Windows must instead
//!   poll [`PtySession::try_wait`] directly and independently of channel state (real,
//!   non-blocking `GetExitCodeProcess` under the hood - see `portable-pty-0.9.0/src/win/mod.rs`),
//!   the way `crate::terminal_pane`'s `#[cfg(windows)]` poll-loop branch in the `app` crate now
//!   does.
//! - **[`PtySession::shutdown`]'s Windows twin now explicitly drops `master` itself**, before
//!   joining the reader thread, rather than handing that join to a detached thread and hoping a
//!   later `Drop` closes `master` eventually - see that function's own docs.
//!
//! None of the `#[cfg(windows)]` code paths below have run on a real Windows machine - this
//! crate has only ever executed on Linux/WSL2. They're reasoned from `portable-pty` 0.9.0's and
//! `filedescriptor` 0.8.3's own real source and checked with
//! `cargo check --target x86_64-pc-windows-gnu`, not verified by running.
//!
//! `#[cfg(windows)]`, not `#[cfg(not(unix))]`, is deliberate throughout this file: every one of
//! these code paths is reasoned specifically about ConPTY/`%PATHEXT%`/Windows semantics, and
//! would be silently wrong (not just untested) if it also compiled in on some other, genuinely
//! different non-unix target this project doesn't support (e.g. `wasm32-unknown-unknown`,
//! already installable in this sandbox - `std::env::split_paths` alone has different real
//! semantics there). `cfg(windows)` makes such a target fail to compile loudly instead of
//! silently picking up Windows-specific logic that makes no sense for it.
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

// `.expect()`/`.unwrap()` are the documented, accepted pattern in test modules (see
// `CLAUDE.md`'s Rust standards) - only production code is held to `clippy::unwrap_used`/
// `expect_used`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
#[cfg(unix)]
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
// Only `#[cfg(unix)]` code below (the process-tree kill path, and the two grace-period
// consts) uses `Duration` outside of `#[cfg(test)]`; the test module below imports it
// again itself so it's in scope there regardless of platform.
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
use thiserror::Error;

pub use portable_pty::ExitStatus;

/// How many output chunks (each up to [`READ_BUF_SIZE`] bytes) the pty->caller channel
/// buffers before the reader thread blocks. Bounds worst-case buffered memory to
/// roughly `OUTPUT_CHANNEL_CAPACITY * READ_BUF_SIZE` (~1MB) regardless of how fast the
/// child produces output or how slowly the caller drains it.
///
/// This is **also the dominant control on delivered throughput**, which is not obvious from
/// the memory-bound framing above and is worth stating explicitly, because it makes lowering
/// this constant look free when it very much isn't. The consumer drains on an interval
/// (`app`'s `TerminalPane::POLL_INTERVAL`, 8ms), so the most that can be delivered per drain is
/// whatever the channel managed to buffer in between - i.e. capacity x actual chunk size. A
/// measured sweep against a real pty firehose with a drain every 8ms:
///
/// ```text
/// capacity  16 ->  4.3 MB/s
/// capacity 256 -> 36.2 MB/s
/// ```
///
/// An earlier revision of this change cut this to 16 to "preserve the ~1MB bound" alongside a
/// 64KiB [`READ_BUF_SIZE`]; that was an 8.5x throughput regression bought for nothing, since
/// the larger read buffer turned out to be worth ~2%. Do not lower this without measuring
/// delivered MB/s under a *slow* consumer - a fast-consumer benchmark cannot see this at all
/// (both capacities reach ~42 MB/s when drained in a tight loop).
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
/// Size of each read from the pty master per loop iteration of the reader thread.
///
/// Deliberately left at 4KiB after measuring the alternative. `alacritty_terminal`'s own reader
/// uses a 1MiB buffer (`READ_BUFFER_SIZE = 0x10_0000` in its `src/event_loop.rs`, verified in
/// the `zed-industries/alacritty` revision this workspace pins), so raising this looks like an
/// obvious win - it isn't, here. Against a real pty firehose, raising it to 64KiB moved
/// delivered throughput by ~2%: 4.27 -> 4.42 MB/s with a slow (backpressured) consumer, and
/// 41.8 -> 42.0 MB/s with a fast one. The reason is that `read(2)` returns whatever is
/// available rather than waiting to fill the buffer, and on a pty the available amount is
/// governed by the line discipline, not by the caller's buffer: measured chunk sizes stayed at
/// p50 ~600-700 bytes and p95 4095 either way. Only the rare `max` grew (4KiB -> ~23KiB), which
/// is exactly the tail that a 16x larger worst-case memory bound would have to be sized for.
/// What actually governs throughput here is [`OUTPUT_CHANNEL_CAPACITY`] - see its docs.
const READ_BUF_SIZE: usize = 4096;
/// How long [`PtySession::shutdown`] waits for a graceful exit (after `SIGHUP`) before
/// unconditionally following up with `SIGKILL`. `Drop`'s fast path uses zero grace.
/// Unix-only: Windows has no `SIGHUP`-equivalent graceful signal available without
/// `unsafe` FFI, so the non-unix `kill`/`shutdown`/`Drop` paths just call
/// `portable_pty::Child::kill()` once - see the crate-level docs.
#[cfg(unix)]
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(200);
/// Poll interval while waiting out [`SHUTDOWN_GRACE_PERIOD`].
#[cfg(unix)]
const SHUTDOWN_GRACE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Depth cap for the `/proc` descendant-tree walk, purely defensive against
/// pathological process trees; realistic terminal workloads are nowhere near this deep.
#[cfg(unix)]
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
    #[error("failed to signal child process: {0}")]
    Signal(String),
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
///   (`AgentKind::binary_name` in the `app` crate only returns plain names like
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

/// Real executable-bit check via `std::fs::Metadata` - see [`resolve_on_path`]'s docs for
/// why this is used instead of a real `access(2)` call.
#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    const EXEC_BITS: u32 = 0o111; // owner, group, other execute bits
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & EXEC_BITS != 0)
        .unwrap_or(false)
}

/// Windows has no execute-bit concept - a real, checkable equivalent instead: `%PATHEXT%`
/// (default `.EXE` when unset, matching what `portable-pty` 0.9.0's own Windows
/// `CommandBuilder::search_path` - `portable-pty-0.9.0/src/cmdbuilder.rs:581-606` - falls back
/// to) lists the extensions Windows treats as directly runnable. Mirrors that real algorithm's
/// per-directory candidate set, with three small, deliberate divergences worth documenting as
/// intentional rather than drift: each `PATHEXT` extension is tried *before* the bare candidate
/// (upstream tries the bare name first) - a bare, extension-less file can never be run directly
/// by `CreateProcessW` (no shebang support), so a directory shadowing a real `.cmd`/`.bat`
/// shim with a same-named extension-less file (exactly what Node version managers like Volta
/// and nvm-windows ship: a POSIX shell script under the bare name next to the real Windows
/// `.cmd` wrapper) must not win over the extension that's actually runnable - see
/// `resolve_in_path_var_prefers_a_pathext_extension_over_a_same_named_extensionless_file` in
/// the test module; a real `is_file()` check at
/// each candidate (upstream instead uses `exists()`, which would also match a same-named
/// directory - see `resolve_on_path_skips_a_same_named_directory` in the test module for the
/// unix equivalent of this guard), and `trim_start_matches('.')` to strip a `PATHEXT` entry's
/// leading dot (upstream instead slices `&ext[1..]`, which panics if an entry were ever empty;
/// this version just skips it instead - see the `filter_map` below).
///
/// Inherited caveat, NOT fixed here: like upstream, `with_extension` below *replaces* rather
/// than appends an extension (`std::path::Path::with_extension`'s own documented behavior) - so
/// for a candidate whose bare name already contains a `.` (e.g. `python3.11`), this produces
/// `python3.EXE`, not `python3.11.EXE`. Upstream's own comment
/// (`portable-pty-0.9.0/src/cmdbuilder.rs:592-594`) calls this "potentially wrong"; mirrored
/// faithfully here rather than silently diverging from the algorithm [`spawn`] itself
/// ultimately relies on via `CommandBuilder` for real (non-`resolve_on_path`) spawns.
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
        // PATHEXT extensions checked before the bare candidate: on Windows a bare,
        // extension-less file can never be run directly by `CreateProcessW` (no shebang
        // support), so preferring it over a real `.cmd`/`.bat`/`.exe` sibling is always wrong
        // - and directories holding a Node version-manager shim (Volta, nvm-windows, ...)
        // routinely contain exactly that pair: a POSIX shell script under the bare name
        // *and* a `.cmd` wrapper beside it. Matching the bare file first used to hand that
        // shell script straight to `Command::new`, which `CreateProcessW` then rejected with
        // `%1 is not a valid Win32 application` instead of running the `.cmd` shim.
        for extension in &extensions {
            let candidate = candidate.with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A running process attached to a PTY.
///
/// See the crate-level docs for the design of output streaming, input writing, and the
/// `Drop` vs. [`shutdown`](PtySession::shutdown) split.
pub struct PtySession {
    // `Option` (not a bare `Box`) so [`PtySession::shutdown`]'s `#[cfg(windows)]` twin can
    // explicitly `.take()`/drop this *before* joining the reader thread - see that function's
    // docs for why an early, explicit drop of this specific field is load-bearing on Windows
    // (it's what actually closes the ConPTY and unblocks the reader's blocked `read`), not
    // just cleanup.
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    exited: Option<ExitStatus>,
    output_rx: Receiver<Vec<u8>>,
    reader_thread: Option<JoinHandle<()>>,
    writer_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Raised by [`run_writer_loop`] when a real write to the pty fails, so
    /// [`PtySession::write_input`] stops reporting success into a broken pipe. See its docs.
    writer_failed: Arc<AtomicBool>,
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

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| PtyError::Reader(err.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| PtyError::Writer(err.to_string()))?;

    let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_CHANNEL_CAPACITY);

    // See the crate-level "Platform scope" docs: unix gets a self-pipe the reader thread
    // polls alongside the pty fd for deterministic shutdown; non-unix has no safe
    // equivalent (`WSAPoll` is socket-only, a ConPTY master handle isn't a socket), so its
    // reader thread just blocks in `read` until the pty naturally closes.
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

/// Body of the background reader thread: blocks in `poll(2)` on `[master_fd,
/// shutdown_read]` (no timeout - the self-pipe is what wakes it), reads and forwards a
/// chunk when the master is readable, and exits the moment the shutdown pipe becomes
/// readable, regardless of echo state or whether `master`/`writer` have been dropped
/// elsewhere. See the crate-level docs for why this replaced an fd-closure-based design.
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

/// Windows-only body of the background reader thread: no self-pipe/poll shutdown signal is
/// available here (see the crate-level "Platform scope" docs), so this just blocks in `read`
/// until the pty naturally closes. That EOF/error is NOT triggered by killing the child -
/// `kill()`/`TerminateProcess` never touches `master`, so on its own it leaves this thread
/// blocked forever. It fires only once ConPTY itself is torn down: `portable-pty`'s Windows
/// backend (`portable-pty-0.9.0/src/win/conpty.rs` + `win/psuedocon.rs`) calls the real
/// `ClosePseudoConsole` inside `PsuedoCon`'s own `Drop`, which only runs once every `Arc`
/// reference to the shared `Inner` (held by `MasterPty`/`SlavePty` clones) is gone - [`spawn`]
/// already drops its `SlavePty` clone (`pair.slave`) right after spawning, so
/// `PtySession::master` ends up the *only* remaining reference. Dropping it is what makes
/// conhost release its handle to the output pipe's write side, which is what actually
/// unblocks this thread's `read`. Concretely, that happens: explicitly and promptly inside
/// [`PtySession::shutdown`]'s `#[cfg(windows)]` twin (which takes `master` itself before
/// joining this thread - see that function's docs); or incidentally on `Drop`, today only
/// because `master` is declared *before* `reader_thread` in the [`PtySession`] struct, so it
/// drops first as the struct's fields go out of scope in order - not because `Drop`'s own code
/// touches `master` directly.
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

/// Body of the background writer thread: serializes writes from [`PtySession::write_input`]
/// so the pty's actual (possibly blocking) `write` syscall never happens on a caller's
/// thread. Exits once its `Sender` is dropped (channel disconnected) or a write fails.
///
/// `failed` is raised on a real write/flush error before the loop exits, and is what lets
/// [`PtySession::write_input`] stop reporting success once the pipe is genuinely broken - see
/// that method's own docs for why "enqueued" and "written" are not the same claim.
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
    ///
    /// ## What `Ok` does and does not promise
    ///
    /// `Ok` means the bytes were handed to the writer thread, **not** that they reached the fd -
    /// the write itself happens later, on that thread, and cannot be awaited here without
    /// reintroducing exactly the blocking this indirection exists to avoid.
    ///
    /// What it *does* promise, since GitHub issue #288: the writer thread has not already died.
    /// It used to swallow a failed `write_all`/`flush` and exit silently, so every subsequent
    /// `write_input` kept returning `Ok` into a pipe that no longer went anywhere - and a caller
    /// that treats `Ok` as "delivered" (issue #288's review notes flip to `sent` on it) would
    /// then claim a delivery that never happened, with no way back. [`Self::writer_failed`] is
    /// raised by [`run_writer_loop`] before it breaks, and checked here first.
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

    /// Whether this session's writer thread has already failed a real write to the pty - see
    /// [`Self::write_input`]'s own docs.
    pub fn writer_failed(&self) -> bool {
        self.writer_failed.load(Ordering::SeqCst)
    }

    /// Resizes the pty. Propagates the resize down to the kernel, which will notify the
    /// child (e.g. via `SIGWINCH` on unix).
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
    ///
    /// Unix only - see the `#[cfg(windows)]` twin below for the narrower,
    /// direct-child-only equivalent (no process-group/tree concept on that platform; see
    /// the crate-level "Platform scope" docs).
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

    /// Windows equivalent of [`Self::kill`]: terminates the direct child only, via
    /// `portable_pty::Child::kill()` (`TerminateProcess` under the hood - see
    /// `portable-pty-0.9.0/src/win/mod.rs`) - see the crate-level "Platform scope" docs for why
    /// there is no process-tree kill here.
    ///
    /// This is a real, known regression relative to the unix path, not a cosmetic gap: any
    /// grandchild process the killed child had itself spawned (e.g. a subprocess an agent CLI
    /// started) is NOT terminated by this and can survive as a real orphan. Windows has no
    /// signal-based process-group equivalent without job objects, a distinct `unsafe`-FFI-heavy
    /// API surface this project's own no-`unsafe` rule deliberately rules out adding. Tracked as
    /// a real, named follow-up rather than fixed here.
    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            if let Ok(Some(status)) = child.try_wait() {
                self.exited = Some(status);
            }
        }
        Ok(())
    }

    /// Suspends the child's real process group *and* any descendants that escaped it (e.g. via
    /// their own `setsid()`) via a real `SIGSTOP`, without killing anything - the process-level
    /// primitive GitHub issue #242 phase B's interactive-rebase UI uses to freeze a running
    /// agent process (and everything it spawned - a build step, a tool call) while a rebase
    /// rewrites files out from under it (see `crate::work_surface::agents::Agents::
    /// pause_agents_for_cwd` in the `app` crate, the real caller). Mirrors
    /// [`terminate_process_tree`]'s own real process-group-plus-escaped-descendants approach,
    /// for the same reason: an earlier version of this method sent a plain `kill(pid, SIGSTOP)`
    /// against only the direct child, which left every real grandchild process running and
    /// still writing files - exactly the hazard "Pause now" exists to remove. The counterpart to
    /// [`Self::resume`].
    ///
    /// A no-op, not an error, if the child has already exited (mirrors [`Self::kill`]'s own
    /// "already gone" handling) or if the platform exposes no pid at all. The direct process
    /// group's own `killpg` is the primary target and surfaces a real error if it fails;
    /// individual escaped descendants are signaled best-effort (errors ignored, matching
    /// [`terminate_process_tree`]'s own "make a best effort" contract) so one already-exited
    /// descendant can't stop the primary group from being paused.
    ///
    /// Unix only - see [`Self::resume`]'s Windows twin below for why there is no real equivalent
    /// there (mirrors [`Self::kill`]'s own unix/windows split, same crate-level "Platform scope"
    /// docs).
    #[cfg(unix)]
    pub fn pause(&self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        let Some(pid) = self.process_id() else {
            return Ok(());
        };
        // Collected *before* signaling anything, matching `collect_descendant_pids`'s own
        // ordering contract - irrelevant for reparenting here (nothing dies), but a stopped
        // process cannot fork further children, so whatever this walk finds is the complete,
        // frozen-in-place set from the moment `killpg` below actually lands.
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

    /// Resumes a process (and its process group and escaped descendants) previously
    /// [`Self::pause`]d via a real `SIGCONT`, in the reverse order `pause` signaled them
    /// (descendants first, then the process group) - safe to call even if the process was never
    /// actually paused (`SIGCONT` on an already-running process is a harmless no-op at the OS
    /// level) - see [`Self::pause`]'s own docs for the process-group-plus-descendants shape and
    /// its error-handling split.
    #[cfg(unix)]
    pub fn resume(&self) -> Result<(), PtyError> {
        if self.exited.is_some() {
            return Ok(());
        }
        let Some(pid) = self.process_id() else {
            return Ok(());
        };
        // A stopped process tree cannot fork further children on its own, so re-walking here
        // (rather than remembering `pause`'s own set) finds exactly the same real descendants,
        // still frozen exactly as `pause` left them.
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

    /// Windows has no `SIGSTOP`/`SIGCONT` equivalent without a distinct, `unsafe`-FFI-heavy job-
    /// object API this project's no-`unsafe` rule rules out adding (mirrors [`Self::kill`]'s own
    /// Windows-side process-tree limitation, same crate-level "Platform scope" docs) - a real,
    /// honest error rather than silently pretending to pause. The caller (`app`'s interactive-
    /// rebase UI) is documented to only ever offer its "Pause now" action where this can
    /// actually succeed.
    #[cfg(windows)]
    pub fn pause(&self) -> Result<(), PtyError> {
        Err(PtyError::Signal(
            "pausing a process is not supported on this platform".to_string(),
        ))
    }

    /// See [`Self::pause`]'s Windows twin above - there is nothing real to resume from either.
    #[cfg(windows)]
    pub fn resume(&self) -> Result<(), PtyError> {
        Err(PtyError::Signal(
            "resuming a process is not supported on this platform".to_string(),
        ))
    }

    /// Deterministically tears the session down: signals the child's process tree
    /// (`SIGHUP`, a bounded grace period, then `SIGKILL`), blocks until the direct
    /// child is reaped, signals the reader thread to stop via the shutdown pipe and
    /// joins it, then closes the writer channel and joins the writer thread. Meant to
    /// be called from a background task (it can block for up to
    /// [`SHUTDOWN_GRACE_PERIOD`] plus however long the child takes to actually exit
    /// after `SIGKILL`, which in practice is fast but is not itself bounded by this
    /// function). Safe to call more than once.
    ///
    /// Unix only - see the `#[cfg(windows)]` twin below, which (unlike an earlier version of
    /// this crate) now also makes the reader-thread join fully deterministic, by explicitly
    /// dropping `master` itself before joining.
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

    /// Windows equivalent of [`Self::shutdown`]. Kills the direct child (see the
    /// `#[cfg(windows)]` [`Self::kill`] docs) and blocks until it's reaped, then explicitly
    /// drops the ConPTY master handle (`self.master = None`) *before* joining the reader
    /// thread.
    ///
    /// That ordering is load-bearing, not stylistic. There is no safe way to proactively
    /// interrupt this platform's blocked `read` (see the crate-level docs) - the *only* real
    /// thing that unblocks it is ConPTY itself closing, which only happens once every
    /// reference to `master` is gone (see [`run_reader_loop`]'s `#[cfg(windows)]` docs for the
    /// full `PsuedoCon`/`ClosePseudoConsole` chain). Killing the child alone does **not** do
    /// this - `child.kill()` only calls `TerminateProcess`, which never touches `master`. An
    /// earlier version of this function knew that and worked around it by handing the reader
    /// thread's `.join()` off to a detached background thread instead of blocking here - which
    /// meant `shutdown()` itself returned promptly, but the reader thread (and its OS thread)
    /// kept running, genuinely unjoined, until whatever later happened to drop the whole
    /// `PtySession` finally closed `master` for it. That's not really `shutdown()` doing its
    /// own job - it's `shutdown()` relying on its caller's own cleanup to finish it, and this
    /// crate's one real call site (`crate::terminal_pane::TerminalPane::shutdown` in the `app`
    /// crate) happened to always do that, but a future caller that kept the session alive after
    /// calling `shutdown()` would leak two permanently-blocked OS threads per session. Taking
    /// `master` here instead means the join below is genuinely bounded by this function's own
    /// return, matching what a caller reasonably expects from a function named `shutdown`.
    #[cfg(windows)]
    pub fn shutdown(&mut self) -> Result<(), PtyError> {
        if self.exited.is_none() {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let status = child.wait().map_err(PtyError::Wait)?;
                self.exited = Some(status);
            }
        }

        // Load-bearing, not cleanup - see this function's docs above for why dropping
        // `master` here (rather than leaving it for `PtySession`'s own eventual `Drop`) is
        // what actually closes ConPTY and unblocks the reader thread's `read`.
        self.master = None;

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
            #[cfg(unix)]
            if let Some(pid) = self.process_id() {
                // Zero grace: Drop must not block the caller (see crate-level docs).
                terminate_process_tree(pid, Duration::ZERO);
            }
            // Windows: no process-tree concept (see the crate-level "Platform scope" docs)
            // - `child.kill()` below is the direct-child-only equivalent, and (like
            // `Self::kill`'s Windows twin) leaves any grandchild the killed process itself
            // spawned as a real, un-terminated orphan - see that function's docs for this
            // tracked, real gap.
            #[cfg(windows)]
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
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
#[cfg(unix)]
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

#[cfg(unix)]
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
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

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
    ///
    /// unix-only: its one caller (`drop_terminates_entire_process_tree_including_escaped_grandchild`)
    /// is `#[cfg(unix)]`.
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
        let session = spawn(SpawnOptions::new("echo").arg("hello-pty-core"))
            .expect("spawning `echo hello-pty-core` should succeed");

        let output = drain_until_contains(&session, "hello-pty-core", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello-pty-core"),
            "expected pty output to contain the echoed text, got: {text:?}"
        );
    }

    /// A long stream must arrive over the output channel **complete and in order**, no matter
    /// how it happens to get split into chunks.
    ///
    /// This is the property [`READ_BUF_SIZE`] and [`OUTPUT_CHANNEL_CAPACITY`] are otherwise free
    /// to trade against each other without anyone noticing, and it is what breaks silently and
    /// catastrophically if they're changed carelessly. Every line carries its own index, so a
    /// dropped, duplicated or truncated chunk fails on the exact line it went wrong at rather
    /// than on a vague length mismatch.
    ///
    /// The consumer below deliberately drains **slowly**, pausing every
    /// [`OUTPUT_CHANNEL_CAPACITY`] receives. That is the point of the test and not an accident:
    /// a tight `recv` loop keeps the channel near-empty, so the reader thread never blocks in
    /// `send` and the bounded-channel backpressure path - the one the real consumer
    /// (`app`'s `TerminalPane`, which drains on an 8ms timer) exercises constantly, and the one
    /// where a mistake would actually lose bytes - goes completely untested. Pausing forces the
    /// channel to fill, the reader to block mid-stream, and the whole fill/block/resume cycle to
    /// repeat many times over the run.
    ///
    /// Ordering is asserted but, honestly, is structurally guaranteed rather than at risk: one
    /// reader thread sends into one FIFO `mpsc`. The assertion is cheap insurance against that
    /// structure changing, not a live hazard being probed.
    ///
    /// `seq` writes into a pty, so the line discipline turns each `\n` into `\r\n`; the
    /// comparison below normalizes that rather than pretending it doesn't happen.
    #[test]
    fn long_output_arrives_complete_and_in_order_across_chunk_boundaries() {
        const LINES: usize = 200_000;

        let session = spawn(SpawnOptions::new("seq").arg("1").arg(LINES.to_string()))
            .expect("spawning `seq` should succeed");

        // Drain to EOF (the reader thread's `output_tx` drops once the pty closes), not to a
        // needle: this test is about the *whole* stream, so stopping early would hide loss.
        let mut collected = Vec::new();
        let mut received = 0usize;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match session.output().recv_timeout(remaining) {
                Ok(chunk) => {
                    collected.extend_from_slice(&chunk);
                    received += 1;
                    // See the doc comment: this pause is what makes the channel actually fill
                    // and the reader thread actually block, which is the path under test.
                    if received.is_multiple_of(OUTPUT_CHANNEL_CAPACITY) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => break,
            }
        }

        let text = String::from_utf8(collected).expect("`seq` output is plain ASCII");
        let normalized = text.replace("\r\n", "\n");
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

    // unix-only: uses pid_exists/is_executable directly, which are #[cfg(unix)]
    // helper functions (see the crate-level "Platform scope" docs).
    #[cfg(unix)]
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

    // unix-only: uses pid_exists/is_executable directly, which are #[cfg(unix)]
    // helper functions (see the crate-level "Platform scope" docs).
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

    /// GitHub issue #242 phase B: real, live-observed proof that [`PtySession::pause`]/
    /// [`PtySession::resume`] genuinely stop and restart a real process, not just that they
    /// return `Ok(())` - reads `/proc/<pid>/status`'s real `State:` line, which the kernel
    /// itself only ever reports as `T (stopped)` for a process a real `SIGSTOP` has actually
    /// landed on (a `SIGCONT`-resumed process goes back to `S (sleeping)`/`R (running)`).
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

        // Give the kernel a moment to settle the freshly spawned process into a steady
        // running/sleeping state before asserting anything about it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !proc_state(pid).starts_with('S') && !proc_state(pid).starts_with('R') {
            assert!(
                Instant::now() < deadline,
                "process never reached a steady state"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        session.pause().expect("pause should succeed");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = proc_state(pid);
            if state.starts_with('T') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} never reached the real kernel-reported stopped state \
                 (State: T) after pause() - last observed state: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        session.resume().expect("resume should succeed");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = proc_state(pid);
            if state.starts_with('S') || state.starts_with('R') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} never left the real kernel-reported stopped state after \
                 resume() - last observed state: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// GitHub issue #242 phase B fix: an earlier version of `pause`/`resume` sent a plain
    /// `kill(pid, SIGSTOP)` against only the direct child - real, live-reproduced proof that the
    /// fixed version really reaches a real *grandchild* process too (one that escaped the
    /// process group via its own `setsid()`, exactly `terminate_process_tree`'s own escaped-
    /// descendant case), not just the direct child. If this regressed back to a bare `kill`,
    /// the grandchild would keep running (and keep writing files) as if nothing had paused at
    /// all - exactly the silent hazard "Pause now" exists to remove.
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
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let state = proc_state(pid);
                if state.starts_with(prefix) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "process {pid} never reached {what} - last observed state: {state:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while session
            .try_wait()
            .expect("try_wait should not error")
            .is_none()
        {
            assert!(Instant::now() < deadline, "`true` never exited");
            std::thread::sleep(Duration::from_millis(20));
        }
        session
            .pause()
            .expect("pause on an already-exited child must be a harmless no-op");
        session
            .resume()
            .expect("resume on an already-exited child must be a harmless no-op");
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

    /// The healthy half of GitHub issue #288's writer-health check: a live session must report a
    /// working writer and must keep accepting writes. The check is a real gate on
    /// `write_input`, so a version of it that were ever true by accident would silently make
    /// every paste and every sent prompt in the app fail.
    #[test]
    fn a_live_session_reports_a_healthy_writer_and_keeps_accepting_writes() {
        let session = spawn(SpawnOptions::new("cat")).expect("spawning `cat` should succeed");
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

    /// And the closed half: once the session has been shut down there is no writer at all, so a
    /// write must be a typed error rather than a silent success into nothing.
    #[test]
    fn a_shut_down_session_refuses_writes() {
        let mut session = spawn(SpawnOptions::new("cat")).expect("spawning `cat` should succeed");
        session.shutdown().expect("shutdown");
        assert!(matches!(
            session.write_input(b"too late"),
            Err(PtyError::WriterClosed)
        ));
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
            .as_ref()
            .expect("session should still have a live master handle")
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

    /// `sh` is as close to a guaranteed-present binary as a unix test environment gets.
    /// This crate now *builds* cross-platform (see the crate-level "Platform scope"
    /// docs), but its test suite - this test included - still assumes a real unix shell
    /// environment throughout (every other test here spawns coreutils/shell binaries like
    /// `echo`/`sleep`/`sh` unconditionally too), matching this project's CI, which only
    /// runs `cargo build` (not `cargo test`) on non-Linux. Real end-to-end proof that
    /// [`resolve_on_path`] finds a real binary and returns a path that is itself real
    /// (exists, is a file, is executable) - not just "found something".
    // unix-only: uses pid_exists/is_executable directly, which are #[cfg(unix)]
    // helper functions (see the crate-level "Platform scope" docs).
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

    /// The real, live-reproduced Windows bug this closes: a directory holding both a bare,
    /// extension-less file (what Volta/nvm-windows ship as the POSIX shim half of an npm
    /// global install, e.g. `typescript-language-server`) and a real `.cmd` sibling
    /// (`typescript-language-server.cmd`) must resolve to the `.cmd` - `CreateProcessW` can't
    /// run the bare file at all (it has no PE header, just a shell script), which is exactly
    /// how `%1 is not a valid Win32 application` gets reported once that bare path reaches
    /// `Command::new` in `LspClient::spawn`. Builds a real, isolated `PATH` (not the process's
    /// real one) so a genuine `.cmd` elsewhere on it can't accidentally make this pass for the
    /// wrong reason - see `resolve_on_path_skips_a_same_named_directory`'s docs for why a
    /// custom `PATH` is passed directly rather than mutating `std::env`.
    #[cfg(windows)]
    #[test]
    fn resolve_in_path_var_prefers_a_pathext_extension_over_a_same_named_extensionless_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let bare = tmp.path().join("myprog");
        let cmd = tmp.path().join("myprog.cmd");
        std::fs::write(&bare, "#!/bin/sh\necho not runnable on windows\n").expect("write bare");
        std::fs::write(&cmd, "@echo off\r\necho real windows shim\r\n").expect("write cmd");

        let path_var = std::env::join_paths(std::iter::once(tmp.path().to_path_buf()))
            .expect("joining a single PATH entry should not fail");

        // Reads the real, process-global `%PATHEXT%` rather than overriding it -
        // `std::env::set_var` needs an `unsafe` block this workspace's `unsafe_code = "deny"`
        // lint forbids (see `Cargo.toml`), and every real Windows install already includes
        // `.CMD` in its default `PATHEXT` (`.COM;.EXE;.BAT;.CMD;...`), which is the one this
        // test's real bug report (`typescript-language-server.cmd`) actually depends on.
        let result = resolve_in_path_var(&path_var, "myprog");

        // Windows' filesystem is case-insensitive, and `with_extension` applies whatever
        // casing `%PATHEXT%` itself uses (typically `.CMD`, not `.cmd`) - so the real resolved
        // path and the `myprog.cmd` this test wrote can differ only in extension casing while
        // still naming the same real file. `eq_ignore_ascii_case` on the full path string
        // captures that; a plain `PathBuf` equality would not.
        let resolved = result.expect("a real `.cmd` sibling should have resolved");
        assert!(
            resolved
                .to_string_lossy()
                .eq_ignore_ascii_case(&cmd.to_string_lossy()),
            "the real, runnable `.cmd` sibling must win over the same-named extension-less \
             file, not the other way around - got {resolved:?}, expected (case-insensitively) \
             {cmd:?}"
        );
    }
}
