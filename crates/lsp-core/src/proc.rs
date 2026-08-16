//! Process-tree discovery and signaling, mirroring `pty-core`'s own kill
//! implementation (`crates/pty-core/src/lib.rs`'s `child_pids_of`/`collect_descendant_pids`/
//! `terminate_process_tree`), though not a literal shared dependency: pty-core doesn't expose
//! those as `pub`, and pty-core's child is a pty session/process-group leader signaled via
//! `killpg`, while `rust-analyzer` here is a plain `std::process::Command` child with no pty
//! and no `setsid()`, so a plain `kill` to each discovered pid is needed, not a process-group
//! signal. The same reasoning applies though: `rust-analyzer` spawns its own child processes
//! (`cargo check`, `rustc`, proc-macro server processes, ...) while indexing, and killing only
//! the direct `rust-analyzer` pid would not necessarily reach those - see
//! [`collect_descendant_pids`].
//!
//! ## Platform scope: unix only, same as `pty-core`
//!
//! Every function in this module is `#[cfg(unix)]`: the `nix` signal calls
//! (`kill`/`SIGTERM`/`SIGKILL`) they're built on are unix-specific - `nix` itself is only a
//! dependency at all `[target.'cfg(unix)'.dependencies]` in `Cargo.toml`. The descendant walk
//! itself is per-OS within unix, exactly as `pty-core`'s is: `/proc/<pid>/task/<pid>/children`
//! on Linux, `libproc`'s `proc_listchildpids` on macOS. On Windows there is no
//! descendant walk and no signal-based kill; the real equivalent this crate's two
//! callers (`LspClient::shutdown`/`Drop` in `client.rs`) use instead on that platform is a
//! direct `std::process::Child::kill()` on the already-held child handle (`TerminateProcess`
//! under the hood) - narrower than the unix path (it reaches only the direct `rust-analyzer`
//! process, not any `cargo check`/`rustc`/proc-macro-server descendants it spawned while
//! indexing, which can survive as real orphans on Windows), but real, not a no-op, mirroring
//! `pty-core`'s own `#[cfg(windows)] PtySession::kill`'s exact same narrowing and its own
//! documented reasoning for it (no process-tree-kill primitive on Windows without job objects,
//! a distinct `unsafe`-FFI-heavy API surface this project's no-`unsafe` rule rules out here).
//!
//! `#[cfg(unix)]`, not `#[cfg(not(unix))]`, gates the `client.rs` call sites for the same
//! reason `pty-core`'s own module docs give: the Windows-specific fallback (`Child::kill`) is
//! reasoned about specifically for Windows semantics, and gating on `cfg(windows)` makes any
//! other, genuinely different non-unix target (e.g. `wasm32-unknown-unknown`) fail to compile
//! loudly instead of silently picking up logic that was never reasoned about for it.

use std::collections::HashSet;
#[cfg(all(unix, not(target_os = "macos")))]
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Depth cap for the descendant-tree walk - purely defensive against a pathological
/// process tree; rust-analyzer's own real child processes are nowhere near this deep.
const TREE_WALK_MAX_DEPTH: usize = 8;

/// Reads the current direct children of `pid` from Linux's `/proc/<pid>/task/<pid>/children`.
/// Best-effort: returns an empty list if the file can't be read (process already gone,
/// a unix without procfs, permissions, ...) rather than erroring - used only for teardown
/// cleanup, where "found nothing else to clean up" is an acceptable fallback.
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
    // A parent with more direct children than this is pathological, not a real rust-analyzer
    // process tree; the doubling below stops here rather than growing without bound.
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
/// be called *before* signaling anything - reading it after a process starts dying races against
/// the kernel reparenting its children out from under it (the same real ordering
/// requirement `pty-core::collect_descendant_pids`'s own docs describe).
#[cfg(unix)]
pub fn collect_descendant_pids(root_pid: u32) -> Vec<u32> {
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

/// Real, direct existence check via the portable POSIX `kill(pid, 0)` probe - not a cached or
/// assumed answer, and not procfs, so it answers the same on every unix.
///
/// `EPERM` counts as "exists": the kernel only reports it for a process that is genuinely there
/// but not signalable by this user, and treating that as gone would make [`terminate_tree`]'s
/// grace loop declare victory over something still running.
#[cfg(unix)]
pub fn pid_exists(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None) {
        Ok(()) => true,
        Err(errno) => errno == nix::errno::Errno::EPERM,
    }
}

/// Sends `signal` to a real, single pid via `nix::sys::signal::kill`. Errors (e.g. `ESRCH`
/// because the target already exited) are intentionally ignored by every caller here - the
/// job is "make a best-effort attempt to reach every discovered pid," not to report which of
/// potentially many already-gone targets could or couldn't be signaled.
#[cfg(unix)]
pub fn signal_pid(pid: u32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal);
}

