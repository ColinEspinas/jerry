//! Real command-pattern undo/redo primitives for two worktree-level actions: "keep all
//! changes" ([`commit_all_changes`]) and "discard worktree" ([`discard_worktree`]).
//!
//! Both real mutations record enough state for a caller (`app::root::worktree_history`) to
//! genuinely undo them later - not a fake "undo" that only toggles UI state. Both undo paths
//! carry a **mandatory identity guard**: they refuse rather than proceed if the real git state
//! they'd be acting on has moved since the outcome/snapshot was recorded, the same
//! "measure, don't assume, and never silently clobber something newer" discipline this
//! project's other staleness guards use (`app::root::code_surface`'s diff-highlight cache,
//! `app::root::completions`'s popup, `app::root::merge_flow`'s save race) - applied here to real
//! git history and a real worktree's existence, instead of in-memory app state.
//!
//! ## "Keep all changes": `git reset --soft`, not `git revert`
//!
//! [`commit_all_changes`] stages and commits everything in a worktree. Undoing it
//! ([`undo_commit_all_changes`]) runs `git reset --soft <parent>` rather than `git revert`:
//! reset moves `HEAD` back and leaves the exact same working tree/index the worktree had right
//! before the commit (uncommitted again, matching "undo" intuition), where a revert would
//! instead leave the commit in history and add a new inverse commit on top - not what a user
//! clicking "undo" on "keep all changes" would expect. Redoing
//! ([`redo_commit_all_changes`]) is the same move in the other direction: nothing deletes the
//! original commit object (`git reset --soft` never touches the object database), so as long as
//! it hasn't been garbage-collected, moving `HEAD` back onto it is a real, well-defined
//! operation - verified empirically in this module's own tests (create, undo, redo, and check
//! the working tree ends up identical to before the undo).
//!
//! ## "Discard worktree": `git stash push --include-untracked`
//!
//! [`discard_worktree`] force-removes a worktree (`remove_worktree(force: true)`), which
//! otherwise has no recovery path at all - see `crate`'s own module docs. Before removing
//! anything, it snapshots real uncommitted/untracked content into a real stash.
//!
//! An earlier version of this function used the lower-level `git stash create` + `git stash
//! store` (create the commit object without touching the working tree, then separately record
//! it under `refs/stash`), reasoning that removal shouldn't have to touch the working tree
//! before the moment it's actually deleted. Live testing against a real repository (this
//! module's own test suite) immediately falsified a hidden assumption behind that plan: `git
//! stash create` silently **never** captures untracked files at all, with no flag that changes
//! that (`--include-untracked`/`-u` are accepted by `git stash push`, not `create`, and pass
//! straight through unrecognized) - a worktree with only a brand-new untracked file produced an
//! empty stash id every time. Since [`discard_worktree`] always force-removes the worktree
//! immediately afterward regardless of whether the working tree was already reset first, there
//! is no real safety this function was buying by avoiding `git stash push`: it's called with
//! `--include-untracked` instead, which real-repo testing confirms genuinely captures untracked
//! content, and its resulting stash commit's real id is read straight off the (repository-
//! shared, not worktree-private) `refs/stash` ref it just moved.
//!
//! `refs/stash` lives in the repository's shared ref store, not anything private to the
//! worktree that pushed it - verified empirically against a real repository in this module's
//! own tests: a stash pushed from a worktree survives that worktree's own removal and is still
//! `git stash apply`-able from a freshly recreated worktree on the same branch.
//!
//! ## Two real, live-reproduced gaps this module refuses/surfaces honestly rather than papering
//! over
//!
//! An audit of an earlier version of this module found both of these by direct, empirical
//! reproduction against a real repository - not guessed at:
//!
//! - **The main worktree can never be force-removed.** `git worktree remove --force --force`
//!   refuses outright on the repository's main working tree (`fatal: '<path>' is a main working
//!   tree`) - unlike a linked worktree, there is no `--force` that overrides this. An earlier
//!   version of [`discard_worktree`] stashed *first*, then attempted the removal - so on a main
//!   worktree, real uncommitted content got stashed (mutating the working tree), the removal
//!   then failed, and the function returned `Err` with **no [`DiscardSnapshot`] ever handed back
//!   to a caller to record for `Undo`** - a real, reachable, silent-data-loss path (this app's
//!   own default session's cwd *is* the repository's main worktree). [`discard_worktree`] now
//!   refuses upfront with [`Error::DiscardSourceIsMainWorktree`] before touching anything at
//!   all. For any *other* real reason `remove_worktree` might fail after a successful stash (a
//!   permissions error, a lock `--force --force` doesn't override on some git version, ...):
//!   trying to restore the stash back into the worktree directory in place was tried and found,
//!   by direct empirical reproduction, not to be reliable - `git worktree remove` typically
//!   deletes the worktree's own contents and fully deregisters it as a worktree *before* failing
//!   only at its very last step (removing the now-empty directory entry itself), so the
//!   directory may no longer even be a valid git worktree to restore anything into by the time
//!   the failure is observed. [`Error::DiscardRemovalFailedAfterStash`] surfaces the real stash
//!   id instead of pretending an in-place restore happened - the stash itself is still real,
//!   durable, and independently recoverable by hand (`git stash apply <stash>`/`git stash list`
//!   from any worktree of the repository) regardless of what state the directory itself is left
//!   in.
//! - **A stash never captures gitignored content**, even with `--include-untracked`: `.env`
//!   files, build output, anything else `.gitignore` excludes. [`is_dirty`](crate::is_dirty)
//!   (which gates whether a stash is even attempted) also doesn't count ignored-only content as
//!   "dirty" at all. Rather than silently claiming full safety, [`discard_worktree`] separately
//!   checks for real ignored content (`git status --porcelain --ignored`) and records it on
//!   [`DiscardSnapshot::had_ignored_content`], so a caller can tell the user honestly that some
//!   real content was *not* preserved - this module still does not capture it (a real
//!   `git stash push --all` would sweep up potentially huge build directories into a git object,
//!   a real cost/risk judged worse than the honest-degradation path this takes instead).
//!
//! Performs blocking I/O everywhere in this module (shells out to `git`); see the crate-level
//! docs on offloading this to a background thread.

use std::ffi::OsString;
use std::path::Path;

use crate::error::{Error, GitExit};
use crate::{
    add_worktree, check_success, describe_worktree, format_args, is_dirty, list_worktrees,
    open_repo, remove_worktree, run_git,
};

/// The real result of a successful [`commit_all_changes`] call - enough for
/// [`undo_commit_all_changes`]/[`redo_commit_all_changes`] to act on later, under their own
/// mandatory identity guards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAllChangesOutcome {
    /// The branch `HEAD` referred to at commit time, or `None` if the worktree was in a
    /// (unusual, but real) detached-`HEAD` state. Only consulted by
    /// [`undo_commit_all_changes`] in the (practically unreachable, but real) case where
    /// [`Self::parent`] is also `None` - see [`Error::CommitHasNoParentAndNoBranch`].
    pub branch: Option<String>,
    /// The real commit id [`commit_all_changes`] just created.
    pub commit: String,
    /// The commit id `HEAD` pointed at immediately before this commit, or `None` if this was
    /// the very first commit ever made on this branch (an "unborn" branch becoming born).
    pub parent: Option<String>,
}

/// Reads `worktree_path`'s current `HEAD` commit id via a real `git rev-parse HEAD`. `Err` if
/// `HEAD` doesn't resolve to a commit at all (an unborn branch, or not a repository).
fn rev_parse_head(worktree_path: &Path) -> Result<String, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "HEAD".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether `worktree_path`'s `HEAD` currently resolves to a real commit at all - the real
/// "is this branch still unborn" check `redo_commit_all_changes` needs for its own identity
/// guard when [`CommitAllChangesOutcome::parent`] is `None`.
fn head_resolves(worktree_path: &Path) -> bool {
    rev_parse_head(worktree_path).is_ok()
}

