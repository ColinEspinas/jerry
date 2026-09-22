//! Where the running process's own `jerry` binary lives, for [`crate::commands::RebaseStart`] to
//! hand `jerry-git` a real path for `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR`.
//!
//! A sibling of [`std::env::current_exe`], `bin/jerry` next to it, or `jerry` one directory up
//! (a cargo test binary's own exe lives in `target/<profile>/deps/`, not `target/<profile>/`) -
//! every real caller has one of these layouts. No `PATH` fallback: unlike `crate::host::
//! find_jerry_binary` in `jerry-app`, this never needs to reach across a split installation, and
//! a fallback would cost this crate the `jerry-pty` dependency it otherwise has none of.

use std::path::{Path, PathBuf};

/// The currently running process's own `jerry` binary, or `None` rather than a guess if no tier
/// finds one.
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
    // unix's `deps/` copy of a `[[bin]]` target is hash-suffixed (unlike Windows's, which the
    // sibling tier above already matches), so the only stable name is one level up, where cargo
    // places the real, unhashed artifact.
    if let Some(one_up) = dir.parent().map(|parent| parent.join(name)) {
        if one_up.is_file() {
            return Some(one_up);
        }
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
    fn one_directory_up_is_the_third_place_checked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let deps_dir = temp.path().join("deps");
        std::fs::create_dir_all(&deps_dir).expect("mkdir deps");
        let current_exe = deps_dir.join("some-test-binary-deadbeef");
        touch(&current_exe);
        let one_up = temp.path().join(exe_name());
        touch(&one_up);

        assert_eq!(locate_from(&current_exe), Some(one_up));
    }

    #[test]
    fn neither_tier_existing_is_a_real_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_exe = temp.path().join("current-exe");
        touch(&current_exe);

        assert_eq!(locate_from(&current_exe), None);
    }
}