/// Signals `root_pid` and every real descendant [`collect_descendant_pids`] discovers with
/// `SIGTERM` (rust-analyzer's own graceful-shutdown signal), waits up to `grace` (polling, not
/// a fixed sleep) for voluntary exit, then unconditionally follows up with `SIGKILL` on
/// whatever (if anything) is still alive. Pure signal-sends plus a bounded poll loop - never
/// blocks on `waitpid` itself (the caller is responsible for reaping the direct child).
#[cfg(unix)]
pub fn terminate_tree(root_pid: u32, grace: Duration) {
    let descendants = collect_descendant_pids(root_pid);

    signal_pid(root_pid, nix::sys::signal::Signal::SIGTERM);
    for pid in &descendants {
        signal_pid(*pid, nix::sys::signal::Signal::SIGTERM);
    }

    if !grace.is_zero() {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            let all_gone = !pid_exists(root_pid) && descendants.iter().all(|pid| !pid_exists(*pid));
            if all_gone {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    signal_pid(root_pid, nix::sys::signal::Signal::SIGKILL);
    for pid in &descendants {
        signal_pid(*pid, nix::sys::signal::Signal::SIGKILL);
    }
}

// Every function above is `#[cfg(unix)]` (see this module's own doc comment on why), and
// the tests below exercise them directly (`collect_descendant_pids`, `signal_pid`,
// `terminate_tree`, `pid_exists`) - there is no non-unix variant of any of them to keep
// around, unlike `pty-core`'s own test module (which mixes unix-only and cross-platform
// tests, and so gates individual test functions instead).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use test_support::ChildGuard;

    /// The two answers [`pid_exists`] has to get right for [`terminate_tree`]'s grace loop to
    /// mean anything: a process that is genuinely running, and one that has already been
    /// reaped.
    #[test]
    fn pid_exists_separates_a_live_process_from_a_reaped_one() {
        assert!(
            pid_exists(std::process::id()),
            "this very test process is unambiguously alive"
        );

        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning `true` should succeed");
        let pid = child.id();
        child.wait().expect("reaping `true` should succeed");

        assert!(
            !pid_exists(pid),
            "pid {pid} was reaped, so it should read as gone rather than alive"
        );
    }

    /// `sleep 30 & wait`, not a bare `sleep 30`: macOS's `/bin/sh` `exec`s a lone final command
    /// in place rather than forking for it, so `sh -c 'sleep 30'` leaves a process *named*
    /// `sleep` at the shell's own pid and no child at all for this walk to find. Backgrounding
    /// forces a real fork on every shell - the same form the sibling test below already uses.
    #[test]
    fn collect_descendant_pids_finds_a_real_child_process() {
        // `ChildGuard`: the assertion below used to sit between the spawn and the manual kill,
        // so a failing walk left a real 30-second `sleep` tree behind.
        let mut child = ChildGuard::spawn(
            Command::new("sh")
                .arg("-c")
                .arg("sleep 30 & wait")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .expect("spawning `sh -c 'sleep 30 & wait'` should succeed");
        let sh_pid = child.id();

        // The shell forks `sleep` asynchronously, so poll for it rather than guessing how long
        // that takes on a loaded machine.
        let mut descendants = Vec::new();
        let forked = test_support::wait_until(Duration::from_secs(5), || {
            descendants = collect_descendant_pids(sh_pid);
            !descendants.is_empty()
        });
        assert!(
            forked,
            "expected `sh -c 'sleep 30 & wait'` to have spawned a real, discoverable `sleep` child"
        );

        child.kill_and_wait().expect("reap the shell");
        for pid in descendants {
            signal_pid(pid, nix::sys::signal::Signal::SIGKILL);
        }
    }

    #[test]
    fn terminate_tree_kills_a_real_process_and_its_real_child() {
        let mut child = ChildGuard::spawn(
            Command::new("sh")
                .arg("-c")
                .arg("sleep 30 & wait")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .expect("spawning the shell pipeline should succeed");
        let sh_pid = child.id();

        let mut descendants = Vec::new();
        assert!(
            test_support::wait_until(Duration::from_secs(5), || {
                descendants = collect_descendant_pids(sh_pid);
                !descendants.is_empty()
            }),
            "expected a real sleep grandchild"
        );

        terminate_tree(sh_pid, Duration::from_millis(500));
        let _ = child.wait();

        assert!(!pid_exists(sh_pid), "the shell itself should be gone");
        for pid in descendants {
            assert!(!pid_exists(pid), "descendant pid {pid} should be gone too");
        }
    }
}
