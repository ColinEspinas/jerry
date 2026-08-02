//! Real git commit-rewriting operations: cherry-pick, revert, and rebase - GitHub issue #1's own
//! acceptance criteria ("cherry pick", "revert commit", "rebase branch"). Every mutation shells
//! out to a real `git` subprocess and surfaces git's own real stderr on failure
//! ([`Error::GitCommand`]), exactly `crate::remote`'s own discipline.
//!
//! None of these functions resolve a real conflict themselves - a conflicting cherry-pick,
//! revert, or rebase leaves the worktree exactly where the equivalent command-line invocation
//! would (a real `CHERRY_PICK_HEAD`/`REVERT_HEAD`/`REBASE_HEAD` and conflict markers in the
//! working tree), for the caller to resolve through this app's own conflict-resolution surface
//! or a real terminal, matching [`crate::remote::pull`]'s own "surfaces whatever git reports,
//! honestly" precedent. [`cherry_pick`]/[`revert`] run with `--no-edit`: this app has no surface
//! for editing a commit message mid-operation, so the original commit's own message is kept
//! unchanged rather than blocking on an editor that can never open (`crate::git_command`'s stdin
//! is always closed). [`rebase_onto`] is deliberately never interactive for the same reason - see
//! its own docs.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{check_success, run_git};

/// Real `git cherry-pick --no-edit <commit>` for `worktree_path`: applies `commit`'s changes as
/// a new commit on top of the current branch.
///
/// Performs blocking I/O.
pub fn cherry_pick(worktree_path: &Path, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["cherry-pick".into(), "--no-edit".into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real `git revert --no-edit <commit>` for `worktree_path`: creates a new commit that undoes
/// `commit`'s changes, leaving `commit` itself in history.
///
/// Performs blocking I/O.
pub fn revert(worktree_path: &Path, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["revert".into(), "--no-edit".into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real `git rebase <onto>` for `worktree_path`: replays the current branch's own commits (the
/// ones not already reachable from `onto`) on top of `onto` - the row menu's "Rebase onto this
/// commit" (GitHub issue #1). Always git's own plain, non-interactive replay: this app has no
/// commit-reordering/squash/edit UI yet (interactive rebase is a real, stated follow-up, not
/// this function), and a real conflict mid-replay stops exactly where the command-line
/// equivalent would, `--onto`-less `git rebase` needing no editor at any step (unlike
/// `cherry_pick`/`revert`, no `--no-edit` is needed here for that reason).
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
