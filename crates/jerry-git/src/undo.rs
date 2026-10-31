//! Undo/redo primitives for two worktree-level actions: [`commit_all_changes`] and
//! [`discard_worktree`].
//!
//! Both record enough state to be genuinely undone later, and both undo paths carry a mandatory
//! identity guard: they refuse rather than proceed when the git state they would act on has moved
//! since the outcome was recorded.
//!
//! Undo of a commit is `git reset --soft <parent>`, not `git revert`: reset restores the exact
//! working tree and index from before the commit, matching what "undo" means, where a revert would
//! leave the commit in history with an inverse on top. Redo is the same move back, which stays
//! well-defined because reset never touches the object database.
//!
//! Discard stashes with `git stash push --include-untracked` before force-removing. Not `git stash
//! create`, which never captures untracked files at all and silently ignores the flag that would
//! ask it to. `refs/stash` is repository-shared, so a stash survives the removal of the worktree
//! that pushed it.
//!
//! Two gaps this surfaces rather than papers over:
//!
//! - The **main worktree can never be force-removed**, so [`discard_worktree`] refuses upfront
//!   instead of stashing and then failing - which would mutate the working tree while handing back
//!   no snapshot to undo from. If removal fails *after* a stash for any other reason, restoring in
//!   place is not reliable (`git worktree remove` usually deregisters the worktree before failing
//!   on its last step), so [`Error::DiscardRemovalFailedAfterStash`] reports the stash id instead
//!   of pretending otherwise.
//! - A stash **never captures gitignored content**, even with `--include-untracked`, and
//!   [`is_dirty`](crate::is_dirty) does not count it as dirty either. Rather than imply full
//!   safety, [`DiscardSnapshot::had_ignored_content`] records it so a caller can say what was not
//!   preserved. `git stash push --all` would capture it, at the cost of sweeping whole build
//!   directories into a git object.

use std::ffi::OsString;
use std::path::Path;

use crate::error::{Error, GitExit};
use crate::{
    add_worktree, check_success, describe_worktree, format_args, is_dirty, list_worktrees,
    open_repo, remove_worktree, run_git,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAllChangesOutcome {
    /// The branch at commit time, `None` when detached. Only consulted when [`Self::parent`] is
    /// also `None`; see [`Error::CommitHasNoParentAndNoBranch`].
    pub branch: Option<String>,
    pub commit: String,
    /// What `HEAD` pointed at before, or `None` for a branch's first commit.
    pub parent: Option<String>,
}

fn rev_parse_head(worktree_path: &Path) -> Result<String, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "HEAD".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn head_resolves(worktree_path: &Path) -> bool {
    rev_parse_head(worktree_path).is_ok()
}

/// Resolves `<commit>^`, not `HEAD^`: an agent running in this worktree can move `HEAD` at any
/// time, but a commit's own parent is immutable once created.
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

/// Stages everything - modified, deleted and untracked alike - and commits it.
///
/// Refuses with [`Error::NothingToCommit`] on a clean worktree, checked up front rather than
/// parsed out of `git commit`'s stderr afterwards.
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

    // Both read from the commit just made, not before `add`/`commit` ran: anything committing in
    // this worktree in that window would leave `parent` at the wrong end of the range, and undo's
    // `HEAD == outcome.commit` guard would not catch it - `HEAD` really is `outcome.commit`, so
    // the soft reset would silently discard the interleaved commit.
    let parent = rev_parse_parent_of(worktree_path, &commit)?;
    let branch = current_branch(worktree_path)?;

    Ok(CommitAllChangesOutcome {
        branch,
        commit,
        parent,
    })
}

