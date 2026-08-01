//! Real git-index staging primitives for the Changes panel's staging checkbox
//! (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §5: "The checkbox **is**
//! staging"). Before this module existed, `app::sidebar::render::AdeApp::toggle_staged` only
//! flipped an in-memory `HashSet<PathBuf>` - real git never saw anything until the commit
//! composer's own `git add` ran at commit time (`crate::undo::commit_paths`). That contradicted
//! the design's explicit framing: checking the box is supposed to be real, immediate staging,
//! not a UI-only intent recorded for later.
//!
//! [`stage_path`]/[`unstage_path`] are the real, immediate mutations `toggle_staged` now calls
//! on every click. [`staged_paths`] is the read side: a real `git diff --cached --name-only`
//! query, used both to re-derive `AdeApp::staged_files` when a worktree is first loaded or
//! switched to (so a file already staged in the real index before Jerry ever touched it reads as
//! staged, rather than starting every worktree at an empty, UI-only set) and by this module's own
//! tests to verify the real index changed.
//!
//! Performs blocking I/O everywhere in this module (shells out to `git`); see the crate-level
//! docs on offloading this to a background thread.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::{check_success, run_git};

/// Real, immediate `git add -- <path>` - stages `path` (relative to `worktree_path`, or
/// absolute so long as it resolves inside it) in the real git index. Works identically for a
/// modified tracked file, a new untracked file, or a deleted tracked file (`git add` stages a
/// deletion too, the same way `wt_core::undo::commit_paths`'s own `git add -- <paths>` already
/// relies on for its "stage exactly these paths, including deletions" behavior).
///
/// Performs blocking I/O.
pub fn stage_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["add".into(), "--".into(), path.as_os_str().to_owned()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real, immediate unstage: `git reset -- <path>` removes `path` from the real git index
/// without touching the working tree - the exact inverse of [`stage_path`]. A no-op (real,
/// successful exit) if `path` was already unstaged, matching `git reset`'s own idempotent
/// behavior, so a caller never needs to check [`staged_paths`] first just to avoid an error.
///
/// Performs blocking I/O.
pub fn unstage_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["reset".into(), "--".into(), path.as_os_str().to_owned()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// The real, current set of staged paths in `worktree_path`'s git index (`git diff --cached
/// --name-only`), worktree-relative. The live source of truth [`app::root::AdeApp::staged_files`]
/// re-derives from on every worktree load/switch, rather than starting empty and silently
/// disagreeing with a file already staged in the real index before Jerry ever opened this
/// worktree.
///
/// Performs blocking I/O.
pub fn staged_paths(worktree_path: &Path) -> Result<HashSet<PathBuf>, Error> {
    let args: Vec<OsString> = vec!["diff".into(), "--cached".into(), "--name-only".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "initial commit"]);
        dir
    }

    #[test]
    fn stage_path_really_adds_a_modified_file_to_the_real_index() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        stage_path(repo.path(), Path::new("file.txt")).expect("stage_path");

        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(
            status, "M  file.txt",
            "file.txt must be staged (M in the index column), not merely modified on disk"
        );
    }

    #[test]
    fn stage_path_really_adds_a_new_untracked_file() {
        let repo = init_repo();
        fs::write(repo.path().join("new.txt"), "new\n").expect("write new file");

        stage_path(repo.path(), Path::new("new.txt")).expect("stage_path");

        let status = git_output(repo.path(), &["status", "--porcelain", "new.txt"]);
        assert_eq!(
            status, "A  new.txt",
            "a new file must be staged as an addition"
        );
    }

    #[test]
    fn unstage_path_really_removes_a_path_from_the_real_index_without_touching_the_working_tree() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        git(repo.path(), &["add", "file.txt"]);
        let staged_before = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(staged_before, "M  file.txt");

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path");

        // `git_output` trims the whole string, so the leading `X` status char of a real
        // `" M file.txt"` porcelain line (unstaged-only modification: `X` is a blank index
        // column, `Y` is `M`) is trimmed away along with it - "M file.txt" (one space, not two)
        // is the real, correctly-unstaged shape here, not a staged `"M  file.txt"` (two spaces).
        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(
            status, "M file.txt",
            "file.txt must be unstaged (working-tree-only M) after a real unstage"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read file.txt"),
            "changed\n",
            "unstaging must never touch the real working-tree content"
        );
    }

    #[test]
    fn unstage_path_is_a_harmless_no_op_when_already_unstaged() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path on a clean index");

        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(status, "M file.txt");
    }

    #[test]
    fn staged_paths_reflects_the_real_git_index_including_files_staged_by_something_else() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("also.txt"), "also\n").expect("write");
        // Staged directly via a real `git add`, standing in for something other than
        // `stage_path` having touched the index first (an agent CLI, a manual `git add`) -
        // `staged_paths` must see it regardless of who staged it.
        git(repo.path(), &["add", "file.txt", "also.txt"]);

        let staged = staged_paths(repo.path()).expect("staged_paths");

        assert_eq!(
            staged,
            [PathBuf::from("file.txt"), PathBuf::from("also.txt")]
                .into_iter()
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn staged_paths_is_empty_on_a_clean_index() {
        let repo = init_repo();
        assert!(staged_paths(repo.path()).expect("staged_paths").is_empty());
    }

    #[test]
    fn staged_paths_never_includes_an_unstaged_modification() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed but not staged\n").expect("modify");

        let staged = staged_paths(repo.path()).expect("staged_paths");

        assert!(
            staged.is_empty(),
            "a real, unstaged modification must never appear in staged_paths"
        );
    }

    #[test]
    fn stage_then_unstage_round_trips_back_to_a_clean_staged_set() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        stage_path(repo.path(), Path::new("file.txt")).expect("stage_path");
        assert_eq!(
            staged_paths(repo.path()).expect("staged_paths"),
            [PathBuf::from("file.txt")].into_iter().collect()
        );

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path");
        assert!(staged_paths(repo.path()).expect("staged_paths").is_empty());
    }
}
