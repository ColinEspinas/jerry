//! Windows-only kill-on-close job object for a test fixture that spawns a real child process
//! (GitHub issue #534): a test's own OS process can end without running any `Drop` at all (a
//! nextest `TerminateProcess` on timeout, an aborting panic) - the same hazard
//! `jerry_app::job_object` exists to close for the real app (issue #482), duplicated here rather
//! than shared across the crate boundary so `jerry-host` and `jerry-pty` - which cannot depend on
//! `jerry-app` - get the same coverage in their own tests.

#[cfg(windows)]
mod windows_job {
    // This module exists entirely to call Win32 FFI (`CreateJobObjectW`, `SetInformationJobObject`,
    // `AssignProcessToJobObject`) - every call site below carries its own `SAFETY` comment; see
    // CLAUDE.md's Rust standards for the project-wide "unsafe only for justified FFI" rule.
    #![allow(unsafe_code)]

    use std::io;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Puts this test process in a fresh kill-on-close job so every child it spawns dies with
    /// it, however it dies. Idempotent - safe to call once per spawning fixture, even from many
    /// fixtures in the same `#[gpui::test]`/`#[test]` process.
    pub fn adopt_this_process() {
        static ADOPTED: std::sync::Once = std::sync::Once::new();
        ADOPTED.call_once(|| match adopt_this_process_returning_job() {
            Ok(_job) => {
                // The handle is deliberately never closed - see `jerry_app::job_object`'s own
                // twin of this comment for why that is the point, not an oversight.
            }
            Err(err) => {
                eprintln!(
                    "test-support: could not set up the kill-on-close job object ({err}) - a \
                     child this test spawns may outlive a forced kill of this process"
                );
            }
        });
    }

    fn adopt_this_process_returning_job() -> io::Result<HANDLE> {
        let job = create_kill_on_close_job()?;
        // SAFETY: `GetCurrentProcess` takes nothing and returns the process's own pseudo-handle,
        // which is valid for the whole call and needs no closing.
        let this_process = unsafe { GetCurrentProcess() };
        // SAFETY: takes two handles the caller owns for the whole call and borrows no memory
        // from this process, so there is nothing for it to invalidate.
        let ok = unsafe { AssignProcessToJobObject(job, this_process) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // SAFETY: `job` came from a successful `CreateJobObjectW` and is closed exactly
            // once here; the assignment failed, so this process is not a member and the close
            // kills nothing.
            unsafe { CloseHandle(job) };
            return Err(err);
        }
        Ok(job)
    }

    fn create_kill_on_close_job() -> io::Result<HANDLE> {
        // SAFETY: both parameters are null by contract - default security, which also makes the
        // handle non-inheritable, and no name. Reads nothing from this process; returns a
        // handle, null on failure.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }

        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `job` is the live handle just created above. The information pointer
        // addresses a live, uniquely borrowed stack `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`
        // whose length is the struct's real size taken from the type itself; the callee only
        // reads it and does not retain the pointer.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // SAFETY: `job` came from a successful `CreateJobObjectW`, nothing was assigned to
            // it, and it is closed exactly once here.
            unsafe { CloseHandle(job) };
            return Err(err);
        }
        Ok(job)
    }

    #[cfg(test)]
    mod kill_on_close_job_tests {
        use super::{adopt_this_process_returning_job, create_kill_on_close_job};
        use crate::{wait_until, ChildGuard};
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};
        use std::time::Duration;
        use windows_sys::core::BOOL;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, IsProcessInJob};

        /// A real child that blocks until killed: `git hash-object --stdin` reads until EOF,
        /// which never comes while this test holds the write end of the pipe - the same
        /// fixture `crate::child::child_teardown_tests` uses, so this needs no extra binary.
        fn blocked_child() -> ChildGuard {
            let mut command = Command::new("git");
            command
                .args(["hash-object", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            ChildGuard::spawn(&mut command).expect("git hash-object must spawn")
        }

        /// The kernel property this whole module rests on: no destructor, no `taskkill`, just
        /// the last handle closing - which is what the OS does to every handle of a dead
        /// process. Pins `test-support`'s own copy of the mechanism, independent of
        /// `jerry_app::job_object`'s already-tested one, since neither crate can depend on the
        /// other's tests.
        #[test]
        fn closing_the_last_job_handle_kills_a_process_assigned_to_it() {
            let job =
                create_kill_on_close_job().expect("job creation must succeed on real Windows");
            let mut child = blocked_child();
            assert!(child.is_running(), "the fixture child must start out alive");
            // SAFETY: both handles are live for the whole call and borrow no memory from this
            // process, so there is nothing for it to invalidate.
            let assigned =
                unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) };
            assert!(
                assigned != 0,
                "assigning a live child to a fresh job must succeed"
            );

            // SAFETY: `job` came from a successful `create_kill_on_close_job` and is closed
            // exactly once here. This test process was never assigned to it, so the close
            // terminates only the child - which is the behavior under test.
            unsafe { CloseHandle(job) };

            assert!(
                wait_until(Duration::from_secs(5), || !child.is_running()),
                "closing the last handle of a kill-on-close job must terminate its members"
            );
        }

        /// Children join the job at spawn because *this process* is a member - the property
        /// [`adopt_this_process`](super::adopt_this_process) relies on for every fixture that
        /// calls it before spawning (GitHub issue #534).
        #[test]
        fn a_child_spawned_after_adoption_is_born_inside_the_job() {
            let job = adopt_this_process_returning_job()
                .expect("adopting the test process must succeed on real Windows");
            let mut child = blocked_child();

            let mut inside: BOOL = 0;
            // SAFETY: both handles are live for the whole call (`job` is never closed, the
            // child outlives the call under its guard), and the out-pointer addresses a live,
            // uniquely borrowed stack `BOOL` the callee writes exactly once.
            let ok = unsafe { IsProcessInJob(child.as_raw_handle() as HANDLE, job, &mut inside) };
            assert!(ok != 0, "IsProcessInJob must succeed for a live child");
            assert!(
                inside != 0,
                "a child spawned after adoption must be inside the job automatically"
            );
            child.kill_and_wait().expect("test child teardown");
        }
    }
}

#[cfg(windows)]
pub use windows_job::adopt_this_process;

/// Off Windows a child dies with its own process group / the pty closing already (see
/// `jerry-pty`'s unix teardown), so there is no job-object equivalent needed here.
#[cfg(not(windows))]
pub fn adopt_this_process() {}