/// Stages exactly `paths` and commits them, never [`commit_all_changes`]'s `-A`.
///
/// Refuses with [`Error::NothingToCommit`] on an empty `paths`, as a backstop behind whatever the
/// caller does.
///
/// The leading `git add` is an idempotent safety net: callers normally stage as the user clicks,
/// but a failed toggle or a worktree-switch race can leave their idea of "staged" out of sync
/// with the index. The contract here is "stage exactly `paths` and commit them" regardless.
///
/// No undo counterpart yet - a partial commit is not wired into [`UndoableAction`], which is a
/// gap rather than a fake undo.
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

    // Pathspec-limited, never a bare `git commit`: that commits the entire index, so anything an
    // agent staged in this worktree would ride along into a commit promised to be just `paths`.
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

/// Amends the tip commit with exactly `paths`, keeping its existing message.
///
/// `--only` is load-bearing: a bare `git commit --amend` folds in the entire index, so anything an
/// agent staged in this worktree would ride along. `--no-edit` is what keeps this an amend rather
/// than a reword.
///
/// Refuses with [`Error::NothingToCommit`] on an empty `paths`.
///
/// The amended commit is a new object with a new id, and `parent` is the pre-amend tip's parent.
/// No undo counterpart: the pre-amend commit is unreachable afterwards, so undoing would require
/// having recorded its id beforehand.
pub fn amend_head_with_paths(
    worktree_path: &Path,
    paths: &[std::path::PathBuf],
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

    let mut amend_args: Vec<OsString> = vec![
        "commit".into(),
        "--amend".into(),
        "--no-edit".into(),
        "--only".into(),
        "--".into(),
    ];
    amend_args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let amend_output = run_git(worktree_path, &amend_args)?;
    check_success(&amend_args, &amend_output)?;

    let commit = rev_parse_head(worktree_path)?;
    let parent = rev_parse_parent_of(worktree_path, &commit)?;
    let branch = current_branch(worktree_path)?;

    Ok(CommitAllChangesOutcome {
        branch,
        commit,
        parent,
    })
}

/// Stashes exactly what is staged, leaving unstaged work in place.
///
/// `--staged` (git 2.35+) is what makes that true; a plain `git stash push` would take the
/// unstaged edits too. A git too old for the flag fails loudly rather than stashing more than was
/// asked for.
///
/// Returns the stash commit id `refs/stash` now points at. Refuses with
/// [`Error::NothingToCommit`] when nothing is staged, so there is no `None` for a caller to
/// interpret.
pub fn stash_staged(worktree_path: &Path, message: &str) -> Result<String, Error> {
    if crate::stage::staged_paths(worktree_path)?.is_empty() {
        return Err(Error::NothingToCommit {
            path: worktree_path.to_path_buf(),
        });
    }

    let args: Vec<OsString> = vec![
        "stash".into(),
        "push".into(),
        "--staged".into(),
        "-m".into(),
        message.into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;

    let rev_args: Vec<OsString> = vec!["rev-parse".into(), "refs/stash".into()];
    let rev_output = run_git(worktree_path, &rev_args)?;
    check_success(&rev_args, &rev_output)?;
    Ok(String::from_utf8_lossy(&rev_output.stdout)
        .trim()
        .to_string())
}

/// Undoes a commit with `git reset --soft <parent>`, restoring the uncommitted state before it.
///
/// Identity guard: refuses with [`Error::HeadMovedSinceRecorded`] unless `outcome.commit` is
/// still `HEAD`, which would otherwise discard whatever was committed on top since.
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
            // A branch's first commit has no parent to reset to. Deleting the branch ref leaves
            // the index and working tree untouched, which is the soft-reset equivalent.
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

/// `reset --soft` never deletes the commit object, so this stays valid unless something else
/// collected it - in which case `git reset` fails rather than silently no-op'ing.
///
/// Identity guard, symmetric with [`undo_commit_all_changes`]: refuses unless `HEAD` is still
/// where the undo left it. A redo can discard newer work just as easily as an undo can.
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
            // The undo removed the branch ref, so `HEAD` must still be unborn for the guard to
            // hold; anything else is refused the same way.
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
            // `update-ref` creates a missing ref as readily as it moves one, so this is the exact
            // reverse of the undo's `update-ref -d`.
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

/// A snapshot taken immediately before [`discard_worktree`] force-removes a worktree - enough for
/// [`undo_discard_worktree`] to recreate it and restore its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardSnapshot {
    /// The branch at discard time, `None` when detached - in which case the undo recreates the
    /// worktree at [`Self::commit`] directly.
    pub branch: Option<String>,
    pub commit: String,
    /// The stash commit id, when there was uncommitted content worth preserving.
    pub stash: Option<String>,
    /// Whether gitignored content was present, which no stash captures - so `true` means
    /// [`Self::stash`] does not account for everything that was in the worktree.
    pub had_ignored_content: bool,
}

