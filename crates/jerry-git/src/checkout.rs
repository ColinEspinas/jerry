//! `HEAD` and branch-pointer moves that are not history rewrites: checking out, creating and
//! renaming branches, and resetting a branch tip.
//!
//! Separate from [`crate::rewrite`], whose operations all create or replay a commit; nothing here
//! changes what any commit contains. Failures surface as git's own stderr, unvalidated.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{check_success, run_git};

/// Detaches `HEAD` onto `commit`, leaving the current branch pointer alone.
///
/// No `--` terminator: `commit` is always an object id resolved from the caller's own graph,
/// never user-typed. Use [`checkout_branch`] for anything that is.
pub fn checkout(worktree_path: &Path, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["checkout".into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Creates branch `name` at `commit` and switches to it.
///
/// `name` is user-typed but needs no `--`: it lands in `-b`'s option-value slot, which git never
/// re-parses as a flag (`-b --evil` reports "not a valid branch name"). Collisions surface as
/// git's own error rather than being pre-checked.
pub fn create_branch_at(worktree_path: &Path, name: &str, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["checkout".into(), "-b".into(), name.into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Switches to an existing local branch with `HEAD` attached to it.
///
/// `switch --`, not [`checkout`], because `branch` comes from a branch listing rather than the
/// caller's graph: `git checkout --orphan` parses as the `--orphan` flag rather than as an
/// unknown branch. `switch` has no pathspec overload, so `--` keeps its plain end-of-options
/// meaning here - unlike `checkout -- <ref>`, which would look for a *file* by that name.
pub fn checkout_branch(worktree_path: &Path, branch: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["switch".into(), "--".into(), branch.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Renames a local branch, keeping its tip, reflog and upstream configuration.
///
/// The `--` is mandatory here, unlike in [`create_branch_at`]: these are ordinary positionals, and
/// `git branch -m feature --force` otherwise parses `--force` as `-M`, exits 0, and destroys both
/// refs by renaming the checked-out branch over `feature`. With `--` it is refused.
///
/// Renaming the checked-out branch is not a special case; git moves `HEAD` onto the new name.
pub fn rename_branch(worktree_path: &Path, old_name: &str, new_name: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec![
        "branch".into(),
        "-m".into(),
        "--".into(),
        old_name.into(),
        new_name.into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// `-d`, never `-D`: git refuses an unmerged branch, and one checked out in any worktree. Neither
/// refusal is pre-checked; both surface as git's own stderr.
///
/// Carries the same mandatory `--` as [`rename_branch`], for the same positional-parsing reason,
/// rather than depending on `name`'s provenance never changing.
pub fn delete_branch(worktree_path: &Path, name: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["branch".into(), "-d".into(), "--".into(), name.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

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

/// - [`ResetMode::Soft`] leaves index and working tree alone, so the difference ends up staged.
/// - [`ResetMode::Mixed`] resets the index only, so it ends up unstaged.
/// - [`ResetMode::Hard`] resets both, discarding uncommitted changes outright. Destructive;
///   confirming it is the caller's job.
///
/// No `--` terminator, for the same reason as [`checkout`]: `commit` is never user-typed.
pub fn reset(worktree_path: &Path, mode: ResetMode, commit: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["reset".into(), mode.flag().into(), commit.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use test_support::{git, git_output, seed_empty_repo};

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
        let repo = seed_empty_repo();
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
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["checkout", "-b", "other"]);
        commit(repo.path(), "a.txt", "other branch content", "other change");
        git(repo.path(), &["checkout", "main"]);
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
        let repo = seed_empty_repo();
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
        let repo = seed_empty_repo();
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
    fn checkout_branch_really_switches_with_head_attached_not_detached() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["checkout", "-b", "feature"]);
        commit(repo.path(), "a.txt", "on feature", "feature commit");
        git(repo.path(), &["checkout", "main"]);
        assert_eq!(current_branch(repo.path()), "main");

        checkout_branch(repo.path(), "feature").expect("checkout_branch should succeed");

        assert_eq!(
            current_branch(repo.path()),
            "feature",
            "HEAD must really be attached to the target branch, not detached"
        );
    }

    #[test]
    fn checkout_branch_refuses_a_nonexistent_branch_with_gits_own_real_error() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");

        let result = checkout_branch(repo.path(), "no-such-branch");
        assert!(result.is_err(), "a nonexistent branch must fail");
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.to_lowercase().contains("no-such-branch")
                        || stderr.to_lowercase().contains("invalid reference"),
                    "git's own real refusal must be preserved: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
    }

    #[test]
    fn checkout_branch_refuses_a_flag_shaped_name_instead_of_parsing_it_as_an_option() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");

        let result = checkout_branch(repo.path(), "--orphan");
        assert!(
            result.is_err(),
            "a flag-shaped name must be refused, not parsed as an option"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.contains("invalid reference") || stderr.contains("not found"),
                    "must be refused as an invalid ref, not misparsed as a flag \
                     (a real flag-parse failure reads like \"option 'orphan' requires a \
                     value\" instead): {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
    }

    #[test]
    fn rename_branch_really_moves_the_ref_to_the_new_name_and_leaves_no_old_one() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["branch", "old-name"]);

        rename_branch(repo.path(), "old-name", "new-name").expect("rename should succeed");

        assert_eq!(
            git_output(repo.path(), &["rev-parse", "new-name"]),
            base,
            "the renamed branch must still point at the very same commit"
        );
        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "new-name"),
            "the new name must really exist in the repository's refs: {branches:?}"
        );
        assert!(
            !branches.lines().any(|line| line == "old-name"),
            "the old name must genuinely be gone, not merely shadowed: {branches:?}"
        );
    }

    #[test]
    fn rename_branch_onto_an_existing_name_surfaces_gits_own_real_error() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["branch", "old-name"]);
        git(repo.path(), &["branch", "taken"]);

        let result = rename_branch(repo.path(), "old-name", "taken");
        assert!(
            result.is_err(),
            "renaming onto a name that already exists must fail"
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
        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "old-name"),
            "the refused rename must have left the original branch exactly where it was: \
             {branches:?}"
        );
    }

    #[test]
    fn renaming_the_currently_checked_out_branch_really_moves_head_onto_the_new_name() {
        let repo = seed_empty_repo();
        let tip = commit(repo.path(), "a.txt", "base", "base");
        assert_eq!(
            current_branch(repo.path()),
            "main",
            "premise: main really is the checked-out branch"
        );

        rename_branch(repo.path(), "main", "renamed-main").expect("rename should succeed");

        assert_eq!(
            current_branch(repo.path()),
            "renamed-main",
            "git's own default behaviour: HEAD must follow the branch it is on to its new name"
        );
        assert_eq!(
            head_sha(repo.path()),
            tip,
            "the working tree must still be sitting on the very same commit"
        );
    }

    #[test]
    fn rename_branch_refuses_a_flag_shaped_name_instead_of_destroying_two_refs() {
        let repo = seed_empty_repo();
        let main_tip = commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["branch", "feature"]);

        let result = rename_branch(repo.path(), "feature", "--force");
        assert!(
            result.is_err(),
            "a flag-shaped branch name must be refused, never parsed as an option"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.contains("not a valid branch name"),
                    "git's own real refusal must be what surfaces: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "main")
                && branches.lines().any(|line| line == "feature"),
            "both refs must survive untouched - the unguarded invocation destroyed both: \
             {branches:?}"
        );
        assert_eq!(
            current_branch(repo.path()),
            "main",
            "and the checked-out branch must not have been renamed out from under the worktree"
        );
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "feature"]),
            main_tip,
            "feature must still point where it did, not have been force-overwritten"
        );
    }

    #[test]
    fn delete_branch_treats_a_flag_shaped_name_as_a_branch_name_not_an_option() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["branch", "keepme"]);

        let result = delete_branch(repo.path(), "--force");
        assert!(result.is_err(), "there is no branch called `--force`");
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.contains("not found"),
                    "git must have looked for a *branch* by that name: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "keepme"),
            "no other branch may be deleted as a side effect: {branches:?}"
        );
    }

    #[test]
    fn delete_branch_really_removes_a_fully_merged_branch() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        // A branch at the current tip: fully merged by construction, which is exactly
        // what `git branch -d` is willing to remove.
        git(repo.path(), &["branch", "merged-branch"]);

        delete_branch(repo.path(), "merged-branch").expect("delete should succeed");

        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            !branches.lines().any(|line| line == "merged-branch"),
            "the branch must really be gone from the repository's refs: {branches:?}"
        );
    }

    #[test]
    fn delete_branch_refuses_an_unmerged_branch_with_gits_own_real_refusal() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");
        git(repo.path(), &["checkout", "-b", "unmerged"]);
        commit(repo.path(), "b.txt", "work", "work only on unmerged");
        git(repo.path(), &["checkout", "main"]);

        let result = delete_branch(repo.path(), "unmerged");
        assert!(
            result.is_err(),
            "the safe delete must refuse a branch carrying commits nothing else has"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.contains("not fully merged"),
                    "git's own real refusal reason must be preserved verbatim: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "unmerged"),
            "the refused delete must have left the branch (and its commits) alone: {branches:?}"
        );
    }

    #[test]
    fn delete_branch_refuses_the_branch_checked_out_in_this_worktree() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "base", "base");

        let result = delete_branch(repo.path(), "main");
        assert!(
            result.is_err(),
            "git itself refuses to delete the branch currently checked out here"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                // Lowercased on both sides: older git (2.39, still Debian bookworm's default)
                // capitalizes this message ("Cannot delete branch..."), newer git doesn't - a
                // wording-only difference across git's own versions, not something this test
                // should pin to one binary's exact casing.
                assert!(
                    stderr
                        .to_lowercase()
                        .contains("cannot delete branch 'main'"),
                    "git's own real refusal reason must be preserved verbatim: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        assert_eq!(
            current_branch(repo.path()),
            "main",
            "the refused delete must leave the worktree exactly where it was"
        );
    }

    #[test]
    fn reset_soft_keeps_the_index_and_working_tree_and_only_moves_the_branch_tip() {
        let repo = seed_empty_repo();
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
        let repo = seed_empty_repo();
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
        let repo = seed_empty_repo();
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
