//! Commit-rewriting operations: cherry-pick, revert, and rebase.
//!
//! None of these resolve conflicts; a conflicting operation leaves the worktree where the
//! equivalent command line would, for the caller to resolve.
//!
//! [`cherry_pick`]/[`revert`] pass `--no-edit` and [`rebase_onto`] is never interactive, because
//! `crate::git_command` always closes stdin - an editor here could never open.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{check_success, run_git};

/// Applies `commit`'s changes as a new commit on top of the current branch.
///
/// Performs blocking I/O.
pub fn cherry_pick(worktree_path: &Path, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["cherry-pick".into(), "--no-edit".into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Commits the inverse of `commit`, leaving `commit` itself in history.
///
/// Performs blocking I/O.
pub fn revert(worktree_path: &Path, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["revert".into(), "--no-edit".into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Replays the current branch's commits - those not already reachable from `onto` - on top of it.
///
/// Plain and non-interactive; a conflict stops where the command line would.
///
/// Performs blocking I/O.
pub fn rebase_onto(worktree_path: &Path, onto: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["rebase".into(), onto.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
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
        dir
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) -> String {
        fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
        git_output(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn cherry_pick_really_applies_the_commits_change_on_top_of_head() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        // A second, unrelated branch line whose tip we'll cherry-pick back onto main.
        git(repo.path(), &["checkout", "-b", "feature"]);
        let feature_sha = commit(repo.path(), "b.txt", "feature content", "feature work");
        git(repo.path(), &["checkout", "main"]);

        cherry_pick(repo.path(), &feature_sha).expect("cherry-pick");

        let head_subject = git_output(repo.path(), &["log", "-1", "--format=%s"]);
        assert_eq!(head_subject, "feature work");
        assert_eq!(
            fs::read_to_string(repo.path().join("b.txt")).expect("read b.txt"),
            "feature content",
            "the cherry-picked file must really exist with its real content on the new commit"
        );
    }

    #[test]
    fn cherry_pick_a_real_conflict_surfaces_as_a_real_error_leaving_conflict_state_behind() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["checkout", "-b", "feature"]);
        let feature_sha = commit(repo.path(), "a.txt", "feature change", "feature work");
        git(repo.path(), &["checkout", "main"]);
        commit(
            repo.path(),
            "a.txt",
            "conflicting main change",
            "main diverges",
        );

        let result = cherry_pick(repo.path(), &feature_sha);
        assert!(
            result.is_err(),
            "a genuinely conflicting cherry-pick must surface as a real error"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(!stderr.is_empty(), "git's own stderr must be preserved");
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        assert!(
            repo.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "the worktree must be left in the real conflicted cherry-pick state, not silently \
             reset, so the user's own conflict-resolution flow can pick it up"
        );
    }

    #[test]
    fn revert_really_creates_a_new_commit_undoing_the_targets_change() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        let to_revert = commit(repo.path(), "a.txt", "changed", "the change to undo");

        revert(repo.path(), &to_revert).expect("revert");

        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "base",
            "reverting must really restore the file's prior content"
        );
        let log = git_output(repo.path(), &["log", "--format=%s"]);
        assert!(
            log.contains("the change to undo"),
            "revert must leave the original commit in history, not rewrite it away"
        );
    }

    #[test]
    fn revert_a_real_conflict_surfaces_as_a_real_error_leaving_conflict_state_behind() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        let to_revert = commit(repo.path(), "a.txt", "changed", "the change to undo");
        commit(
            repo.path(),
            "a.txt",
            "changed again differently",
            "later edit",
        );

        let result = revert(repo.path(), &to_revert);
        assert!(
            result.is_err(),
            "a genuinely conflicting revert must surface as a real error"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(!stderr.is_empty(), "git's own stderr must be preserved");
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        assert!(
            repo.path().join(".git/REVERT_HEAD").exists(),
            "the worktree must be left in the real conflicted revert state, not silently reset"
        );
    }

    fn rebase_head_exists(dir: &Path) -> bool {
        Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--verify", "--quiet", "REBASE_HEAD"])
            .output()
            .expect("failed to spawn git")
            .status
            .success()
    }

    #[test]
    fn rebase_onto_really_replays_the_current_branchs_commits_on_top_of_the_target() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        let base_sha = git_output(repo.path(), &["rev-parse", "HEAD"]);

        git(repo.path(), &["checkout", "-b", "feature", &base_sha]);
        commit(repo.path(), "b.txt", "feature content", "feature work");

        git(repo.path(), &["checkout", "main"]);
        let target_sha = commit(repo.path(), "c.txt", "main content", "main advances");

        git(repo.path(), &["checkout", "feature"]);
        rebase_onto(repo.path(), &target_sha).expect("rebase onto main");

        let log = git_output(repo.path(), &["log", "--format=%s"]);
        assert_eq!(
            log, "feature work\nmain advances\nbase",
            "the feature commit must now sit on top of main's real tip, not its old base"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("c.txt")).expect("read c.txt"),
            "main content",
            "the replayed branch must really contain main's own content too"
        );
    }

    #[test]
    fn rebase_onto_a_real_conflict_surfaces_as_a_real_error_leaving_rebase_state_behind() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        let base_sha = git_output(repo.path(), &["rev-parse", "HEAD"]);

        git(repo.path(), &["checkout", "-b", "feature", &base_sha]);
        commit(repo.path(), "a.txt", "feature change", "feature work");

        git(repo.path(), &["checkout", "main"]);
        let target_sha = commit(
            repo.path(),
            "a.txt",
            "conflicting main change",
            "main diverges",
        );

        git(repo.path(), &["checkout", "feature"]);
        let result = rebase_onto(repo.path(), &target_sha);
        assert!(
            result.is_err(),
            "a genuinely conflicting rebase must surface as a real error"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(!stderr.is_empty(), "git's own stderr must be preserved");
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        assert!(
            rebase_head_exists(repo.path()),
            "the worktree must be left in the real conflicted rebase state, not silently reset, \
             so the user's own conflict-resolution flow can pick it up"
        );
    }
}