/// What restoring from a [`DiscardSnapshot`] achieved. Not a plain success/failure: a stash can
/// conflict when applied back, and a caller must not report that as fully restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoDiscardOutcome {
    /// Recreated, with any stash applied cleanly.
    Restored,
    /// Recreated, but the stash left conflict markers. The stash entry is not dropped: the undo
    /// always uses `apply`, never `pop`, so it survives as a fallback.
    RestoredWithConflicts { stash: String },
}

/// Force-removes a worktree, taking a [`DiscardSnapshot`] first - unlike a bare
/// `remove_worktree(force: true)`, which destroys uncommitted content with no recovery path.
///
/// Refusals, all before anything is touched: [`Error::DiscardSourceIsMainWorktree`], since git can
/// never remove it and stashing first would strand the content; and
/// [`Error::DiscardSourceUnborn`], since `git stash` has nothing to diff against.
///
/// A clean worktree gets no stash. A dirty one whose stash fails refuses with
/// [`Error::DiscardSnapshotFailed`] rather than forcing the removal through uncaptured.
/// [`DiscardSnapshot::had_ignored_content`] is populated either way.
///
/// A removal that fails *after* a stash returns [`Error::DiscardRemovalFailedAfterStash`].
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
        // Restoring in place is not reliable here: `git worktree remove` usually empties and
        // deregisters the worktree before failing on the final directory entry, so this path may
        // no longer be a git worktree at all. The stash is durable, so surface its id instead.
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

/// Whether `worktree_path` is the repository's main worktree, via the same lookup
/// [`crate::list_worktrees`] uses. Both sides are canonicalized, so a relative or symlinked path
/// still matches.
fn is_main_worktree(worktree_path: &Path) -> Result<bool, Error> {
    let repo = open_repo(worktree_path)?;
    let main_repo = repo.main_repo().map_err(|source| Error::Open {
        path: worktree_path.to_path_buf(),
        source: Box::new(source),
    })?;
    let Some(main_path) = main_repo.work_dir() else {
        return Ok(false);
    };
    let main_canon = std::fs::canonicalize(main_path).unwrap_or_else(|_| main_path.to_path_buf());
    let target_canon =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    Ok(main_canon == target_canon)
}

/// Whether `worktree_dir` holds any gitignored content, matching only `!!` entries so ordinary
/// changes - already covered by the stash - cannot false-positive.
fn has_ignored_content(worktree_dir: &Path) -> Result<bool, Error> {
    let args: Vec<OsString> = vec!["status".into(), "--porcelain".into(), "--ignored".into()];
    let output = run_git(worktree_dir, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with("!!")))
}

/// Reads `refs/stash`, or `None` when no stash has ever been pushed here.
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

