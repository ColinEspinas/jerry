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
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Depth cap for the descendant walk, defensive against a pathological process tree.
const TREE_WALK_MAX_DEPTH: usize = 8;

/// The direct children of `pid`. Best-effort: an unreadable file is an empty list, not an error.
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

/// Breadth-first, depth-capped walk of `root_pid`'s descendants.
///
/// Must run *before* any signal: reading it once a process is dying races the kernel reparenting
/// its children out from under the file.
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
    PathBuf::from(format!("/proc/{pid}")).exists()
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
