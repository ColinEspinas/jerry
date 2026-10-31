//! Windows only: gives this host its own kill-on-close job for every session it spawns
//! (`docs/architecture/decisions.md` §14 point 3, §24) - so a force-killed or crashed
//! `jerry-host` does not leak the PTY processes it owns, independent of whatever job (if any)
//! contains the host itself (it may have arrived here via `CREATE_BREAKAWAY_FROM_JOB`). Copied
//! rather than shared across a crate boundary from `jerry-app`'s own private `job_object.rs` -
//! the same precedent `crate::session::session_manager_tests`' own `process_is_alive` pair
//! already set for one small platform primitive.

#![cfg(windows)]
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

/// Puts this process in a fresh kill-on-close job so every session it spawns dies with it,
/// however it dies. Call once, before anything spawns - idempotent. Failure is logged and
/// non-fatal: without the job, cleanup degrades to whatever tree-kill each session already does
/// on its own exit.
pub(crate) fn adopt_this_process() {
    static ADOPTED: std::sync::Once = std::sync::Once::new();
    ADOPTED.call_once(|| match adopt_this_process_returning_job() {
        Ok(_job) => {
            // The handle is deliberately never closed. This process is a member of a
            // kill-on-close job, so closing the last handle would terminate it; the kernel
            // closes it when this process dies, which is the trigger doing its job.
            log::info!("jerry-host: sessions are adopted by a kill-on-close job object");
        }
        Err(err) => {
            log::warn!(
                "jerry-host: could not set up the kill-on-close job object ({err}) - sessions \
                 of a force-killed host will outlive it"
            );
        }
    });
}

fn adopt_this_process_returning_job() -> io::Result<HANDLE> {
    let job = create_kill_on_close_job()?;
    // SAFETY: `GetCurrentProcess` takes nothing and returns the process's own pseudo-handle,
    // which is valid for the whole call and needs no closing.
    let this_process = unsafe { GetCurrentProcess() };
    if let Err(err) = assign_process(job, this_process) {
        // SAFETY: `job` came from a successful `CreateJobObjectW` and is closed exactly once
        // here; the assignment failed, so this process is not a member and the close kills
        // nothing.
        unsafe { CloseHandle(job) };
        return Err(err);
    }
    Ok(job)
}

/// A fresh, unnamed job object whose members are terminated when its last handle closes
/// (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), with breakaway permitted
/// (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`) for symmetry with `jerry-app`'s own job - nothing here
/// spawns a breakaway child today, but a job that forbids it for no reason is a trap for later.
fn create_kill_on_close_job() -> io::Result<HANDLE> {
    // SAFETY: both parameters are null by contract - default security, which also makes the
    // handle non-inheritable (load-bearing: an inherited copy in a child would keep the job
    // alive past this process's death), and no name. Reads nothing from this process; returns a
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
    // SAFETY: `job` is the live handle just created above. The information pointer addresses a
    // live, uniquely borrowed stack `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` whose length is the
    // struct's real size taken from the type itself, the pair this call's contract requires for
    // `JobObjectExtendedLimitInformation`; the callee only reads it and does not retain the
    // pointer.
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
        // SAFETY: `job` came from a successful `CreateJobObjectW`, nothing was assigned to it,
        // and it is closed exactly once here.
        unsafe { CloseHandle(job) };
        return Err(err);
    }
    Ok(job)
}

/// Assigns `process` to `job`. Members it spawns afterwards join automatically.
fn assign_process(job: HANDLE, process: HANDLE) -> io::Result<()> {
    // SAFETY: takes two handles the caller owns for the whole call and borrows no memory from
    // this process, so there is nothing for it to invalidate.
    let ok = unsafe { AssignProcessToJobObject(job, process) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod kill_on_close_job_tests {
    use super::{adopt_this_process_returning_job, assign_process, create_kill_on_close_job};
    use std::os::windows::io::AsRawHandle;
    use std::process::Stdio;
    use std::time::Duration;
    use test_support::{wait_until, ChildGuard};
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;

    /// A real child that blocks until killed: `pause` reads from a stdin pipe whose write end
    /// this test holds open, so nothing ever arrives.
    fn blocked_child() -> ChildGuard {
        let mut command = jerry_pty::new_std_command("cmd.exe");
        command
            .args(["/d", "/c", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ChildGuard::spawn(&mut command).expect("cmd.exe must spawn")
    }

    #[test]
    fn closing_the_last_job_handle_kills_a_process_assigned_to_it() {
        let job = create_kill_on_close_job().expect("job creation must succeed on real Windows");
        let mut child = blocked_child();
        assert!(child.is_running(), "the fixture child must start out alive");
        assign_process(job, child.as_raw_handle() as HANDLE)
            .expect("assigning a live child to a fresh job must succeed");

        // SAFETY: `job` came from a successful `create_kill_on_close_job` and is closed exactly
        // once here. This test process was never assigned to it, so the close terminates only
        // the child - which is the behavior under test.
        unsafe { CloseHandle(job) };

        assert!(
            wait_until(Duration::from_secs(5), || !child.is_running()),
            "closing the last handle of a kill-on-close job must terminate its members"
        );
    }

    #[test]
    fn a_child_spawned_after_adoption_is_born_inside_the_job() {
        let job = adopt_this_process_returning_job()
            .expect("adopting the test process must succeed on real Windows");
        // This test's process is now inside a kill-on-close job, so `job` must stay open until
        // the process exits - leaked here exactly as production leaks it. nextest gives the
        // test its own process, so nothing else inherits that state.
        let mut child = blocked_child();

        let mut inside: BOOL = 0;
        // SAFETY: both handles are live for the whole call (`job` is never closed, the child
        // outlives the call under its guard), and the out-pointer addresses a live, uniquely
        // borrowed stack `BOOL` the callee writes exactly once.
        let ok = unsafe { IsProcessInJob(child.as_raw_handle() as HANDLE, job, &mut inside) };
        assert!(ok != 0, "IsProcessInJob must succeed for a live child");
        assert!(
            inside != 0,
            "a child spawned after adoption must be inside the job automatically"
        );
        child.kill_and_wait().expect("test child teardown");
    }
}
