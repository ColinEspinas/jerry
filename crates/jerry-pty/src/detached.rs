//! Spawning a process that survives this one's own exit, even from inside a kill-on-close job
//! (`docs/architecture/decisions.md` §14/§24): `jerry-host`, detached from `jerry-app` or
//! `jerry-cli`. Windows escapes via `CREATE_BREAKAWAY_FROM_JOB`; unix via a new process group,
//! so nothing in the spawning process's own job/session can drag the child down with it.
//!
//! Every call here is a safe `std` wrapper (`creation_flags`/`process_group`) - no `unsafe`, and
//! this crate stays that way (CLAUDE.md's Rust standards). Detecting *why* a spawn failed - a
//! forbidding job, read via Win32 FFI - is `jerry-app`'s/`jerry-host`'s own
//! `job_object::breakaway_is_forbidden_for_current_process`, the two places that FFI is
//! sanctioned; a caller of `jerry_core::host_spawn::spawn_or_connect_with` injects it.

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