/// Pushes a stash and returns the commit id it actually created.
///
/// `git stash push` can exit 0 printing "No local changes to save" without pushing anything, even
/// on a worktree [`crate::is_dirty`] correctly calls dirty - a dirty submodule pointer is
/// uncommitted state no flag makes it capture. `refs/stash` is then left on some unrelated
/// earlier stash, and trusting it would let [`discard_worktree`] remove a worktree believing its
/// content was captured, then restore the wrong content later.
///
/// So `refs/stash` is read before and after, and the result is only trusted if it resolves *and*
/// differs. Otherwise this returns [`Error::DiscardSnapshotFailed`] and nothing is removed.
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

    // The absolute id, not the relative `stash@{0}` name, which shifts if anything else pushes a
    // stash before this snapshot is undone. A successful exit alone proves nothing was pushed.
    let after_stash = read_stash_ref(worktree_path)?;
    match after_stash {
        Some(after) if Some(&after) != before_stash.as_ref() => Ok(after),
        _ => Err(Error::DiscardSnapshotFailed {
            path: worktree_path.to_path_buf(),
        }),
    }
}

/// Recreates a discarded worktree at its original path and branch, then restores any stash.
///
/// Identity guards, checked before anything is touched: [`Error::DiscardWorktreePathReoccupied`]
/// if something already occupies the path, and [`Error::DiscardBranchMovedOrReoccupied`] if the
/// branch is gone, checked out elsewhere, or no longer at `snapshot.commit`. Recreating anyway
/// would clobber whatever now holds that name, or resurrect stale content over newer work.
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

