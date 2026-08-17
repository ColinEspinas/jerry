//! `/proc` process-tree discovery and signalling, so killing `rust-analyzer` also reaches the
//! `cargo check`/`rustc`/proc-macro processes it spawns while indexing.
//!
//! Parallel to `pty-core`'s kill path rather than shared with it: that child is a process-group
//! leader signalled with `killpg`, while this one is a plain child needing a `kill` per pid.
//!
//! Unix only. Windows has neither `/proc` nor signals, so `client.rs` falls back to
//! `std::process::Child::kill()` - which reaches only the direct process, leaving descendants as
//! orphans, since a process-tree kill there needs job objects and their `unsafe` FFI.

use std::collections::HashSet;
#[cfg(all(unix, not(target_os = "macos")))]
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Depth cap for the descendant walk, defensive against a pathological process tree.
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

/// A direct `/proc/<pid>` existence check.
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

/// Sends `signal` to one pid. Callers ignore the error: the job is reaching every discovered pid,
/// not reporting which of them had already exited.
#[cfg(unix)]
pub fn signal_pid(pid: u32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal);
}

/// `SIGTERM`s `root_pid` and every descendant, polls up to `grace` for voluntary exit, then
/// `SIGKILL`s whatever is left.
///
/// Never blocks on `waitpid`; reaping the direct child is the caller's job.
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

// Everything above is `#[cfg(unix)]`, so the whole module is gated rather than each test.
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
