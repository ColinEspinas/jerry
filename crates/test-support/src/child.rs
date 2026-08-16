//! RAII teardown for a test that spawns a real process.
//!
//! A test that returns early — or fails an assertion, which unwinds — must not leave its child
//! behind: leaked children are what make a full-suite run peg a machine (`docs/testing.md`'s
//! hard rules).

use std::io;
use std::ops::{Deref, DerefMut};
use std::process::{Child, Command, ExitStatus};

/// A [`Child`] that is killed and reaped when it goes out of scope, however the test exits.
///
/// Derefs to the wrapped [`Child`], so `guard.stdin`, `guard.id()` and `guard.try_wait()` work
/// unchanged.
pub struct ChildGuard {
    // `Option` only so `into_inner` can hand the child back without `Drop` also killing it.
    child: Option<Child>,
}

impl ChildGuard {
    /// Takes ownership of an already-spawned child.
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Spawns `command` and guards the result.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.spawn().map(Self::new)
    }

    /// Whether the process is still running, without blocking on it.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child_mut().try_wait(), Ok(None))
    }

    /// Kills the process now and waits for it, rather than at end of scope. `Ok(None)` means it
    /// had already exited.
    pub fn kill_and_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let child = self.child_mut();
        if let Ok(Some(_)) = child.try_wait() {
            return Ok(None);
        }
        child.kill()?;
        child.wait().map(Some)
    }

    /// Gives up the guard's ownership — the caller becomes responsible for teardown.
    pub fn into_inner(mut self) -> Child {
        self.child.take().expect("child guard already consumed")
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child guard already consumed")
    }
}

impl Deref for ChildGuard {
    type Target = Child;

    fn deref(&self) -> &Child {
        self.child.as_ref().expect("child guard already consumed")
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child guard already consumed")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // Both results are deliberately discarded: an already-exited child reports "no such
            // process" here, and panicking inside `Drop` during an assertion failure's unwind
            // would replace the real failure message with an abort.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod child_teardown_tests {
    use crate::ChildGuard;
    use std::process::{Command, Stdio};

    /// A process that genuinely blocks until it is killed, using a binary every machine running
    /// this suite already needs: `git hash-object --stdin` reads until EOF, which never comes
    /// while the test holds the write end of the pipe.
    fn blocked_child() -> ChildGuard {
        let mut command = Command::new("git");
        command
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ChildGuard::spawn(&mut command).expect("spawn git hash-object")
    }

    #[test]
    fn kill_and_wait_stops_a_running_process() {
        let mut guard = blocked_child();
        assert!(
            guard.is_running(),
            "the fixture child must start out blocked"
        );

        let status = guard.kill_and_wait().expect("kill_and_wait");

        assert!(
            status.is_some(),
            "a running child must report the status it was killed with"
        );
        assert!(
            !guard.is_running(),
            "after kill_and_wait the process must really be gone, not merely signalled"
        );
    }

    #[test]
    fn kill_and_wait_is_a_no_op_for_a_child_that_already_exited() {
        let mut guard =
            ChildGuard::spawn(Command::new("git").arg("--version").stdout(Stdio::null()))
                .expect("spawn git --version");
        guard.wait().expect("wait for git --version");

        assert!(
            guard.kill_and_wait().expect("kill_and_wait").is_none(),
            "an already-exited child is not an error to tear down"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_guard_kills_the_process() {
        let pid = {
            let guard = blocked_child();
            guard.id().to_string()
        };

        let alive = Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .expect("spawn kill -0")
            .success();
        assert!(
            !alive,
            "the guard drops at the end of its scope, so pid {pid} must be killed and reaped by \
             the time the test reaches this line"
        );
    }
}
