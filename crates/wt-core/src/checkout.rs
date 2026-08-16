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

/// Real `git switch -- <branch>` for `worktree_path`: switches to an existing local branch with
/// `HEAD` attached to it - the Branches panel's own branch context menu "Checkout Branch"
/// (GitHub issue #241). Deliberately not [`checkout`]: that function's own docs establish it is
/// safe *only* because every existing caller passes a commit id resolved from this app's own
/// graph, never user-typed or taken from a branch listing - a guarantee this new caller (a real
/// branch name, sourced from the Branches panel's own list, not this app's graph) does not share.
///
/// The `--` here is not decorative, and plain `git checkout <branch>` (what [`checkout`] runs)
/// is not a substitute - live-reproduced: `git checkout --orphan` (no branch of that name exists
/// or ever could, since git itself refuses to create one starting with `-`) is parsed as the
/// real `--orphan` *flag*, not refused as an unknown branch, because `checkout`'s argument
/// parser inspects a leading positional for flag-shaped text before it is ever resolved as a
/// ref. `git switch` has no pathspec overload the way `checkout` does, so `--` here keeps its
/// ordinary "end of options" meaning without changing what gets checked out (unlike `checkout --
/// <ref>`, which would instead try to check out `<ref>` as a *file path*): `git switch --
/// --orphan` is refused honestly (`fatal: invalid reference: --orphan`), and `git switch --
/// <real-branch>` switches to it exactly like a bare `git switch <real-branch>` would.
///
/// A branch that doesn't exist, or a real failure switching (uncommitted changes that would be
/// overwritten), surfaces as git's own real stderr through [`Error::GitCommand`] - the same
/// no-pre-checking discipline every mutation in this module follows.
///
/// Performs blocking I/O.
pub fn checkout_branch(worktree_path: &Path, branch: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["switch".into(), "--".into(), branch.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real `git branch -m <old_name> <new_name>` for `worktree_path`: renames an existing local
/// branch, keeping its tip commit, its reflog and its upstream configuration - the Branches
/// panel's own branch context menu "Rename Branch…" (GitHub issue #241).
///
/// `new_name` is genuinely user-typed (the same hand-rolled prompt [`create_branch_at`] uses), so
/// the `--` terminator here is **mandatory**, not decorative - unlike [`create_branch_at`], whose
/// name lands in `-b`'s own option-value slot and so can never be re-parsed as a flag. `git branch
/// -m`'s arguments are ordinary positionals, and git's `parse-options` really does consume a
/// flag-shaped one as an option: live-reproduced on git 2.43 against a real repository, `git
/// branch -m feature --force` exits **0** having parsed `--force` as `-M`, renaming the
/// *currently checked-out* branch on top of `feature` and destroying both refs - reported to the
/// caller as a successful rename. With `--` in front, that same invocation is refused honestly
/// (`fatal: '--force' is not a valid branch name`, exit 128).
///
/// Nothing else is pre-validated: a `new_name` that already exists (`fatal: a branch named
/// '<name>' already exists`) or is not a legal ref name surfaces as git's own real stderr through
/// [`Error::GitCommand`], exactly like [`create_branch_at`]'s own collision handling. Renaming the
/// branch that is currently checked out is *not* a special case either - git itself moves `HEAD`
/// onto the new name, which is the correct behaviour and is proven directly by this module's own
/// tests.
///
/// Performs blocking I/O.
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

/// Real `git branch -d <name>` for `worktree_path`: the **safe** delete - the Branches panel's
/// own branch context menu "Delete Branch…" (GitHub issue #241).
///
/// Deliberately `-d`, never `-D`: git itself refuses to delete a branch whose commits are not
/// already merged into its upstream or into `HEAD` (`error: the branch '<name>' is not fully
/// merged`), and refuses to delete a branch that is checked out in *this* or any other worktree
/// of the repository (`error: cannot delete branch '<name>' used by worktree at '<path>'`). Neither
/// refusal is pre-checked here - both surface as git's own real stderr via [`Error::GitCommand`],
/// matching every other mutation in this module. The UI layer's own two-click confirmation (see
/// `app::graph_view`'s `GraphTabState::delete_branch_confirm_armed`) is about the user's intent,
/// not about second-guessing git's own safety rules.
///
/// Carries the same mandatory `--` terminator [`rename_branch`] documents: `name` reaches here
/// from this app's own branch list rather than a text field, but it is the same ordinary
/// positional slot, and one `--` is cheaper than depending on that provenance never changing
/// (`git branch -d -- --evil` reports `error: branch '--evil' not found`, never an option parse).
///
/// Performs blocking I/O.
pub fn delete_branch(worktree_path: &Path, name: &str) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["branch".into(), "-d".into(), "--".into(), name.into()];
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

    /// The whole reason [`checkout_branch`] exists rather than reusing [`checkout`]: a
    /// flag-shaped string in this positional slot must be refused as an invalid reference, never
    /// silently parsed as an option to `git switch` itself. No branch actually named `--orphan`
    /// can exist (git refuses to create one), so this proves the refusal is the *safe* one
    /// (`fatal: invalid reference`) rather than [`checkout`]'s own real failure mode reproduced in
    /// this module's docs (`--orphan` consumed as a flag, `error: option 'orphan' requires a
    /// value`).
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

    /// A live-reproduced data-loss path this function's `--` terminator exists to close, not a
    /// hypothetical: on git 2.43, `git branch -m feature --force` (no terminator) exits **0**,
    /// having parsed `--force` as `-M` and renamed the *currently checked-out* branch on top of
    /// `feature` - destroying both refs while reporting success. The rename prompt's name is
    /// genuinely user-typed, so this is reachable by typing it.
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

    /// The same terminator, on the delete side - `name` comes from this app's own branch list
    /// today, so this pins the guard rather than a live bug.
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
        // A branch pointing at the current tip - fully merged by construction, which is exactly
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
                assert!(
                    stderr.contains("cannot delete branch 'main'"),
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
