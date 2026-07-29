//! `/proc`-based process-tree discovery and signaling, mirroring `pty-core`'s own kill
//! implementation (`crates/pty-core/src/lib.rs`'s `child_pids_of`/`collect_descendant_pids`/
//! `terminate_process_tree`), though not a literal shared dependency: pty-core doesn't expose
//! those as `pub`, and pty-core's child is a pty session/process-group leader signaled via
//! `killpg`, while `rust-analyzer` here is a plain `std::process::Command` child with no pty
//! and no `setsid()`, so a plain `kill` to each discovered pid is needed, not a process-group
//! signal. The same reasoning applies though: `rust-analyzer` spawns its own child processes
//! (`cargo check`, `rustc`, proc-macro server processes, ...) while indexing, and killing only
//! the direct `rust-analyzer` pid would not necessarily reach those - see
//! [`collect_descendant_pids`].

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Depth cap for the `/proc` descendant-tree walk - purely defensive against a pathological
/// process tree; rust-analyzer's own real child processes are nowhere near this deep.
const TREE_WALK_MAX_DEPTH: usize = 8;

/// Reads the current direct children of `pid` from Linux's `/proc/<pid>/task/<pid>/children`.
/// Best-effort: returns an empty list if the file can't be read (process already gone,
/// non-Linux unix, permissions, ...) rather than erroring - used only for teardown cleanup,
/// where "found nothing else to clean up" is an acceptable fallback.
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

/// Breadth-first, depth-capped walk of `root_pid`'s descendant tree via `/proc`. Must be called
/// *before* signaling anything - reading it after a process starts dying races against the
/// kernel reparenting its children out from under `children` (the same real ordering
/// requirement `pty-core::collect_descendant_pids`'s own docs describe).
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

/// Real, direct `/proc/<pid>` existence check - not a cached or assumed answer.
pub fn pid_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// Sends `signal` to a real, single pid via `nix::sys::signal::kill`. Errors (e.g. `ESRCH`
/// because the target already exited) are intentionally ignored by every caller here - the
/// job is "make a best-effort attempt to reach every discovered pid," not to report which of
/// potentially many already-gone targets could or couldn't be signaled.
pub fn signal_pid(pid: u32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal);
}

/// Signals `root_pid` and every real descendant [`collect_descendant_pids`] discovers with
/// `SIGTERM` (rust-analyzer's own graceful-shutdown signal), waits up to `grace` (polling, not
/// a fixed sleep) for voluntary exit, then unconditionally follows up with `SIGKILL` on
/// whatever (if anything) is still alive. Pure signal-sends plus a bounded poll loop - never
/// blocks on `waitpid` itself (the caller is responsible for reaping the direct child).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn collect_descendant_pids_finds_a_real_child_process() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning `sh -c 'sleep 30'` should succeed");
        let sh_pid = child.id();

        // Give the shell a moment to actually exec `sleep` as its own child.
        std::thread::sleep(Duration::from_millis(200));

        let descendants = collect_descendant_pids(sh_pid);
        assert!(
            !descendants.is_empty(),
            "expected `sh -c 'sleep 30'` to have spawned a real, discoverable `sleep` child"
        );

        let _ = child.kill();
        let _ = child.wait();
        for pid in descendants {
            signal_pid(pid, nix::sys::signal::Signal::SIGKILL);
        }
    }

    #[test]
    fn terminate_tree_kills_a_real_process_and_its_real_child() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning the shell pipeline should succeed");
        let sh_pid = child.id();
        std::thread::sleep(Duration::from_millis(200));

        let descendants = collect_descendant_pids(sh_pid);
        assert!(!descendants.is_empty(), "expected a real sleep grandchild");

        terminate_tree(sh_pid, Duration::from_millis(500));
        let _ = child.wait();

        assert!(!pid_exists(sh_pid), "the shell itself should be gone");
        for pid in descendants {
            assert!(!pid_exists(pid), "descendant pid {pid} should be gone too");
        }
    }
}
