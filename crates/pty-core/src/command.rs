//! The workspace's one constructor for non-PTY child processes (`std::process::Command`).
//! Owns the same "how children are spawned on this OS" concern as [`crate::resolve_on_path`]:
//! on Windows the release binary is a GUI-subsystem process (`windows_subsystem = "windows"`),
//! where a console-subsystem child spawned with default creation flags allocates its own
//! visible console window — `CREATE_NO_WINDOW` suppresses that (GitHub issue #465). Not for
//! PTY children: `portable-pty`'s ConPTY spawn already attaches those to a headless
//! pseudo console.

use std::ffi::OsStr;
use std::process::Command;

/// A [`Command`] for `program` that never flashes a console window on Windows.
///
/// Every production spawn in the workspace constructs through here rather than
/// `Command::new` — enforced by `clippy.toml`'s `disallowed-methods`.
#[cfg(windows)]
// The one sanctioned bare constructor (see `clippy.toml`); everything else calls this fn.
#[allow(clippy::disallowed_methods)]
pub fn new_std_command(program: impl AsRef<OsStr>) -> Command {
    use std::os::windows::process::CommandExt as _;

    /// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// A [`Command`] for `program` — the identity constructor everywhere but Windows, kept as the
/// single shared entry point so call sites need no platform `cfg` of their own.
#[cfg(not(windows))]
// The one sanctioned bare constructor (see `clippy.toml`); everything else calls this fn.
#[allow(clippy::disallowed_methods)]
pub fn new_std_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

#[cfg(test)]
mod command_tests {
    use super::new_std_command;

    #[test]
    fn the_program_handed_in_is_the_program_the_command_runs() {
        let command = new_std_command("git");
        assert_eq!(command.get_program(), "git");
    }

    /// The creation flags must not poison an ordinary spawn: a child started through the
    /// helper still runs to completion and reports its exit status on every platform.
    #[test]
    fn a_child_spawned_through_the_helper_runs_to_completion() {
        let output = new_std_command("git")
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