/// Reads `commit`'s real, immutable first-parent commit id (`git rev-parse --verify -q
/// <commit>^`), or `None` if `commit` genuinely has no parent (a true root commit - the very
/// first commit ever made on its branch).
///
/// Deliberately resolves `<commit>^`, not `HEAD^`: `HEAD` can move at any point after
/// `commit_all_changes` creates its commit (this app's whole domain is running agent CLIs
/// inside these worktrees, and an agent process committing on top is realistic, not exotic), but
/// a specific commit object's own parent is immutable the instant it's created - asking "what is
/// `commit`'s parent" is always correct regardless of what `HEAD` does afterward, where asking
/// "what is `HEAD`'s parent" would silently answer a different question if `HEAD` has already
/// moved past `commit` by the time this runs.
fn rev_parse_parent_of(worktree_path: &Path, commit: &str) -> Result<Option<String>, Error> {
    let args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--verify".into(),
        "-q".into(),
        format!("{commit}^").into(),
    ];
    let output = run_git(worktree_path, &args)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Reads `worktree_path`'s currently checked-out branch (`git symbolic-ref --quiet --short
/// HEAD`), or `None` if `HEAD` is detached.
fn current_branch(worktree_path: &Path) -> Result<Option<String>, Error> {
    let args: Vec<OsString> = vec![
        "symbolic-ref".into(),
        "--quiet".into(),
        "--short".into(),
        "HEAD".into(),
    ];
    let output = run_git(worktree_path, &args)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Stage every change in `worktree_path` (`git add -A`: modified, deleted, and untracked files
/// alike) and commit it with `message`.
///
/// Refuses with [`Error::NothingToCommit`] if the worktree has no uncommitted changes at all -
/// this crate's existing "check first, structured error" convention (see
/// [`crate::merge::attempt_merge`]'s own `MergeTargetDirty` pre-check) rather than parsing
/// `git commit`'s "nothing to commit" stderr after the fact.
///
/// Performs blocking I/O.
pub fn commit_all_changes(
    worktree_path: &Path,
    message: &str,
) -> Result<CommitAllChangesOutcome, Error> {
    if !is_dirty(worktree_path)? {
        return Err(Error::NothingToCommit {
            path: worktree_path.to_path_buf(),
        });
    }

    let add_args: Vec<OsString> = vec!["add".into(), "-A".into()];
    let add_output = run_git(worktree_path, &add_args)?;
    check_success(&add_args, &add_output)?;

    let commit_args: Vec<OsString> = vec!["commit".into(), "-m".into(), message.into()];
    let commit_output = run_git(worktree_path, &commit_args)?;
    check_success(&commit_args, &commit_output)?;

    let commit = rev_parse_head(worktree_path)?;

    // Both derived from the real commit that was just made, not from a pre-commit read taken
    // before `add`/`commit` ran (live-reproduced as a real staleness bug: anything else that
    // commits in this worktree in that window - e.g. an agent CLI process, which this app runs
    // inside these very worktrees - would make a pre-commit-read `parent` point at the wrong,
    // stale end of the range, and `undo_commit_all_changes`'s `HEAD == outcome.commit` guard
    // would not catch it, since `HEAD` genuinely is `outcome.commit` - it would just be a soft
    // reset that silently discards the interleaved commit). See `rev_parse_parent_of`'s own docs
    // for why this asks about `commit`'s parent specifically, not `HEAD`'s.
    let parent = rev_parse_parent_of(worktree_path, &commit)?;
    let branch = current_branch(worktree_path)?;

    Ok(CommitAllChangesOutcome {
        branch,
        commit,
        parent,
    })
}

/// Stage exactly `paths` (`git add -- <paths>`, never `commit_all_changes`'s `-A`) and commit
/// them with `message` - the Changes panel commit composer's real backing (Revision R12 §5:
/// "Commit N files" commits only the staged subset, not the whole worktree diff).
///
/// Refuses with [`Error::NothingToCommit`] if `paths` is empty, the same "check first, structured
/// error" convention [`commit_all_changes`] follows for a clean worktree - the composer's own
/// primary button already disables itself with nothing staged (see
/// `crate::sidebar::changes::commit_button_label`), so this is a defensive backstop, not the
/// primary guard.
///
/// **The leading `git add -- <paths>` is a deliberate, harmless idempotent safety net, not dead
/// weight.** The Changes panel's staging checkbox (`crate::sidebar::render::AdeApp::
/// toggle_staged`, backed by [`crate::stage::stage_path`]/[`crate::stage::unstage_path`]) now
/// really stages/unstages `paths` in the real index the moment each box is clicked, so by the
/// time the composer's primary button calls this function every path it passes is normally
/// already staged - but "normally" isn't "always": a real per-path staging failure that got
/// silently reverted client-side (see `toggle_staged`'s own docs on that failure mode), or a
/// worktree-switch race where `AdeApp::staged_files` hasn't yet been re-derived from a fresh
/// `git diff --cached` when the commit fires, can both leave the app's own idea of "staged"
/// briefly out of sync with the real index. This function's contract is "stage exactly `paths`
/// and commit them" regardless of what state the index happens to already be in when it's
/// called, and re-running `git add` on a path that's already staged is a real no-op - so keeping
/// it here is what makes that contract hold unconditionally rather than only when the click-time
/// staging already succeeded.
///
/// Returns the same [`CommitAllChangesOutcome`] shape [`commit_all_changes`] does, but this
/// function has no `undo_commit_paths`/`redo_commit_paths` counterpart yet - a partial commit
/// isn't wired into [`crate::undo::UndoableAction`] (a real, honest gap, not a fake "undo" that
/// would only look like it worked).
///
/// Performs blocking I/O.
pub fn commit_paths(
    worktree_path: &Path,
    paths: &[std::path::PathBuf],
    message: &str,
) -> Result<CommitAllChangesOutcome, Error> {
    if paths.is_empty() {
        return Err(Error::NothingToCommit {
            path: worktree_path.to_path_buf(),
        });
    }

    let mut add_args: Vec<OsString> = vec!["add".into(), "--".into()];
    add_args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let add_output = run_git(worktree_path, &add_args)?;
    check_success(&add_args, &add_output)?;

    // Pathspec-limited (`-- <paths>`), never a bare `git commit`: a bare `git commit` commits
    // the *entire* index, not just what this call just `add`ed - anything else already staged
    // (an agent CLI running its own `git add` in this same worktree, the same interleaving
    // hazard `commit_all_changes`'s own doc above calls out) would silently ride along into a
    // commit this function's own doc promises is limited to `paths`. Real regression:
    // `commit_paths_never_commits_a_path_that_was_staged_by_something_else`.
    let mut commit_args: Vec<OsString> =
        vec!["commit".into(), "-m".into(), message.into(), "--".into()];
    commit_args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let commit_output = run_git(worktree_path, &commit_args)?;
    check_success(&commit_args, &commit_output)?;

    let commit = rev_parse_head(worktree_path)?;
    let parent = rev_parse_parent_of(worktree_path, &commit)?;
    let branch = current_branch(worktree_path)?;

    Ok(CommitAllChangesOutcome {
        branch,
        commit,
        parent,
    })
}

/// Undo a [`commit_all_changes`] call: real `git reset --soft <parent>`, returning the worktree
/// to exactly the uncommitted state it was in right before that commit - see this module's own
/// docs for why `reset --soft`, not `revert`.
///
/// Mandatory identity guard: refuses with [`Error::HeadMovedSinceRecorded`] unless
/// `outcome.commit` is still genuinely `HEAD` right now - otherwise this would silently discard
/// whatever was committed on top since.
///
/// Performs blocking I/O.
pub fn undo_commit_all_changes(
    worktree_path: &Path,
    outcome: &CommitAllChangesOutcome,
) -> Result<(), Error> {
    let current = rev_parse_head(worktree_path)?;
    if current != outcome.commit {
        return Err(Error::HeadMovedSinceRecorded {
            path: worktree_path.to_path_buf(),
            expected: outcome.commit.clone(),
            actual: current,
        });
    }

    match &outcome.parent {
        Some(parent) => {
            let args: Vec<OsString> = vec!["reset".into(), "--soft".into(), parent.clone().into()];
            let output = run_git(worktree_path, &args)?;
            check_success(&args, &output)
        }
        None => {
            // This was the first commit ever on this branch: there is no parent commit to
            // reset to. `git update-ref -d <ref>` removes the branch ref itself while leaving
            // the index/working tree untouched - the real "soft reset to before this branch
            // existed" equivalent.
            let Some(branch) = &outcome.branch else {
                return Err(Error::CommitHasNoParentAndNoBranch {
                    path: worktree_path.to_path_buf(),
                });
            };
            let ref_name = format!("refs/heads/{branch}");
            let args: Vec<OsString> = vec!["update-ref".into(), "-d".into(), ref_name.into()];
            let output = run_git(worktree_path, &args)?;
            check_success(&args, &output)
        }
    }
}

/// Redo a [`commit_all_changes`] call previously undone by [`undo_commit_all_changes`]: moves
/// `HEAD` forward onto `outcome.commit` again. Safe as long as that commit object is still in
/// the repository's object database - `reset --soft` never deletes it, only
/// [`undo_commit_all_changes`] itself moved away from it, so nothing should have collected it in
/// the interim; if something has (e.g. an external `git gc`), this fails with a real
/// [`Error::GitCommand`] from the underlying `git reset` rather than silently no-op'ing.
///
/// Mandatory identity guard, symmetric with [`undo_commit_all_changes`]'s own: refuses with
/// [`Error::HeadMovedSinceRecorded`] unless `HEAD` is still genuinely sitting exactly where the
/// undo left it (`outcome.parent`, or a genuinely unborn branch if `outcome.parent` is `None`) -
/// a redo can silently discard newer work just as easily as an undo can.
///
/// Performs blocking I/O.
pub fn redo_commit_all_changes(
    worktree_path: &Path,
    outcome: &CommitAllChangesOutcome,
) -> Result<(), Error> {
    match &outcome.parent {
        Some(parent) => {
            let current = rev_parse_head(worktree_path)?;
            if &current != parent {
                return Err(Error::HeadMovedSinceRecorded {
                    path: worktree_path.to_path_buf(),
                    expected: parent.clone(),
                    actual: current,
                });
            }
            let args: Vec<OsString> = vec![
                "reset".into(),
                "--soft".into(),
                outcome.commit.clone().into(),
            ];
            let output = run_git(worktree_path, &args)?;
            check_success(&args, &output)
        }
        None => {
            // The undo un-made the branch ref entirely (see `undo_commit_all_changes`'s `None`
            // arm) - `HEAD` must still be genuinely unborn (`rev_parse_head` fails) for the
            // guard to hold; anything else (including an unrelated real error resolving `HEAD`)
            // is refused the same honest way.
            if head_resolves(worktree_path) {
                return Err(Error::HeadMovedSinceRecorded {
                    path: worktree_path.to_path_buf(),
                    expected: "no commit yet (unborn branch)".to_string(),
                    actual: "HEAD now resolves to a real commit".to_string(),
                });
            }
            let Some(branch) = &outcome.branch else {
                return Err(Error::CommitHasNoParentAndNoBranch {
                    path: worktree_path.to_path_buf(),
                });
            };
            // `git update-ref` creates a ref that doesn't exist yet just as readily as it moves
            // one that does - the real reverse of `undo_commit_all_changes`'s `update-ref -d`.
            let ref_name = format!("refs/heads/{branch}");
            let args: Vec<OsString> = vec![
                "update-ref".into(),
                ref_name.into(),
                outcome.commit.clone().into(),
            ];
            let output = run_git(worktree_path, &args)?;
            check_success(&args, &output)
        }
    }
}

/// A real snapshot of a worktree taken immediately before [`discard_worktree`] force-removes
/// it - enough to recreate the worktree and (if there was anything to restore) its
/// uncommitted/untracked content via [`undo_discard_worktree`], under that function's own
/// mandatory identity guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardSnapshot {
    /// The branch checked out at discard time, or `None` for an (unusual, but real)
    /// detached-`HEAD` worktree - [`undo_discard_worktree`] falls back to recreating the
    /// worktree directly at [`Self::commit`] (detached) in that case.
    pub branch: Option<String>,
    /// `HEAD`'s commit id at discard time.
    pub commit: String,
    /// `Some` iff the worktree had real uncommitted/untracked content worth preserving - the
    /// real `git stash push --include-untracked` commit id for it.
    pub stash: Option<String>,
    /// Whether real gitignored content (`git status --porcelain --ignored`) was present at
    /// discard time - `git stash push`, even with `--include-untracked`, never captures it, so
    /// `true` here means [`Self::stash`] (if any) does *not* fully account for everything that
    /// was really in the worktree. See this module's own top-level docs for why this is
    /// surfaced honestly rather than either silently dropped or captured via a real
    /// `git stash push --all` (a real cost/risk this module judged worse).
    pub had_ignored_content: bool,
}

/// What restoring from a [`DiscardSnapshot`] actually achieved. [`UndoDiscardOutcome::Restored`]
/// is not the only success case: a stored stash can (rarely) conflict when applied back onto
/// the recreated worktree, and callers must not claim "fully restored" for
/// [`UndoDiscardOutcome::RestoredWithConflicts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoDiscardOutcome {
    /// The worktree was recreated, and (if there was one) the stash applied with no conflicts.
    Restored,
    /// The worktree was recreated, but applying the stored stash left real conflict markers in
    /// one or more files. The stash entry (`stash`) is deliberately *not* dropped in this case -
    /// [`undo_discard_worktree`] always uses `git stash apply`, never `pop`, specifically so the
    /// stash survives as a fallback regardless of whether the apply itself was clean.
    RestoredWithConflicts { stash: String },
}

