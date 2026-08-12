//! Real git `HEAD`/branch-pointer operations that are *not* history-rewrites: checking out a
//! commit, creating a branch at a commit, and resetting the current branch's tip - the git graph
//! row menu's "Check out" / "Create branch here" / Soft-Mixed-Hard reset (GitHub issue #241).
//!
//! Kept in a module of its own rather than folded into [`crate::rewrite`]: every one of
//! `rewrite`'s cherry-pick/revert/rebase-onto creates or replays a real commit, changing what
//! some commit *contains*; none of the three functions here ever do that - they only ever move
//! `HEAD` or a branch ref to point somewhere else. Same real-git-invocation discipline as
//! `rewrite` throughout: every mutation shells out to a real `git` subprocess and surfaces git's
//! own real stderr on failure ([`Error::GitCommand`]), nothing simulated or pre-validated beyond
//! what's needed to avoid a clearly-broken invocation.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{check_success, run_git};

/// Real `git checkout <commit>` for `worktree_path`: moves `HEAD` (detached) onto `commit`,
/// leaving the current branch pointer untouched - the row menu's "Check out".
///
/// `commit` is always a real object id resolved from this app's own graph, never user-typed -
/// exactly like [`crate::rewrite::cherry_pick`]/[`crate::rewrite::revert`]/
/// [`crate::rewrite::rebase_onto`]'s own commit arguments - so no `--` terminator is needed to
/// guard against a flag-shaped value.
///
/// Plain `git checkout` already refuses on its own, with its own real, unmodified error, if
/// switching would silently overwrite modified tracked files; this makes no attempt to pre-check
/// or second-guess that itself - the worktree is left exactly where the equivalent command-line
/// invocation would leave it, for the user to resolve.
///
/// Performs blocking I/O.
pub fn checkout(worktree_path: &Path, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["checkout".into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real `git checkout -b <name> <commit>` for `worktree_path`: creates a new branch named `name`
/// at `commit` and switches to it in one atomic step - the row menu's "Create branch here".
///
/// `name` is genuinely user-typed (the row menu's own inline branch-name prompt), unlike almost
/// every other string this crate shells out with. It still needs no `--` terminator, unlike
/// `crate::add_worktree`'s own positional `worktree_path`: `-b`'s argument is always consumed as
/// `-b`'s own option-value by git's argument parser, never re-parsed as a flag regardless of its
/// content - confirmed empirically (`git checkout -b --evil <commit>` reports `fatal: '--evil'
/// is not a valid branch name`, not an unknown-option error) - so there is no positional slot
/// here for a `--` terminator to protect. A name colliding with an existing branch surfaces as
/// git's own real [`Error::GitCommand`] (`fatal: a branch named '<name>' already exists`), not
/// hand-rolled collision detection.
///
/// Performs blocking I/O.
pub fn create_branch_at(worktree_path: &Path, name: &str, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["checkout".into(), "-b".into(), name.into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// The three real `git reset` modes the row menu's "Reset" section offers - see [`reset`]'s own
/// docs for what each really does to the working tree/index/branch tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }
}

/// Real `git reset --soft|--mixed|--hard <commit>` for `worktree_path`: moves the current
/// branch's tip to `commit` - the row menu's "Reset" section.
///
/// - [`ResetMode::Soft`] leaves the index and working tree exactly as they were: every
///   difference between the old tip and `commit` ends up staged.
/// - [`ResetMode::Mixed`] (git's own default) resets the index to match `commit` but leaves the
///   working tree untouched: those differences end up unstaged instead.
/// - [`ResetMode::Hard`] resets both the index *and* the working tree to `commit`, discarding any
///   uncommitted changes outright - genuinely destructive, which is why the row menu only ever
///   reaches this for `Hard` after its own two-click confirmation
///   (`GraphTabState::hard_reset_confirm_armed`), never on a first click.
///
/// `commit` is always a real object id resolved from this app's own graph, never user-typed -
/// exactly like [`checkout`] above - so no `--` terminator is needed.
///
/// Performs blocking I/O.
pub fn reset(worktree_path: &Path, mode: ResetMode, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["reset".into(), mode.flag().into(), commit.into()];
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

    fn head_sha(dir: &Path) -> String {
        git_output(dir, &["rev-parse", "HEAD"])
    }

    fn current_branch(dir: &Path) -> String {
        git_output(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    #[test]
    fn checkout_really_moves_head_to_the_target_commit_detached() {
        let repo = init_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        let tip = commit(repo.path(), "a.txt", "changed", "second commit");
        assert_eq!(head_sha(repo.path()), tip);

        checkout(repo.path(), &base).expect("checkout should succeed");

        assert_eq!(
            head_sha(repo.path()),
            base,
            "HEAD must really point at the target commit"
        );
        assert_eq!(
            current_branch(repo.path()),
            "HEAD",
            "checking out a bare commit must really detach HEAD, not stay on the branch"
        );
    }

    #[test]
    fn checkout_on_a_dirty_worktree_with_a_real_conflict_surfaces_gits_own_refusal() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["checkout", "-b", "other"]);
        commit(repo.path(), "a.txt", "other branch content", "other change");
        git(repo.path(), &["checkout", "main"]);
        // An uncommitted change to the same file that genuinely conflicts with what "other"
        // holds - real git refuses to silently clobber it.
        fs::write(repo.path().join("a.txt"), "uncommitted dirty content").expect("write");

        let result = checkout(repo.path(), "other");
        assert!(
            result.is_err(),
            "a genuinely conflicting checkout over dirty tracked files must fail"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.to_lowercase().contains("overwritten")
                        || stderr.to_lowercase().contains("checkout"),
                    "git's own real refusal reason must be preserved: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        assert_eq!(
            current_branch(repo.path()),
            "main",
            "a refused checkout must leave the worktree exactly where it was"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "uncommitted dirty content",
            "the refused checkout must not have touched the dirty file"
        );
    }

    #[test]
    fn create_branch_at_really_creates_the_branch_at_the_commit_and_switches_to_it() {
        let repo = init_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        commit(repo.path(), "a.txt", "changed again", "second commit");

        create_branch_at(repo.path(), "feature-x", &base).expect("create_branch_at should succeed");

        assert_eq!(
            current_branch(repo.path()),
            "feature-x",
            "must really switch onto the newly created branch"
        );
        assert_eq!(
            head_sha(repo.path()),
            base,
            "the new branch must really be rooted at the given commit"
        );
        let branches = git_output(repo.path(), &["branch", "--list", "feature-x"]);
        assert!(
            branches.contains("feature-x"),
            "the branch must really exist in the repository's refs"
        );
    }

    #[test]
    fn create_branch_at_with_a_colliding_name_surfaces_gits_own_real_error() {
        let repo = init_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["branch", "existing-branch"]);

        let result = create_branch_at(repo.path(), "existing-branch", &base);
        assert!(
            result.is_err(),
            "creating a branch with a name that already exists must fail"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.contains("already exists"),
                    "git's own real collision error must be preserved: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
    }

    #[test]
    fn reset_soft_keeps_the_index_and_working_tree_and_only_moves_the_branch_tip() {
        let repo = init_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        commit(repo.path(), "a.txt", "changed", "second commit");

        reset(repo.path(), ResetMode::Soft, &base).expect("soft reset should succeed");

        assert_eq!(
            head_sha(repo.path()),
            base,
            "the branch tip must really move to the target commit"
        );
        let staged = git_output(repo.path(), &["diff", "--cached", "--name-only"]);
        assert_eq!(
            staged, "a.txt",
            "the undone commit's own change must land staged in the index"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "changed",
            "the working tree content must be untouched by a soft reset"
        );
    }

    #[test]
    fn reset_mixed_unstages_but_keeps_the_working_tree_content() {
        let repo = init_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        commit(repo.path(), "a.txt", "changed", "second commit");

        reset(repo.path(), ResetMode::Mixed, &base).expect("mixed reset should succeed");

        assert_eq!(head_sha(repo.path()), base);
        let staged = git_output(repo.path(), &["diff", "--cached", "--name-only"]);
        assert!(
            staged.is_empty(),
            "a mixed reset must leave nothing staged, got: {staged:?}"
        );
        let unstaged = git_output(repo.path(), &["diff", "--name-only"]);
        assert_eq!(
            unstaged, "a.txt",
            "the undone commit's own change must land unstaged instead"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "changed",
            "the working tree content must be untouched by a mixed reset"
        );
    }

    #[test]
    fn reset_hard_discards_the_working_tree_change_entirely() {
        let repo = init_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        commit(repo.path(), "a.txt", "changed", "second commit");

        reset(repo.path(), ResetMode::Hard, &base).expect("hard reset should succeed");

        assert_eq!(head_sha(repo.path()), base);
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "base",
            "a hard reset must really restore the working tree to the target commit's content"
        );
        let status = git_output(repo.path(), &["status", "--porcelain"]);
        assert!(
            status.is_empty(),
            "a hard reset must leave a genuinely clean worktree, got: {status:?}"
        );
    }
}