/// Applies `stash` - never `pop`, so it survives - and classifies the outcome.
///
/// A non-zero exit from `git stash apply` is ambiguous: git uses it both for a conflict, where
/// most content did land, and for an outright failure with nothing restored, such as a stash
/// dropped from a terminal in between. Reporting both as `RestoredWithConflicts` would falsely
/// claim something was restored in the second case, so `git diff --diff-filter=U` disambiguates:
/// non-empty means conflicting content landed, empty means nothing did and this errors.
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
    use test_support::{git, git_output, seed_repo, seed_repo_at};

    /// A session worktree on branch `name`, checked out from `main`'s tip.
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
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("new.txt"), "new\n").expect("new file");
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
        let repo = seed_repo();
        let err = commit_all_changes(repo.path(), "nothing to do").unwrap_err();
        assert!(matches!(err, Error::NothingToCommit { .. }));
    }

    // --- commit_paths ----------------------------------------------------------------------

    #[test]
    fn commit_paths_commits_only_the_given_paths_leaving_other_changes_uncommitted() {
        let repo = seed_repo();
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
        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(status, "", "file.txt must be committed, not left staged");
        let untouched_status = git_output(repo.path(), &["status", "--porcelain", "untouched.txt"]);
        assert!(
            untouched_status.contains("untouched.txt"),
            "a path not passed to commit_paths must be left exactly as it was: {untouched_status:?}"
        );
        assert!(is_dirty(repo.path()).expect("is_dirty"));
    }

    #[test]
    fn commit_paths_never_commits_a_path_that_was_staged_by_something_else() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("also-staged.txt"), "staged elsewhere\n")
            .expect("write also-staged.txt");
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");

        fs::write(repo.path().join("file.txt"), "changed again\n").expect("modify again");
        git(repo.path(), &["add", "file.txt"]);
        git(repo.path(), &["commit", "-m", "a later, unrelated commit"]);

        let err = undo_commit_all_changes(repo.path(), &outcome).unwrap_err();
        assert!(matches!(err, Error::HeadMovedSinceRecorded { .. }));
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "changed again\n"
        );
    }

    #[test]
    fn redo_commit_all_changes_moves_head_forward_again_to_the_exact_same_commit() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");
        undo_commit_all_changes(repo.path(), &outcome).expect("undo");

        redo_commit_all_changes(repo.path(), &outcome).expect("redo");

        assert_eq!(
            git_output(repo.path(), &["rev-parse", "HEAD"]),
            outcome.commit
        );
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
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");
        undo_commit_all_changes(repo.path(), &outcome).expect("undo");

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
        // Reading `HEAD` before the commit rather than the commit's own parent afterwards makes
        // `parent` stale whenever something else commits in the window: the graph becomes
        // `A <- B <- C`, `parent` records `A`, and the `HEAD == outcome.commit` guard cannot see
        // it - `HEAD` really is `C` - so the undo resets to `A` and discards `B`.
        //
        // This can't be reproduced by literally racing a second thread against
        // `commit_all_changes` (it's a single, sequential blocking call), but the fix is
        // structural: `parent` must always be derived from the real commit's own immutable
        // parent (`<commit>^`), never from a pre-commit snapshot. This test proves that
        // invariant directly: a real second commit interleaves *before* `commit_all_changes`
        // ever runs (standing in for the vulnerable window an earlier implementation had), and
        // `commit_all_changes`'s own recorded `parent` must be that real interleaved commit, not
        // the one further back - and undoing must preserve it, never silently drop it.
        let repo = seed_repo();
        let commit_a = git_output(repo.path(), &["rev-parse", "HEAD"]);

        fs::write(
            repo.path().join("interleaved.txt"),
            "from another process\n",
        )
        .expect("write interleaved file");
        git(repo.path(), &["add", "interleaved.txt"]);
        git(repo.path(), &["commit", "-m", "a real interleaved commit"]);
        let commit_b = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_ne!(commit_a, commit_b);

        fs::write(repo.path().join("file.txt"), "changed by keep-all\n").expect("modify");
        let outcome =
            commit_all_changes(repo.path(), "ade: keep all changes").expect("commit_all_changes");

        assert_eq!(
            outcome.parent.as_deref(),
            Some(commit_b.as_str()),
            "parent must be the real interleaved commit B, not the stale commit A further back"
        );

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
        let repo = seed_repo();
        let wt_path = repo.path().join("session-a");
        add_session_worktree(repo.path(), &wt_path, "session-a");
        fs::write(wt_path.join("scratch.txt"), "wip\n").expect("write untracked file");
        fs::write(wt_path.join("file.txt"), "edited in session\n").expect("modify tracked file");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        assert_eq!(snapshot.branch.as_deref(), Some("session-a"));
        assert!(snapshot.stash.is_some());
        assert!(!wt_path.exists(), "the worktree directory must be gone");
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        let wt_path = repo.path().join("session-b");
        add_session_worktree(repo.path(), &wt_path, "session-b");
        fs::write(wt_path.join("scratch.txt"), "real content\n").expect("write");

        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");
        let stash = snapshot.stash.clone().expect("dirty worktree must stash");

        assert!(!wt_path.exists());
        git(repo.path(), &["stash", "apply", &stash]);
        assert_eq!(
            fs::read_to_string(repo.path().join("scratch.txt")).expect("read restored file"),
            "real content\n"
        );
    }

    #[test]
    fn undo_discard_worktree_recreates_the_worktree_and_restores_the_real_stash_content() {
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        let wt_path = repo.path().join("session-d");
        add_session_worktree(repo.path(), &wt_path, "session-d");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        fs::create_dir_all(&wt_path).expect("recreate the path with something unrelated");

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardWorktreePathReoccupied { .. }));
    }

    #[test]
    fn undo_discard_worktree_refuses_when_the_branch_moved_since() {
        let repo = seed_repo();
        let wt_path = repo.path().join("session-e");
        add_session_worktree(repo.path(), &wt_path, "session-e");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        // The branch outlives its worktree and can still be advanced. `commit-tree` builds a
        // distinct commit (same tree and parent, different message) and the ref moves onto it; a
        // bare `update-ref ... HEAD` would be a no-op, the branch having not yet diverged.
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
        let repo = seed_repo();
        let wt_path = repo.path().join("session-f");
        add_session_worktree(repo.path(), &wt_path, "session-f");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        git(repo.path(), &["branch", "-D", "session-f"]);

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardBranchMovedOrReoccupied { .. }));
    }

    #[test]
    fn undo_discard_worktree_refuses_when_the_branch_is_checked_out_elsewhere_already() {
        let repo = seed_repo();
        let wt_path = repo.path().join("session-g");
        add_session_worktree(repo.path(), &wt_path, "session-g");
        let snapshot = discard_worktree(repo.path(), &wt_path).expect("discard_worktree");

        let other_path = repo.path().join("session-g-reoccupied");
        add_worktree(repo.path(), &other_path, None, Some("session-g")).expect("recheckout");

        let err = undo_discard_worktree(repo.path(), &wt_path, &snapshot).unwrap_err();
        assert!(matches!(err, Error::DiscardBranchMovedOrReoccupied { .. }));
        assert!(!wt_path.exists());
    }

    #[test]
    fn discard_worktree_refuses_on_an_unborn_worktree() {
        // A worktree checked out at a commit still has history behind it in this crate's model;
        // the only way
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
        // The undo's identity guard makes a conflict essentially unreachable through the full
        // flow, but `apply_stash` must still handle one rather than assume its caller's guard.
        //
        // An earlier version of this test used a real *uncommitted* conflicting edit to trigger
        // this - live-reproduced as the wrong scenario during an audit: `git stash apply`
        // refuses outright ("Your local changes ... would be overwritten by merge") rather than
        // attempting a merge at all when the working tree already has uncommitted changes to the
        // same file, which is a real *non-restore failure* (case for a genuine `Err`, not
        // `RestoredWithConflicts` - nothing was restored), not a real conflict. A genuine
        // conflict-with-markers instead needs the working tree *clean* but `HEAD` moved: a new,
        // real *committed* change to the same line the stash's own base diverges from.
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        let fake_stash = "0000000000000000000000000000000000000000";
        let err = apply_stash(repo.path(), fake_stash).unwrap_err();
        assert!(matches!(err, Error::GitCommand { .. }));
    }

    // --- discard_worktree: main-worktree refusal, ignored content, restore-on-failure ----

    #[test]
    fn discard_worktree_refuses_the_main_worktree_and_touches_nothing() {
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();

        let sub_dir = TempDir::new().expect("tempdir for submodule");
        seed_repo_at(sub_dir.path());
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

        assert!(
            wt_path.exists(),
            "a refused snapshot must not have removed the real worktree"
        );
        assert!(is_dirty(&wt_path).expect("is_dirty after refusal"));
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

        let repo = seed_repo();
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
    /// A branch whose tip commit is amendable, with a second path staged alongside - the exact
    /// interleaving `amend_head_with_paths`' `--only` exists to keep out of the amend.
    fn repo_with_an_amendable_tip() -> TempDir {
        let repo = seed_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("a.txt"), "a1\n").expect("write a");
        fs::write(repo.path().join("b.txt"), "b1\n").expect("write b");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-m", "the tip's own message"]);
        repo
    }

    #[test]
    fn amend_head_with_paths_folds_the_named_path_into_the_tip_and_keeps_its_message() {
        let repo = repo_with_an_amendable_tip();
        let before = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let commits_before = git_output(repo.path(), &["rev-list", "--count", "HEAD"]);
        fs::write(repo.path().join("a.txt"), "a1\na2\n").expect("edit a");

        let outcome = amend_head_with_paths(repo.path(), &[PathBuf::from("a.txt")])
            .expect("amend_head_with_paths");

        assert_ne!(
            outcome.commit, before,
            "an amend really rewrites the tip object"
        );
        assert_eq!(
            git_output(repo.path(), &["rev-list", "--count", "HEAD"]),
            commits_before,
            "an amend must not add a commit - that would be a plain commit wearing the wrong name"
        );
        assert_eq!(
            git_output(repo.path(), &["log", "-1", "--format=%s"]),
            "the tip's own message",
            "`--no-edit`: an amend is not a reword, and there is nowhere for a new message to \
             have come from"
        );
        assert_eq!(
            git_output(repo.path(), &["show", "HEAD:a.txt"]),
            "a1\na2",
            "the edit is now inside the tip"
        );
        assert!(
            git_output(repo.path(), &["status", "--porcelain"]).is_empty(),
            "and it is no longer uncommitted"
        );
        assert_eq!(outcome.branch, Some("feature".to_string()));
    }

    #[test]
    fn amend_head_with_paths_leaves_an_unrelated_staged_path_out_of_the_amended_tip() {
        // `--only` is what makes this true. Without it, `git commit --amend` amends the *entire*
        // index, so an agent CLI's own `git add` in this same worktree would silently ride along.
        let repo = repo_with_an_amendable_tip();
        fs::write(repo.path().join("a.txt"), "a1\na2\n").expect("edit a");
        fs::write(repo.path().join("b.txt"), "b1\nsomething else staged\n").expect("edit b");
        git(repo.path(), &["add", "b.txt"]);

        amend_head_with_paths(repo.path(), &[PathBuf::from("a.txt")]).expect("amend");

        assert_eq!(
            git_output(repo.path(), &["show", "HEAD:b.txt"]),
            "b1",
            "the other path's staged edit must not have been folded into this amend"
        );
        assert_eq!(
            git_output(repo.path(), &["diff", "--cached", "--name-only"]),
            "b.txt",
            "and it must still be staged exactly where it was"
        );
    }

    #[test]
    fn amend_head_with_paths_refuses_an_empty_path_list_rather_than_amending_the_whole_index() {
        let repo = repo_with_an_amendable_tip();
        let before = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let err = amend_head_with_paths(repo.path(), &[]).expect_err("must refuse");
        assert!(matches!(err, Error::NothingToCommit { .. }), "got {err:?}");
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "HEAD"]),
            before,
            "and nothing may have been rewritten on the way to refusing"
        );
    }

    #[test]
    fn stash_staged_takes_the_staged_edit_and_leaves_the_unstaged_one_in_the_worktree() {
        // The menu row's own hint is "keeps the worktree clean"; a plain `git stash push` would
        // take the unstaged work with it, which is not what a control under a *staged* count means.
        let repo = repo_with_an_amendable_tip();
        fs::write(repo.path().join("a.txt"), "a1\nstaged edit\n").expect("edit a");
        git(repo.path(), &["add", "a.txt"]);
        fs::write(repo.path().join("b.txt"), "b1\nunstaged edit\n").expect("edit b");

        let stash = stash_staged(repo.path(), "jerry: stash staged files").expect("stash_staged");

        assert_eq!(
            stash.len(),
            40,
            "a real stash commit id, read back off refs/stash"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a"),
            "a1\n",
            "the staged edit is gone from the working tree"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("b.txt")).expect("read b"),
            "b1\nunstaged edit\n",
            "and the unstaged edit is untouched"
        );
        assert!(
            git_output(repo.path(), &["stash", "list"]).contains("jerry: stash staged files"),
            "the stash carries the message it was given, so it is findable later"
        );
        assert!(
            git_output(repo.path(), &["diff", "--cached", "--name-only"]).is_empty(),
            "nothing is left staged"
        );
    }

    #[test]
    fn stash_staged_refuses_when_nothing_is_staged_rather_than_stashing_the_whole_worktree() {
        let repo = repo_with_an_amendable_tip();
        fs::write(repo.path().join("b.txt"), "b1\nunstaged only\n").expect("edit b");

        let err = stash_staged(repo.path(), "jerry").expect_err("must refuse");
        assert!(matches!(err, Error::NothingToCommit { .. }), "got {err:?}");
        assert_eq!(
            fs::read_to_string(repo.path().join("b.txt")).expect("read b"),
            "b1\nunstaged only\n",
            "the unstaged work must still be right where it was"
        );
        assert!(git_output(repo.path(), &["stash", "list"]).is_empty());
    }
}