/// Force-remove the worktree at `worktree_path`, first taking a real [`DiscardSnapshot`] of it -
/// unlike a bare `remove_worktree(force: true)` call, which permanently destroys
/// uncommitted/untracked content with no recovery path at all.
///
/// Refuses with [`Error::DiscardSourceIsMainWorktree`] if `worktree_path` is the repository's
/// main worktree - `git worktree remove --force --force` can never remove it (git itself always
/// refuses), so proceeding would stash real content, fail to remove anything, and leave that
/// content stashed with no [`DiscardSnapshot`] ever returned to record it for `Undo` - a real,
/// reachable data-loss path this refuses upfront instead (see this module's own top-level docs).
///
/// Refuses with [`Error::DiscardSourceUnborn`] if the worktree has no commits yet at all (`git
/// stash` itself has nothing to diff against in that case). If the worktree is clean
/// ([`crate::is_dirty`] is `false`), no stash is created - there is nothing to lose, so
/// [`DiscardSnapshot::stash`] is `None`. If the worktree is dirty but `git stash push` itself
/// fails or produces no real output, this refuses with [`Error::DiscardSnapshotFailed`] rather
/// than forcing the removal through uncaptured. [`DiscardSnapshot::had_ignored_content`] is
/// always populated, independent of whether a stash was taken (see its own docs).
///
/// If a stash was taken but the removal itself then fails for any other real reason, this
/// returns [`Error::DiscardRemovalFailedAfterStash`] (see its own docs and this module's
/// top-level docs for why an in-place restore isn't attempted).
///
/// Performs blocking I/O.
pub fn discard_worktree(repo_path: &Path, worktree_path: &Path) -> Result<DiscardSnapshot, Error> {
    let repo = open_repo(worktree_path)?;
    let before = describe_worktree(&repo, worktree_path.to_path_buf(), false, false, None)?;
    drop(repo);
    let Some(commit) = before.head_commit.clone() else {
        return Err(Error::DiscardSourceUnborn {
            path: worktree_path.to_path_buf(),
        });
    };

    if is_main_worktree(worktree_path)? {
        return Err(Error::DiscardSourceIsMainWorktree {
            path: worktree_path.to_path_buf(),
        });
    }

    let had_ignored_content = has_ignored_content(worktree_path)?;

    let stash = if is_dirty(worktree_path)? {
        Some(push_and_capture_stash(worktree_path)?)
    } else {
        None
    };

    if let Err(err) = remove_worktree(repo_path, worktree_path, true) {
        // Live-reproduced: attempting to restore the stash back into `worktree_path` in place
        // here is not reliable - `git worktree remove` typically deletes the worktree's own
        // contents and deregisters it *before* failing only at the final, now-empty directory
        // entry's own removal, so `worktree_path` may no longer even be a valid git worktree by
        // this point. The stash is still real and durable regardless (`refs/stash`, never
        // dropped), so this surfaces its id rather than pretending an in-place restore happened.
        return Err(match stash {
            Some(stash_id) => Error::DiscardRemovalFailedAfterStash {
                path: worktree_path.to_path_buf(),
                stash: stash_id,
                source: Box::new(err),
            },
            None => err,
        });
    }

    Ok(DiscardSnapshot {
        branch: before.branch,
        commit,
        stash,
        had_ignored_content,
    })
}

