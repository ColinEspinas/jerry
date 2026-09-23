//! Spawning a process that survives this one's own exit, even from inside a kill-on-close job
//! (`docs/architecture/decisions.md` §14/§24): `jerry-host`, detached from `jerry-app` or
//! `jerry-cli`. Windows escapes via `CREATE_BREAKAWAY_FROM_JOB`; unix via a new process group,
//! so nothing in the spawning process's own job/session can drag the child down with it.

// Windows-only: every call here is either a safe `std` wrapper (`creation_flags`) or a Win32
// FFI query, each with its own SAFETY comment - see CLAUDE.md's Rust standards.
#![cfg_attr(windows, allow(unsafe_code))]

use std::ffi::OsStr;
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

/// A [`Command`] for `program`, configured to survive this process's own exit. Built on
/// [`crate::new_std_command`], so the Windows `CREATE_NO_WINDOW` hygiene still applies. Stdio is
/// pre-closed: a detached process must never share this one's console or pipes. The caller still
/// sets args/env and calls `.spawn()`.
pub fn new_detached_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = crate::new_std_command(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        // `creation_flags` REPLACES what `new_std_command` set rather than ORing into it, so
        // `CREATE_NO_WINDOW` is repeated here (docs/architecture/decisions.md §14).
        command.creation_flags(
            CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP,
        );
    }
    #[cfg(unix)]
    {
        // A new process group with this child as its leader: it does not die with this
        // process's own session or controlling terminal, mirroring the Windows breakaway above.
        command.process_group(0);
    }
    command
}

/// Windows only: whether the job this process currently belongs to (if any) forbids
/// `CREATE_BREAKAWAY_FROM_JOB` - read directly from the OS rather than guessed from a failed
/// spawn's error code, exactly as `docs/architecture/decisions.md` §14's spike proved.
/// `Ok(false)` also covers "not in a job at all", since nothing then forbids anything.
#[cfg(windows)]
pub fn breakaway_is_forbidden_for_current_process() -> std::io::Result<bool> {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut in_any_job: BOOL = 0;
    // SAFETY: `GetCurrentProcess` returns this process's own valid pseudo-handle; a null job
    // handle asks "in any job at all"; `in_any_job` addresses a live, uniquely borrowed stack
    // `BOOL` the callee writes exactly once.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_any_job) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if in_any_job == 0 {
        return Ok(false);
    }

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: a null job handle queries the calling process's own job (confirmed above to be a
    // member of exactly one); the buffer pointer addresses a live, uniquely borrowed stack value
    // sized exactly to `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, which the callee writes into and
    // does not retain past the call. The return-length pointer is null: the exact size asked for
    // is already known.
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_BREAKAWAY_OK == 0)
}

#[cfg(test)]
mod detached_command_tests {
    use super::new_detached_command;

    #[test]
    fn the_program_handed_in_is_the_program_the_command_runs() {
        let command = new_detached_command("git");
        assert_eq!(command.get_program(), "git");
    }

    #[test]
    fn a_detached_child_still_runs_to_completion_and_reports_its_exit_status() {
        let output = new_detached_command("git")
            .arg("--version")
            .output()
            .expect("`git --version` must spawn through the helper");
        assert!(
            output.status.success(),
            "`git --version` must exit 0, got {:?}",
            output.status
        );
    }
}

#[cfg(all(test, windows))]
mod breakaway_detection_tests {
    use super::breakaway_is_forbidden_for_current_process;

    /// nextest gives every test its own process, so this test's own job membership (if any) is
    /// whatever the test harness itself set up - never a job this test created, so the exact
    /// answer is unknown, but the OS query must at least succeed rather than error.
    #[test]
    fn querying_this_process_own_job_information_succeeds_on_real_windows() {
        breakaway_is_forbidden_for_current_process()
            .expect("IsProcessInJob/QueryInformationJobObject must succeed for this process");
    }
}
