//! Where the running process's own `jerry` binary lives, for [`crate::commands::RebaseStart`] to
//! hand `jerry-git` a real path for `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR`.
//!
//! A sibling of [`std::env::current_exe`], or `bin/jerry` next to it - the two layouts every real
//! caller actually has: `jerry-cli`'s own binary (trivially a sibling of itself), and `jerry-app`
//! dispatching in-process, where the two binaries ship side by side
//! (`docs/architecture/decisions.md` Â§17). No `PATH` fallback: unlike hook/skill injection
//! (`crate::host::find_jerry_binary` in `jerry-app`, which does search `PATH`), this never needs
//! to reach across an installation that split the two binaries apart, and adding one here would
//! mean a `jerry-pty` dependency this crate deliberately has none of (Â§15's "standalone jerry-cli
//! can run [Git-locality commands] without linking any host or PTY machinery").

use std::path::{Path, PathBuf};

/// The currently running process's own `jerry` binary, or `None` rather than a guess if neither
/// tier exists.
pub fn locate() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    locate_from(&current_exe)
}

fn locate_from(current_exe: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "jerry.exe" } else { "jerry" };
    let dir = current_exe.parent()?;
    let sibling = dir.join(name);
    if sibling.is_file() {
        return Some(sibling);
    }
    let nested = dir.join("bin").join(name);
    if nested.is_file() {
        return Some(nested);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::locate_from;

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, b"").expect("write");
    }

    fn exe_name() -> &'static str {
        if cfg!(windows) {
            "jerry.exe"
        } else {
            "jerry"
        }
    }

    #[test]
    fn a_sibling_of_the_current_executable_is_found_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("some-other-bin");
        touch(&current_exe);
        let sibling = current_exe.with_file_name(exe_name());
        touch(&sibling);

        assert_eq!(locate_from(&current_exe), Some(sibling));
    }

    #[test]
    fn the_currently_running_binary_being_named_jerry_itself_resolves_to_itself() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join(exe_name());
        touch(&current_exe);

        assert_eq!(locate_from(&current_exe), Some(current_exe));
    }

    #[test]
    fn a_bin_subdirectory_is_the_second_place_checked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("current-exe");
        touch(&current_exe);
        let nested = temp.path().join("bin").join(exe_name());
        touch(&nested);

        assert_eq!(locate_from(&current_exe), Some(nested));
    }

    #[test]
    fn neither_tier_existing_is_a_real_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("current-exe");
        touch(&current_exe);

        assert_eq!(locate_from(&current_exe), None);
    }
}