/// Whether `worktree_path` is the repository's main worktree (the one `git init`/`git clone`
/// creates, as opposed to a linked one from `git worktree add`) - reuses the exact same
/// `gix::Repository::main_repo`/`work_dir` real-worktree-location lookup [`crate::list_worktrees`]
/// itself uses for the same determination, rather than a second, independent way of answering
/// the same question. Canonicalizes both sides before comparing so a relative or symlinked
/// `worktree_path` still matches correctly.
fn is_main_worktree(worktree_path: &Path) -> Result<bool, Error> {
    let repo = open_repo(worktree_path)?;
    let main_repo = repo.main_repo().map_err(|source| Error::Open {
        path: worktree_path.to_path_buf(),
        source: Box::new(source),
    })?;
    let Some(main_path) = main_repo.work_dir() else {
        // A bare repository has no main worktree at all - see `crate::list_worktrees`'s own
        // docs for the same fact.
        return Ok(false);
    };
    let main_canon = std::fs::canonicalize(main_path).unwrap_or_else(|_| main_path.to_path_buf());
    let target_canon =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    Ok(main_canon == target_canon)
}

/// Whether `worktree_dir` contains any real gitignored content right now (`git status
/// --porcelain --ignored`, filtering to the `!!`-prefixed ignored entries specifically, so
/// ordinary tracked/untracked changes - already covered by [`crate::is_dirty`]/the real stash -
/// never false-positive this).
fn has_ignored_content(worktree_dir: &Path) -> Result<bool, Error> {
    let args: Vec<OsString> = vec!["status".into(), "--porcelain".into(), "--ignored".into()];
    let output = run_git(worktree_dir, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with("!!")))
}

/// Reads `refs/stash`'s current commit id, or `None` if it doesn't resolve at all (no stash has
/// ever been pushed in this repository yet) - `git rev-parse --verify -q` exits non-zero for
/// exactly that "doesn't exist yet" case, which this tolerates rather than treating as an error.
fn read_stash_ref(worktree_path: &Path) -> Result<Option<String>, Error> {
    let rev_args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--verify".into(),
        "-q".into(),
        "refs/stash".into(),
    ];
    let rev_output = run_git(worktree_path, &rev_args)?;
    if !rev_output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&rev_output.stdout)
            .trim()
            .to_string(),
    ))
}

/// The real `git stash push --include-untracked` this module's own docs describe - factored out
/// so [`discard_worktree`] reads as the real, linear "snapshot, then remove" it is.
///
/// Live-reproduced (an earlier version of this function trusted `refs/stash` unconditionally
/// after the push, and was falsified by direct testing): `git stash push` can exit `0` and print
/// "No local changes to save" **without pushing anything at all**, even when
/// [`crate::is_dirty`] correctly reports the worktree as dirty - a dirty submodule pointer
/// (`M <submodule>` in `git status --porcelain`) is real, uncommitted state that `git stash
/// push` cannot capture at all, with no flag that changes that. When this happens, `refs/stash`
/// is left pointing at whatever it already pointed at before this call (nothing ever drops old
/// stash entries), which can easily be a completely unrelated stash from a prior, unrelated
/// operation in this same shared repository. Trusting that value as "the stash for *this*
/// snapshot" would hand back a real but wrong sha - [`discard_worktree`] would then force-remove
/// the real worktree believing its content was captured, and a later undo would restore the
/// *wrong* content while claiming success. To catch this, `refs/stash` is read (via
/// [`read_stash_ref`]) both **before** and **after** the push; the post-push value is only
/// trusted as this snapshot's stash if it both resolves to something real *and* differs from the
/// pre-push value - otherwise this returns [`Error::DiscardSnapshotFailed`] instead, so the
/// caller never proceeds to remove anything.
fn push_and_capture_stash(worktree_path: &Path) -> Result<String, Error> {
    let before_stash = read_stash_ref(worktree_path)?;

    let push_args: Vec<OsString> = vec![
        "stash".into(),
        "push".into(),
        "--include-untracked".into(),
        "-m".into(),
        "ade: discard-worktree snapshot".into(),
    ];
    let push_output = run_git(worktree_path, &push_args)?;
    check_success(&push_args, &push_output)?;

    // `refs/stash` should now point at the stash `push` just created (`stash@{0}`) - read its
    // real id directly rather than relying on the relative `stash@{0}` name, which would
    // silently shift if anything else pushed another stash before this snapshot is ever undone.
    // But a successful exit alone doesn't prove anything was actually pushed (see this
    // function's own docs) - only accept it if it's both present and genuinely new.
    let after_stash = read_stash_ref(worktree_path)?;
    match after_stash {
        Some(after) if Some(&after) != before_stash.as_ref() => Ok(after),
        _ => Err(Error::DiscardSnapshotFailed {
            path: worktree_path.to_path_buf(),
        }),
    }
}

/// Undo a [`discard_worktree`] call: recreate the worktree at its original path on its original
/// branch/commit, then (if the snapshot captured one) restore the stash on top.
///
/// Mandatory identity guard, checked before touching anything: refuses with
/// [`Error::DiscardWorktreePathReoccupied`] if a worktree (or any other directory) already
/// occupies `worktree_path`, or with [`Error::DiscardBranchMovedOrReoccupied`] if the recorded
/// branch no longer exists, is checked out in a different worktree already, or its tip commit is
/// no longer `snapshot.commit` - real evidence something else touched it since the discard. Any
/// of these means blindly recreating would either silently clobber whatever now occupies that
/// name, or resurrect stale content on top of real newer work - both refused honestly rather
/// than forced through.
///
/// Performs blocking I/O.
pub fn undo_discard_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    snapshot: &DiscardSnapshot,
) -> Result<UndoDiscardOutcome, Error> {
    if worktree_path.exists() {
        return Err(Error::DiscardWorktreePathReoccupied {
            path: worktree_path.to_path_buf(),
        });
    }

    let existing = list_worktrees(repo_path)?;
    for entry in existing.into_iter().flatten() {
        if entry.path == worktree_path {
            return Err(Error::DiscardWorktreePathReoccupied {
                path: worktree_path.to_path_buf(),
            });
        }
        if let Some(branch) = &snapshot.branch {
            if entry.branch.as_deref() == Some(branch.as_str()) {
                return Err(Error::DiscardBranchMovedOrReoccupied {
                    branch: branch.clone(),
                });
            }
        }
    }

    if let Some(branch) = &snapshot.branch {
        let ref_name = format!("refs/heads/{branch}");
        let args: Vec<OsString> = vec![
            "rev-parse".into(),
            "--verify".into(),
            "-q".into(),
            ref_name.into(),
        ];
        let output = run_git(repo_path, &args)?;
        let tip = if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        };
        if tip.as_deref() != Some(snapshot.commit.as_str()) {
            return Err(Error::DiscardBranchMovedOrReoccupied {
                branch: branch.clone(),
            });
        }
        add_worktree(repo_path, worktree_path, None, Some(branch))?;
    } else {
        add_worktree(repo_path, worktree_path, None, Some(&snapshot.commit))?;
    }

    let Some(stash) = &snapshot.stash else {
        return Ok(UndoDiscardOutcome::Restored);
    };

    apply_stash(worktree_path, stash)
}

