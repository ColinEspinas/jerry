//! The Windows kill-on-close job object that stops spawned children outliving Jerry
//! (GitHub issue #482). `PtySession`'s tree kills only run while this process is alive to run
//! them; a force-killed or crashed Jerry runs no destructors, and a Windows child is otherwise
//! unaffected by its parent dying. Owns exactly one process-wide job; per-session kills stay
//! in `pty-core`.

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

/// Puts this process in a fresh kill-on-close job so every child it ever spawns dies with it,
/// however it dies. Call once, before anything can spawn.
///
/// Failure is logged and non-fatal: without the job, cleanup degrades to the `Drop`-time tree
/// kills that already exist, which is exactly the pre-#482 behavior.
pub fn adopt_this_process() {
    match adopt_this_process_returning_job() {
        Ok(_job) => {
            // The handle is deliberately never closed. This process is a member of a
            // kill-on-close job, so closing the last handle would terminate Jerry itself; the
            // kernel closes it when this process dies, which is the trigger doing its job.
            log::info!("child processes are adopted by a kill-on-close job object");
        }
        Err(err) => {
            log::warn!(
                "could not set up the kill-on-close job object ({err}) - children of a \
                 force-killed Jerry will outlive it"
            );
        }
    }
}

/// [`adopt_this_process`]'s fallible core, returning the job handle so a test can query
/// membership against it. The caller must keep the handle open for the life of the process.
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
/// (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`) so the updater's relaunch can escape it deliberately.
fn create_kill_on_close_job() -> io::Result<HANDLE> {
    // SAFETY: both parameters are null by contract - default security, which also makes the
    // handle non-inheritable (load-bearing: an inherited copy in a child would keep the job
    // alive past this process's death), and no name. Reads nothing from this process; returns
    // a handle, null on failure.
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
    // struct's real size taken from the type itself, the pair this call's contract requires
    // for `JobObjectExtendedLimitInformation`; the callee only reads it and does not retain
    // the pointer.
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
        let mut command = pty_core::new_std_command("cmd.exe");
        command
            .args(["/d", "/c", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ChildGuard::spawn(&mut command).expect("cmd.exe must spawn")
    }

    /// The kernel property the whole fix rests on: no destructor, no `taskkill`, just the last
    /// handle closing - which is what the OS does to every handle of a dead process.
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

    /// Children join the job at spawn because *this process* is a member - proving no
    /// per-spawn-site plumbing is needed for coverage.
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