/// Runs `git stash apply <stash>` (never `pop` - see this module's own docs on
/// [`UndoDiscardOutcome::RestoredWithConflicts`] for why) and classifies the real outcome:
/// [`UndoDiscardOutcome::Restored`] on a clean apply, or
/// [`UndoDiscardOutcome::RestoredWithConflicts`] - not an [`Err`] - if `git` reports real
/// conflict markers instead.
///
/// A non-zero exit from `git stash apply` is genuinely ambiguous: git uses it both for a real
/// merge conflict (a real partial success - most content did land, just with `<<<<<<<` markers
/// in some files) *and* for a genuine failure with nothing restored at all (e.g. `stash` no
/// longer resolves to anything real - live-reproducible by running `git stash drop`/`clear` in a
/// terminal between [`discard_worktree`] and [`undo_discard_worktree`], which this app's own
/// real terminal sessions make entirely possible). Collapsing both into
/// `RestoredWithConflicts` would be a real, false "something was restored" claim for the second
/// case. `git diff --name-only --diff-filter=U` is git's own real, authoritative "is there an
/// actual unresolved-merge conflict right now" signal, so it disambiguates the two: a non-empty
/// result really did land conflicting content (this deliberately doesn't route that case through
/// [`check_success`]/[`Error::GitCommand`], since it's git's well-known, non-exceptional way of
/// reporting a conflict, not a failure to run the command); an empty result means nothing real
/// landed at all, surfaced as a real [`Error::GitCommand`] instead.
fn apply_stash(worktree_path: &Path, stash: &str) -> Result<UndoDiscardOutcome, Error> {
    let apply_args: Vec<OsString> = vec!["stash".into(), "apply".into(), stash.into()];
    let apply_output = run_git(worktree_path, &apply_args)?;
    if apply_output.status.success() {
        return Ok(UndoDiscardOutcome::Restored);
    }

    let unmerged_args: Vec<OsString> = vec![
        "diff".into(),
        "--name-only".into(),
        "--diff-filter=U".into(),
    ];
    let unmerged_output = run_git(worktree_path, &unmerged_args)?;
    let has_real_conflict = unmerged_output.status.success() && !unmerged_output.stdout.is_empty();

    if has_real_conflict {
        Ok(UndoDiscardOutcome::RestoredWithConflicts {
            stash: stash.to_string(),
        })
    } else {
        Err(Error::GitCommand {
            args: format_args(&apply_args),
            exit: GitExit::from_status(&apply_output.status),
            stderr: String::from_utf8_lossy(&apply_output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
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

    fn init_repo_at(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        fs::write(dir.join("file.txt"), "hello\n").expect("write file");
        git(dir, &["add", "file.txt"]);
        git(dir, &["commit", "-m", "initial commit"]);
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        init_repo_at(dir.path());
        dir
    }

    /// A session worktree on branch `name`, checked out from `main`'s current tip - mirrors how
    /// `app::root` actually creates session worktrees (`add_worktree` with `-b`).
    fn add_session_worktree(repo: &Path, path: &Path, branch: &str) {
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
    }

    // --- commit_all_changes / undo / redo -----------------------------------------------

    #[test]
    fn commit_all_changes_stages_and_commits_modified_new_and_deleted_files() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("new.txt"), "new\n").expect("new file");
        // A second tracked file, committed first so it has real history, then deleted - covers
        // the "deleted tracked file" case in the same `commit_all_changes` call.
        fs::write(repo.path().join("to-delete.txt"), "bye\n").expect("write");
        git(repo.path(), &["add", "to-delete.txt"]);
        git(repo.path(), &["commit", "-m", "add to-delete.txt"]);
        fs::remove_file(repo.path().join("to-delete.txt")).expect("delete");

        let before_head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");

        assert_eq!(outcome.parent.as_deref(), Some(before_head.as_str()));
        assert_eq!(outcome.branch.as_deref(), Some("main"));
        assert_eq!(
            outcome.commit,
            git_output(repo.path(), &["rev-parse", "HEAD"])
        );
        assert!(!is_dirty(repo.path()).expect("is_dirty"));
        assert!(!repo.path().join("to-delete.txt").exists());
        assert_eq!(
            fs::read_to_string(repo.path().join("new.txt")).expect("read new.txt"),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read file.txt"),
            "changed\n"
        );
    }

    #[test]
    fn commit_all_changes_refuses_on_a_clean_worktree() {
        let repo = init_repo();
        let err = commit_all_changes(repo.path(), "nothing to do").unwrap_err();
        assert!(matches!(err, Error::NothingToCommit { .. }));
    }

    // --- commit_paths ----------------------------------------------------------------------

    #[test]
    fn commit_paths_commits_only_the_given_paths_leaving_other_changes_uncommitted() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("untouched.txt"), "not staged\n").expect("new file");

        let before_head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let outcome = commit_paths(
            repo.path(),
            &[PathBuf::from("file.txt")],
            "ade: commit staged files",
        )
        .expect("commit_paths");

        assert_eq!(outcome.parent.as_deref(), Some(before_head.as_str()));
        assert_eq!(
            outcome.commit,
            git_output(repo.path(), &["rev-parse", "HEAD"])
        );
        // The committed file is clean...
        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(status, "", "file.txt must be committed, not left staged");
        // ...but the other real change is genuinely untouched - still there, still uncommitted.
        let untouched_status = git_output(repo.path(), &["status", "--porcelain", "untouched.txt"]);
        assert!(
            untouched_status.contains("untouched.txt"),
            "a path not passed to commit_paths must be left exactly as it was: {untouched_status:?}"
        );
        assert!(is_dirty(repo.path()).expect("is_dirty"));
    }

    /// A real regression test for a real bug: a bare `git commit` (no pathspec) commits the
    /// *entire* index, not just whatever this call's own `git add -- <paths>` just staged. The
    /// previous `commit_paths_commits_only_the_given_paths_leaving_other_changes_uncommitted`
    /// test above never actually exercised that failure mode - it only ever `write`s
    /// `untouched.txt` to disk without `git add`ing it, so it was never in the index for a bare
    /// `git commit` to sweep up in the first place. This test pre-stages the other file for
    /// real (mirroring an agent CLI running its own `git add` in this same worktree, the exact
    /// interleaving hazard `commit_all_changes`'s own doc calls out above), so it genuinely
    /// fails against a bare `git commit` and genuinely passes only once the commit itself is
    /// pathspec-limited (`-- <paths>`).
    #[test]
    fn commit_paths_never_commits_a_path_that_was_staged_by_something_else() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("also-staged.txt"), "staged elsewhere\n")
            .expect("write also-staged.txt");
        // Simulates something else in this worktree (an agent CLI, a manual `git add`) already
        // having staged a different file before the composer's own commit runs.
        git(repo.path(), &["add", "also-staged.txt"]);

        commit_paths(
            repo.path(),
            &[PathBuf::from("file.txt")],
            "ade: commit only file.txt",
        )
        .expect("commit_paths");

        let committed_files =
            git_output(repo.path(), &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(
            committed_files, "file.txt",
            "only the requested path must land in the real commit, even though \
             also-staged.txt was genuinely staged in the index at commit time"
        );
        let status = git_output(repo.path(), &["status", "--porcelain", "also-staged.txt"]);
        assert_eq!(
            status, "A  also-staged.txt",
            "also-staged.txt must remain exactly as staged (still added, still uncommitted) - \
             commit_paths must never silently commit it just because it happened to share an \
             index with the real, requested commit"
        );
    }

    #[test]
    fn commit_paths_refuses_an_empty_path_list() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let err = commit_paths(repo.path(), &[], "nothing selected").unwrap_err();
        assert!(matches!(err, Error::NothingToCommit { .. }));
        assert!(
            is_dirty(repo.path()).expect("is_dirty"),
            "refusing must not touch the working tree at all"
        );
    }

    #[test]
    fn commit_paths_real_message_becomes_the_real_commit_message() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        commit_paths(
            repo.path(),
            &[PathBuf::from("file.txt")],
            "a distinctive real commit message",
        )
        .expect("commit_paths");
        let subject = git_output(repo.path(), &["log", "-1", "--format=%s"]);
        assert_eq!(subject, "a distinctive real commit message");
    }

    #[test]
    fn undo_commit_all_changes_restores_the_exact_pre_commit_uncommitted_state() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("new.txt"), "new\n").expect("new file");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");

        undo_commit_all_changes(repo.path(), &outcome).expect("undo");

        assert!(is_dirty(repo.path()).expect("is_dirty again"));
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "HEAD"]),
            outcome.parent.clone().unwrap()
        );
        // The working tree/index content itself must be untouched by the soft reset.
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read file.txt"),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("new.txt")).expect("read new.txt"),
            "new\n"
        );
        let status = git_output(repo.path(), &["status", "--porcelain"]);
        assert!(status.contains("file.txt"));
        assert!(status.contains("new.txt"));
    }

    #[test]
    fn undo_commit_all_changes_refuses_when_head_moved_since() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");

        // Something else committed on top after the "keep" - the identity guard must refuse
        // rather than silently discard it.
        fs::write(repo.path().join("file.txt"), "changed again\n").expect("modify again");
        git(repo.path(), &["add", "file.txt"]);
        git(repo.path(), &["commit", "-m", "a later, unrelated commit"]);

        let err = undo_commit_all_changes(repo.path(), &outcome).unwrap_err();
        assert!(matches!(err, Error::HeadMovedSinceRecorded { .. }));
        // Nothing must have been touched - the later commit is still real HEAD.
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "changed again\n"
        );
    }

    #[test]
    fn redo_commit_all_changes_moves_head_forward_again_to_the_exact_same_commit() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");
        undo_commit_all_changes(repo.path(), &outcome).expect("undo");

        redo_commit_all_changes(repo.path(), &outcome).expect("redo");

        assert_eq!(
            git_output(repo.path(), &["rev-parse", "HEAD"]),
            outcome.commit
        );
        // A soft reset never touches the working tree, so content is still "changed" - the same
        // as it was the whole time, undo included.
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "changed\n"
        );
        assert!(
            !is_dirty(repo.path()).expect("is_dirty"),
            "HEAD is back at the commit that already captured this exact content"
        );
    }

    #[test]
    fn redo_commit_all_changes_refuses_when_head_moved_since_the_undo() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");
        undo_commit_all_changes(repo.path(), &outcome).expect("undo");

        // A new, unrelated commit lands on top of the undo's parent before redo runs.
        fs::write(repo.path().join("other.txt"), "other\n").expect("write");
        git(repo.path(), &["add", "other.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "unrelated commit after undo"],
        );

        let err = redo_commit_all_changes(repo.path(), &outcome).unwrap_err();
        assert!(matches!(err, Error::HeadMovedSinceRecorded { .. }));
    }

    #[test]
    fn commit_all_changes_records_the_real_interleaved_parent_not_a_stale_pre_commit_read() {
        // Live-reproduced staleness bug: an earlier version of `commit_all_changes` read
        // `HEAD` (via `describe_worktree`) *before* running `git add -A`/`git commit`, and used
        // that pre-commit read as `parent`. If anything else commits in this worktree in the
        // window between that read and the real commit - this app's whole domain is running
        // agent CLIs inside these worktrees, so an agent process committing there is realistic,
        // not exotic - the real graph becomes `A <- B(interleaved) <- C(ours)`, but `parent`
        // would be stale-recorded as `A`. `undo_commit_all_changes`'s `HEAD == outcome.commit`
        // guard would not catch this (`HEAD` genuinely *is* `C`), so it would run
        // `git reset --soft A` and silently discard the real interleaved commit `B`.
        //
        // This can't be reproduced by literally racing a second thread against
        // `commit_all_changes` (it's a single, sequential blocking call), but the fix is
        // structural: `parent` must always be derived from the real commit's own immutable
        // parent (`<commit>^`), never from a pre-commit snapshot. This test proves that
        // invariant directly: a real second commit interleaves *before* `commit_all_changes`
        // ever runs (standing in for the vulnerable window an earlier implementation had), and
        // `commit_all_changes`'s own recorded `parent` must be that real interleaved commit, not
        // the one further back - and undoing must preserve it, never silently drop it.
        let repo = init_repo();
        let commit_a = git_output(repo.path(), &["rev-parse", "HEAD"]);

        // A real, interleaved commit lands on top of `A` - standing in for a concurrent agent
        // process committing in this same worktree.
        fs::write(
            repo.path().join("interleaved.txt"),
            "from another process\n",
        )
        .expect("write interleaved file");
        git(repo.path(), &["add", "interleaved.txt"]);
        git(repo.path(), &["commit", "-m", "a real interleaved commit"]);
        let commit_b = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_ne!(commit_a, commit_b);

        // Now the real "keep all changes" commit runs on top of that.
        fs::write(repo.path().join("file.txt"), "changed by keep-all\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");

        assert_eq!(
            outcome.parent.as_deref(),
            Some(commit_b.as_str()),
            "parent must be the real interleaved commit B, not the stale commit A further back"
        );

        // Undoing must preserve the interleaved commit - never silently discard it.
        undo_commit_all_changes(repo.path(), &outcome).expect("undo");
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "HEAD"]),
            commit_b,
            "undo must land exactly on the real interleaved commit, not further back"
        );
        assert!(
            repo.path().join("interleaved.txt").exists(),
            "the real interleaved commit's content must survive the undo"
        );
        // And the commit itself must still be a real, reachable commit object - not discarded.
        assert_eq!(
            git_output(repo.path(), &["log", "--format=%H", "-1", &commit_b]),
            commit_b
        );
    }

    #[test]
    fn commit_all_changes_on_the_very_first_commit_ever_can_be_undone_and_redone() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("a.txt"), "hi\n").expect("write");

        let outcome = commit_all_changes(dir.path(), "ade: first commit ever")
            .expect("commit_all_changes on unborn branch");
        assert_eq!(outcome.parent, None);
        assert_eq!(outcome.branch.as_deref(), Some("main"));

        undo_commit_all_changes(dir.path(), &outcome).expect("undo the very first commit");
        assert!(is_dirty(dir.path()).expect("is_dirty"));
        let head_check = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("spawn git");
        assert!(
            !head_check.status.success(),
            "HEAD must be unborn again after undoing the only commit"
        );

        redo_commit_all_changes(dir.path(), &outcome).expect("redo the very first commit");
        assert_eq!(
            git_output(dir.path(), &["rev-parse", "HEAD"]),
            outcome.commit
        );
    }

    // --- discard_worktree / undo -------------------------------------------------------

    #[test]
    fn discard_worktree_snapshots_a_dirty_worktree_then_removes_it() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-a");
        add_session_worktree(repo.path(), &wt_path, "session-a");
        fs::write(wt_path.join("scratch.txt"), "wip\n").expect("write untracked file");
        fs::write(wt_path.join("file.txt"), "edited in session\n").expect("modify tracked file");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        assert_eq!(snapshot.branch.as_deref(), Some("session-a"));
        assert!(snapshot.stash.is_some());
        assert!(!wt_path.exists(), "the worktree directory must be gone");
        // The branch itself must survive (only the worktree checkout was removed).
        assert_eq!(
            git_output(
                repo.path(),
                &["rev-parse", "--verify", "refs/heads/session-a"]
            ),
            snapshot.commit
        );
    }

    #[test]
    fn discard_worktree_on_a_clean_worktree_records_no_stash() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-clean");
        add_session_worktree(repo.path(), &wt_path, "session-clean");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        assert_eq!(
            snapshot.stash, None,
            "a clean worktree has nothing real to stash"
        );
        assert!(!wt_path.exists());
    }

    #[test]
    fn a_stash_created_and_stored_from_a_worktree_survives_that_worktrees_own_removal() {
        // Direct, empirical verification of this module's own core claim (see its module docs):
        // `refs/stash` (which `git stash push` moves) lives in the *repository's* shared refs,
        // not inside the worktree's own private files, so it is genuinely still there - and
        // still `git stash apply`-able - after the worktree that created it is gone.
        let repo = init_repo();
        let wt_path = repo.path().join("session-b");
        add_session_worktree(repo.path(), &wt_path, "session-b");
        fs::write(wt_path.join("scratch.txt"), "real content\n").expect("write");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");
        let stash = snapshot.stash.clone().expect("dirty worktree must stash");

        assert!(!wt_path.exists());
        // `git stash apply` from the *main* worktree (a completely different working directory
        // than the one that created the stash) must still find it and apply real content.
        git(repo.path(), &["stash", "apply", &stash]);
        assert_eq!(
            fs::read_to_string(repo.path().join("scratch.txt")).expect("read restored file"),
            "real content\n"
        );
    }

    #[test]
    fn undo_discard_worktree_recreates_the_worktree_and_restores_the_real_stash_content() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-c");
        add_session_worktree(repo.path(), &wt_path, "session-c");
        fs::write(wt_path.join("scratch.txt"), "wip content\n").expect("write");
        fs::write(wt_path.join("file.txt"), "edited\n").expect("modify");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");
        assert!(!wt_path.exists());

        let outcome =
            undo_discard_worktree(repo.path(), &wt_path, &snapshot).expect("undo_discard_worktree");
        assert_eq!(outcome, UndoDiscardOutcome::Restored);

        assert!(wt_path.exists());
        assert_eq!(
            fs::read_to_string(wt_path.join("scratch.txt")).expect("read restored untracked file"),
            "wip content\n"
        );
        assert_eq!(
            fs::read_to_string(wt_path.join("file.txt")).expect("read restored tracked file"),
            "edited\n"
        );
    }

    #[test]
    fn undo_discard_worktree_on_a_clean_snapshot_just_recreates_the_worktree() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-clean2");
        add_session_worktree(repo.path(), &wt_path, "session-clean2");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        let outcome =
            undo_discard_worktree(repo.path(), &wt_path, &snapshot).expect("undo_discard_worktree");

        assert_eq!(outcome, UndoDiscardOutcome::Restored);
        assert!(wt_path.exists());
        assert_eq!(
            fs::read_to_string(wt_path.join("file.txt")).expect("read"),
            "hello\n"
        );
    }

    #[test]
    fn undo_discard_worktree_refuses_when_the_path_was_reoccupied() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-d");
        add_session_worktree(repo.path(), &wt_path, "session-d");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        // Something else now occupies the exact same path - just a plain directory is enough to
        // prove the guard, real or not.
        fs::create_dir_all(&wt_path).expect("recreate the path with something unrelated");

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardWorktreePathReoccupied { .. }));
    }

    #[test]
    fn undo_discard_worktree_refuses_when_the_branch_moved_since() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-e");
        add_session_worktree(repo.path(), &wt_path, "session-e");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        // Something else committed on the branch after the discard (possible even though the
        // worktree is gone - the branch itself still exists and can be advanced, e.g. via a
        // `git commit` in another checkout of it). A real, distinct new commit is built
        // directly via `commit-tree` (same tree/parent, different message, so it gets a real,
        // different sha) and the branch ref moved onto it - a bare `update-ref ... HEAD` would
        // be a no-op here, since `session-e` was branched from `HEAD` with no divergence yet.
        let tree = git_output(repo.path(), &["rev-parse", "HEAD^{tree}"]);
        let parent = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let moved_commit = git_output(
            repo.path(),
            &["commit-tree", &tree, "-p", &parent, "-m", "moved elsewhere"],
        );
        git(
            repo.path(),
            &["update-ref", "refs/heads/session-e", &moved_commit],
        );

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardBranchMovedOrReoccupied { .. }));
        assert!(
            !wt_path.exists(),
            "a refused undo must not have recreated anything"
        );
    }

    #[test]
    fn undo_discard_worktree_refuses_when_the_branch_was_deleted() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-f");
        add_session_worktree(repo.path(), &wt_path, "session-f");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        git(repo.path(), &["branch", "-D", "session-f"]);

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardBranchMovedOrReoccupied { .. }));
    }

    #[test]
    fn undo_discard_worktree_refuses_when_the_branch_is_checked_out_elsewhere_already() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-g");
        add_session_worktree(repo.path(), &wt_path, "session-g");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        // A brand new worktree reoccupies the same *branch* at a different path.
        let other_path = repo.path().join("session-g-reoccupied");
        add_worktree(repo.path(), &other_path, None, Some("session-g")).expect("recheckout");

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardBranchMovedOrReoccupied { .. }));
        assert!(!wt_path.exists());
    }

    #[test]
    fn discard_worktree_refuses_on_an_unborn_worktree() {
        // A worktree checked out at a commit still has real history behind it in this crate's
        // model (`add_worktree` always checks out an existing branch/commit); the only real way
        // to reach "no HEAD commit at all" is the main worktree of a repository that was
        // `git init`'d but never committed to.
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);

        let err = discard_worktree(dir.path(), dir.path()).unwrap_err();
        assert!(matches!(err, Error::DiscardSourceUnborn { .. }));
    }

    // --- apply_stash conflict handling --------------------------------------------------

    #[test]
    fn a_stash_apply_conflict_is_reported_honestly_not_as_an_error_and_the_stash_survives() {
        // `undo_discard_worktree`'s own mandatory identity guard makes a real conflict
        // essentially unreachable through the full guarded flow (see this module's own docs):
        // if the branch tip still matches exactly what was recorded, the freshly recreated
        // worktree's content is guaranteed identical to what existed when the stash was taken,
        // so the stash's diff always applies cleanly against it. `apply_stash` itself, however,
        // must still handle a real conflict honestly rather than assuming its caller's guard
        // makes one impossible.
        //
        // An earlier version of this test used a real *uncommitted* conflicting edit to trigger
        // this - live-reproduced as the wrong scenario during an audit: `git stash apply`
        // refuses outright ("Your local changes ... would be overwritten by merge") rather than
        // attempting a merge at all when the working tree already has uncommitted changes to the
        // same file, which is a real *non-restore failure* (case for a genuine `Err`, not
        // `RestoredWithConflicts` - nothing was restored), not a real conflict. A genuine
        // conflict-with-markers instead needs the working tree *clean* but `HEAD` moved: a new,
        // real *committed* change to the same line the stash's own base diverges from.
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "stash content\n").expect("edit for stash");
        let stash = push_and_capture_stash(repo.path()).expect("push_and_capture_stash");
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "hello\n",
            "sanity check: stash push must have restored the pre-stash content"
        );

        // A real, committed change to the same line on top of the stash's own base commit -
        // the working tree is clean, but `HEAD` has genuinely moved since the stash was taken.
        fs::write(repo.path().join("file.txt"), "committed change\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "a real committed change since the stash"],
        );

        let outcome = apply_stash(repo.path(), &stash).expect("apply_stash must not error");
        assert_eq!(
            outcome,
            UndoDiscardOutcome::RestoredWithConflicts {
                stash: stash.clone()
            }
        );
        let conflict_markers = fs::read_to_string(repo.path().join("file.txt")).expect("read");
        assert!(
            conflict_markers.contains("<<<<<<<"),
            "a real conflict must leave real conflict markers in the file: {conflict_markers:?}"
        );

        // The stash entry must survive a conflicting `apply` (never `pop`, regardless of
        // outcome) - still a real, resolvable object in the repository.
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "--verify", "refs/stash"]),
            stash
        );
    }

    #[test]
    fn apply_stash_reports_a_real_error_not_a_false_conflict_when_local_changes_block_the_apply() {
        // `git stash apply` refuses outright (no merge attempted, nothing changed) when the
        // working tree already has real uncommitted changes to the same file - this must
        // surface as a genuine `Err`, not `RestoredWithConflicts` (which would falsely claim
        // something was restored).
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "stash content\n").expect("edit for stash");
        let stash = push_and_capture_stash(repo.path()).expect("push_and_capture_stash");

        fs::write(
            repo.path().join("file.txt"),
            "a real uncommitted local edit\n",
        )
        .expect("uncommitted edit");

        let err = apply_stash(repo.path(), &stash).unwrap_err();
        assert!(matches!(err, Error::GitCommand { .. }));
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "a real uncommitted local edit\n",
            "a refused apply must leave the real local edit completely untouched"
        );
    }

    #[test]
    fn apply_stash_reports_a_real_error_for_a_stash_id_that_no_longer_exists() {
        let repo = init_repo();
        let fake_stash = "0000000000000000000000000000000000000000";
        let err = apply_stash(repo.path(), fake_stash).unwrap_err();
        assert!(matches!(err, Error::GitCommand { .. }));
    }

    // --- discard_worktree: main-worktree refusal, ignored content, restore-on-failure ----

    #[test]
    fn discard_worktree_refuses_the_main_worktree_and_touches_nothing() {
        let repo = init_repo();
        fs::write(repo.path().join("wip.txt"), "real uncommitted work\n").expect("write");

        let err = discard_worktree(repo.path(), repo.path()).unwrap_err();
        assert!(matches!(err, Error::DiscardSourceIsMainWorktree { .. }));

        assert!(repo.path().exists(), "the main worktree must still exist");
        assert_eq!(
            fs::read_to_string(repo.path().join("wip.txt")).expect("read"),
            "real uncommitted work\n",
            "a refused discard must not have stashed (or lost) anything"
        );
        assert!(
            is_dirty(repo.path()).expect("is_dirty"),
            "the real uncommitted work must not have been swept into a stash"
        );
    }

    #[test]
    fn discard_worktree_reports_ignored_content_honestly() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-h");
        add_session_worktree(repo.path(), &wt_path, "session-h");
        fs::write(wt_path.join(".gitignore"), "ignored.txt\n").expect("write");
        git(&wt_path, &["add", ".gitignore"]);
        git(&wt_path, &["commit", "-m", "add gitignore"]);
        fs::write(
            wt_path.join("ignored.txt"),
            "real content that will be lost\n",
        )
        .expect("write ignored file");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        assert!(
            snapshot.had_ignored_content,
            "real gitignored content was present and must be reported, not silently dropped"
        );
        assert!(!wt_path.exists());
    }

    #[test]
    fn discard_worktree_reports_no_ignored_content_when_there_is_none() {
        let repo = init_repo();
        let wt_path = repo.path().join("session-i");
        add_session_worktree(repo.path(), &wt_path, "session-i");
        fs::write(wt_path.join("scratch.txt"), "wip\n").expect("write");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        assert!(!snapshot.had_ignored_content);
    }

    #[test]
    fn discard_worktree_refuses_when_stash_push_captures_nothing_and_an_unrelated_stash_already_exists(
    ) {
        // Live-reproduced (see `push_and_capture_stash`'s own docs): a dirty submodule pointer
        // is real, uncommitted state `is_dirty` correctly flags as dirty, but `git stash push`
        // cannot capture it at all - it exits `0` and prints "No local changes to save" without
        // touching `refs/stash`. If an unrelated stash already occupies `refs/stash` (left over
        // from some earlier, unrelated operation in this same shared repository - nothing ever
        // drops old stash entries), the old, unguarded code would silently hand that unrelated
        // sha back as if it were this worktree's own snapshot, then proceed to force-remove the
        // real worktree believing its content was captured. This must instead refuse outright,
        // before anything real is destroyed.
        let repo = init_repo();

        let sub_dir = TempDir::new().expect("tempdir for submodule");
        init_repo_at(sub_dir.path());
        let sub_url = format!("file://{}", sub_dir.path().display());
        git(
            repo.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &sub_url,
                "sub",
            ],
        );
        git(repo.path(), &["commit", "-m", "add submodule"]);

        let wt_path = repo.path().join("session-k");
        add_session_worktree(repo.path(), &wt_path, "session-k");
        git(
            &wt_path,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "-q",
            ],
        );

        // Advance the submodule's own checked-out commit inside the worktree - a real, dirty
        // submodule pointer change (`M sub` in `git status --porcelain`) that `git stash push`
        // is empirically unable to capture, with no flag that changes that.
        //
        // `git submodule update --init` above creates a genuinely new, independent git
        // directory for the checked-out submodule (not a copy of `sub_dir`'s own local config,
        // which only applies to that original repo) - a real CI runner with no global
        // `user.email`/`user.name` configured (unlike a real developer's own machine, where
        // this test always passed silently) fails the commit below with "Author identity
        // unknown" without this, live-reproduced against a real GitHub Actions run.
        let sub_wt = wt_path.join("sub");
        git(&sub_wt, &["config", "user.email", "test@example.com"]);
        git(&sub_wt, &["config", "user.name", "Test User"]);
        fs::write(sub_wt.join("extra.txt"), "more\n").expect("write submodule file");
        git(&sub_wt, &["add", "extra.txt"]);
        git(&sub_wt, &["commit", "-m", "advance submodule"]);
        assert!(
            is_dirty(&wt_path).expect("is_dirty"),
            "a dirty submodule pointer must be flagged dirty"
        );

        // A pre-existing, unrelated stash already occupies `refs/stash` from a completely
        // different worktree/operation before this discard ever runs.
        let other_wt = repo.path().join("session-other");
        add_session_worktree(repo.path(), &other_wt, "session-other");
        fs::write(other_wt.join("unrelated.txt"), "unrelated real content\n").expect("write");
        git(
            &other_wt,
            &[
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "unrelated pre-existing stash",
            ],
        );
        let unrelated_stash = git_output(repo.path(), &["rev-parse", "--verify", "refs/stash"]);

        let err = discard_worktree(repo.path(), &wt_path).unwrap_err();
        assert!(
            matches!(err, Error::DiscardSnapshotFailed { .. }),
            "expected DiscardSnapshotFailed, got {err:?}"
        );

        // Nothing real must have been destroyed by a refused snapshot.
        assert!(
            wt_path.exists(),
            "a refused snapshot must not have removed the real worktree"
        );
        assert!(is_dirty(&wt_path).expect("is_dirty after refusal"));
        // The unrelated stash must be completely untouched - never claimed as this snapshot's.
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "--verify", "refs/stash"]),
            unrelated_stash
        );
    }

    #[test]
    #[cfg(unix)]
    fn discard_worktree_surfaces_the_real_recoverable_stash_id_if_removal_itself_fails() {
        // A genuine filesystem permission error reliably makes `git worktree remove` fail at
        // its very last step (deleting the now-empty worktree directory entry from its parent),
        // *after* it has already deleted the worktree's own contents and fully deregistered it -
        // live-reproduced directly with real `git` commands outside this test, and the reason
        // an in-place restore isn't attempted (see this function's own docs). What must still be
        // true: the real stash this function took beforehand is not lost - it's independently
        // recoverable from the main repository, regardless of what became of the worktree
        // directory itself.
        use std::os::unix::fs::PermissionsExt;

        let repo = init_repo();
        let container = TempDir::new().expect("tempdir");
        let wt_path = container.path().join("session-j");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "session-j",
                wt_path.to_str().expect("utf8 path"),
            ],
        );
        fs::write(wt_path.join("scratch.txt"), "real work in progress\n").expect("write");

        let mut perms = fs::metadata(container.path())
            .expect("metadata")
            .permissions();
        perms.set_mode(0o555);
        fs::set_permissions(container.path(), perms.clone()).expect("chmod read-only");

        let result = discard_worktree(repo.path(), &wt_path);

        // Restore write access before any further filesystem work, including this test's own
        // assertions below and `container`'s `Drop` cleanup at the end of this function.
        perms.set_mode(0o755);
        fs::set_permissions(container.path(), perms).expect("chmod restore");

        let Err(Error::DiscardRemovalFailedAfterStash { stash, .. }) = result else {
            panic!("expected Error::DiscardRemovalFailedAfterStash, got {result:?}");
        };

        // Recoverable by hand from a completely different, real worktree of the same
        // repository - the main one - proving the stash is genuinely independent of whatever
        // state the original worktree directory was left in.
        git(repo.path(), &["stash", "apply", &stash]);
        assert_eq!(
            fs::read_to_string(repo.path().join("scratch.txt")).expect("read recovered content"),
            "real work in progress\n"
        );
    }
}
