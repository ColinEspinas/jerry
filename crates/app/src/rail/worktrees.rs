//! Maps `wt-core`'s [`wt_core::WorktreeStatus`] list (from `wt_core::list_worktrees_porcelain`,
//! the real `git worktree list --porcelain` output) into a UI-friendly, GPUI-independent shape
//! for the left sidebar. Kept separate from the rendering code so the mapping logic is
//! unit-testable without a GPUI window.
//!
//! GitHub issue #12 ("worktrees panel is populated once and never invalidated"): this is the
//! **one** place `wt_core::WorktreeStatus` becomes a [`WorktreeItem`], used identically by the
//! app's very first load, `crate::rail::render::AdeApp::execute_prune`'s post-prune reload, and
//! the live watcher/poll refresh (`crate::rail::worktree_watch`) - see
//! `crate::root::AdeApp::load_worktrees`'s own docs. There is no separate "optimistic insert"
//! path that could diverge from what a real re-parse produces.

use std::path::{Path, PathBuf};

/// One row in the worktree sidebar: a worktree as `git` itself currently reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeItem {
    /// Also the directory a selected worktree's terminal/file tree operate on.
    pub path: PathBuf,
    /// The branch name if known, else the short commit id if detached, else the directory
    /// name, else the full path - see [`label_for`].
    pub label: String,
    pub branch: Option<String>,
    /// The main worktree (the one `git init`/`git clone` created), as opposed to one made by
    /// `git worktree add`. Always `false` for [`Self::is_bare`] - a bare repository has no
    /// main *worktree* to distinguish (mirrors `wt_core::list_worktrees`'s own rule).
    pub is_main: bool,
    /// This entry is the bare repository itself, not a real checkout - distinct from
    /// [`Self::is_main`] so the rail can render it differently rather than lumping it in with
    /// an ordinary linked worktree (GitHub issue #12's "the bare/main worktree is identified
    /// distinctly from linked worktrees" acceptance criterion).
    pub is_bare: bool,
    /// `HEAD` is a real commit checked out directly, not via a branch ref. When `true`,
    /// [`Self::branch`] is `None` and [`Self::label`]/[`Self::short_sha`] show the commit
    /// instead (issue #12's "detached HEAD worktrees show the short SHA" criterion).
    pub is_detached: bool,
    /// The first 7 characters of `HEAD`'s commit id, if any (an unborn branch has none). Shown
    /// in place of a branch name when [`Self::is_detached`].
    pub short_sha: Option<String>,
    pub is_locked: bool,
    /// The reason given to `git worktree lock --reason <...>`, if any and non-empty - surfaced
    /// as a tooltip next to the lock indicator (issue #12's "locked worktrees are visually
    /// marked, with the lock reason surfaced" criterion).
    pub lock_reason: Option<String>,
    /// `git` itself considers this worktree prunable: its administrative metadata points at a
    /// working tree directory that's no longer there (deleted by hand, moved outside of `git
    /// worktree move`, ...). Issue #12's "prunable / missing worktrees ... are marked as broken
    /// rather than silently listed as healthy" criterion. When `true`, [`Self::error`] is also
    /// `Some`, so every existing "is this worktree usable" check elsewhere in this crate
    /// (selecting it, spawning an agent in it, computing its disk usage, ...) already treats
    /// it as unusable with no separate call site needing to learn about this field.
    pub is_broken: bool,
    /// The reason `git` gives for [`Self::is_broken`], if any and non-empty.
    pub broken_reason: Option<String>,
    /// `Some` if this entry is not currently usable - today, only ever set from
    /// [`Self::is_broken`] (`git worktree list --porcelain` resolves every entry itself, unlike
    /// the old `gix`-backed per-worktree reads this replaced, so there is no other source of
    /// per-entry failure left to report here). Kept as a free-text `Option<String>` rather than
    /// folded away, since every existing `item.error.is_none()` gate elsewhere in this crate
    /// already means exactly "safe to select/spawn into/diff".
    pub error: Option<String>,
}

/// `branch` if `HEAD` is a real branch ref; otherwise `short_sha` if `HEAD` resolves to a real
/// commit (a detached checkout); otherwise the worktree directory's own name; otherwise (no
/// file name component at all - e.g. `/`) the full path.
fn label_for(path: &Path, branch: &Option<String>, short_sha: &Option<String>) -> String {
    if let Some(branch) = branch {
        return branch.clone();
    }
    if let Some(short_sha) = short_sha {
        return short_sha.clone();
    }
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

/// The first 7 characters of a full commit id - long enough to be unambiguous in the vast
/// majority of real repositories, matching `git`'s own default abbreviation length. `sha` is
/// always ASCII hex, so slicing by `char` count (rather than needing a UTF-8-boundary-aware
/// byte slice) is exact.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Converts `wt-core`'s real `git worktree list --porcelain` results into display rows. See
/// this module's own docs for why every consumer of the worktree list now goes through this one
/// function.
pub fn build_worktree_items(results: Vec<wt_core::WorktreeStatus>) -> Vec<WorktreeItem> {
    results
        .into_iter()
        .map(|status| {
            let short_sha = status.head_commit.as_deref().map(short_sha);
            let is_broken = status.is_prunable;
            let broken_reason = status.prunable_reason.clone();
            let error = if is_broken {
                Some(broken_reason.clone().unwrap_or_else(|| {
                    "worktree is prunable (its directory is missing)".to_string()
                }))
            } else {
                None
            };
            WorktreeItem {
                label: label_for(&status.path, &status.branch, &short_sha),
                path: status.path,
                branch: status.branch,
                is_main: status.is_main,
                is_bare: status.is_bare,
                is_detached: status.is_detached,
                short_sha,
                is_locked: status.is_locked,
                lock_reason: status.lock_reason,
                is_broken,
                broken_reason,
                error,
            }
        })
        .collect()
}

/// What to do with [`crate::root::AdeApp::selected`] after a worktree-list refresh - the pure
/// half of GitHub issue #12's "the currently active worktree stays highlighted across
/// refreshes; if it disappears, the user is notified and the selection falls back to the main
/// worktree" acceptance criterion. Free of `gpui` so it's unit-testable without a window; see
/// `crate::root::AdeApp::load_worktrees` for how the `gpui`-side wiring uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionRecovery {
    /// Nothing was selected before the refresh; nothing to recover.
    NoPriorSelection,
    /// The previously selected worktree (identified by path - the one stable identity a
    /// worktree has across a refresh, since `git` hands out no other id) is still present and
    /// usable in the new list, at this index. No user-visible change.
    Unchanged(usize),
    /// The previously selected worktree is gone (or now [`WorktreeItem::is_broken`]) - fell
    /// back to the main worktree at this index (`None` if the new list has no usable main
    /// worktree at all, e.g. the repository itself vanished). `notice` is the message to show
    /// the user, mirroring this crate's other "small, visible, honest error surface" banners
    /// (see `crate::sidebar::tree_ops`'s `report_tree_op_error` docs).
    FellBackToMain {
        new_index: Option<usize>,
        notice: String,
    },
}

/// Computes [`SelectionRecovery`] for a refresh: `previously_selected` is the
/// [`WorktreeItem`] that was selected *before* the refresh (by value, since the caller is about
/// to discard the old list this came from), and `new_items` is the freshly built list to
/// select into.
pub fn recover_selection(
    previously_selected: Option<&WorktreeItem>,
    new_items: &[WorktreeItem],
) -> SelectionRecovery {
    let Some(previous) = previously_selected else {
        return SelectionRecovery::NoPriorSelection;
    };

    if let Some(index) = new_items
        .iter()
        .position(|item| item.path == previous.path && item.error.is_none())
    {
        return SelectionRecovery::Unchanged(index);
    }

    let new_index = new_items
        .iter()
        .position(|item| item.is_main && item.error.is_none());
    SelectionRecovery::FellBackToMain {
        new_index,
        notice: format!(
            "\"{}\" is no longer available; switched to the main worktree",
            previous.label
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::WorktreeStatus;

    fn status(path: &str, branch: Option<&str>, is_main: bool) -> WorktreeStatus {
        WorktreeStatus {
            path: PathBuf::from(path),
            is_main,
            is_bare: false,
            head_commit: Some("deadbeefcafef00d".to_string()),
            branch: branch.map(str::to_string),
            is_detached: branch.is_none(),
            is_locked: false,
            lock_reason: None,
            is_prunable: false,
            prunable_reason: None,
        }
    }

    #[test]
    fn labels_use_branch_name_when_available() {
        let items = build_worktree_items(vec![status("/repo", Some("main"), true)]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "main");
        assert_eq!(items[0].path, PathBuf::from("/repo"));
        assert!(items[0].is_main);
        assert!(items[0].error.is_none());
    }

    #[test]
    fn detached_head_labels_with_the_short_sha_not_the_directory_name() {
        let items = build_worktree_items(vec![status("/repos/feature-x", None, false)]);
        assert_eq!(items[0].label, "deadbee");
        assert_eq!(items[0].short_sha.as_deref(), Some("deadbee"));
        assert_eq!(items[0].branch, None);
        assert!(items[0].is_detached);
        assert!(!items[0].is_main);
    }

    #[test]
    fn falls_back_to_directory_name_when_there_is_no_commit_at_all() {
        let mut unborn = status("/repos/fresh", None, false);
        unborn.head_commit = None;
        let items = build_worktree_items(vec![unborn]);
        assert_eq!(items[0].label, "fresh");
        assert_eq!(items[0].short_sha, None);
    }

    #[test]
    fn empty_list_maps_to_empty_items() {
        assert_eq!(build_worktree_items(Vec::new()), Vec::new());
    }

    #[test]
    fn locked_state_and_reason_are_preserved() {
        let mut locked = status("/repo-wt/locked", Some("locked-branch"), false);
        locked.is_locked = true;
        locked.lock_reason = Some("external disk".to_string());
        let items = build_worktree_items(vec![locked]);
        assert!(items[0].is_locked);
        assert_eq!(items[0].lock_reason.as_deref(), Some("external disk"));
    }

    #[test]
    fn unlocked_state_is_preserved() {
        let items = build_worktree_items(vec![status("/repo-wt/unlocked", Some("x"), false)]);
        assert!(!items[0].is_locked);
        assert_eq!(items[0].lock_reason, None);
    }

    #[test]
    fn bare_entry_is_flagged_bare_and_never_main() {
        let mut bare = status("/bare-repo", None, false);
        bare.is_bare = true;
        bare.head_commit = None;
        bare.is_detached = false;
        let items = build_worktree_items(vec![bare]);
        assert!(items[0].is_bare);
        assert!(!items[0].is_main);
    }

    /// The central "don't silently list a broken worktree as healthy" guarantee (issue #12):
    /// a prunable entry must be both flagged via `is_broken`/`broken_reason` *and* rejected by
    /// the plain `error.is_none()` gate every existing selection/agent-spawn/diff call site
    /// already uses, with no changes needed at those call sites.
    #[test]
    fn a_prunable_entry_is_marked_broken_and_unusable() {
        let mut gone = status("/repo-wt/gone", Some("gone-branch"), false);
        gone.is_prunable = true;
        gone.prunable_reason = Some("gitdir file points to non-existent location".to_string());
        let items = build_worktree_items(vec![gone]);
        assert!(items[0].is_broken);
        assert_eq!(
            items[0].broken_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
        assert!(
            items[0].error.is_some(),
            "a broken worktree must also fail the existing error.is_none() usability gate"
        );
    }

    #[test]
    fn a_prunable_entry_with_no_reason_still_reports_a_real_error() {
        let mut gone = status("/repo-wt/gone-no-reason", Some("gone"), false);
        gone.is_prunable = true;
        let items = build_worktree_items(vec![gone]);
        assert!(items[0].is_broken);
        assert_eq!(items[0].broken_reason, None);
        assert!(items[0].error.is_some());
    }

    #[test]
    fn a_healthy_entry_is_not_broken() {
        let items = build_worktree_items(vec![status("/repo", Some("main"), true)]);
        assert!(!items[0].is_broken);
        assert_eq!(items[0].broken_reason, None);
        assert!(items[0].error.is_none());
    }

    // --- `recover_selection` ------------------------------------------------------------

    fn item(path: &str, is_main: bool, error: Option<&str>) -> WorktreeItem {
        WorktreeItem {
            path: PathBuf::from(path),
            label: path.to_string(),
            branch: Some("x".to_string()),
            is_main,
            is_bare: false,
            is_detached: false,
            short_sha: None,
            is_locked: false,
            lock_reason: None,
            is_broken: error.is_some(),
            broken_reason: None,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn no_prior_selection_needs_no_recovery() {
        let new_items = vec![item("/repo", true, None)];
        assert_eq!(
            recover_selection(None, &new_items),
            SelectionRecovery::NoPriorSelection
        );
    }

    #[test]
    fn a_selection_that_is_still_present_is_unchanged() {
        let previous = item("/repo-wt/feature", false, None);
        let new_items = vec![
            item("/repo", true, None),
            item("/repo-wt/feature", false, None),
        ];
        assert_eq!(
            recover_selection(Some(&previous), &new_items),
            SelectionRecovery::Unchanged(1)
        );
    }

    #[test]
    fn a_vanished_selection_falls_back_to_main_with_a_notice() {
        let previous = item("/repo-wt/feature", false, None);
        let new_items = vec![item("/repo", true, None)];
        let recovery = recover_selection(Some(&previous), &new_items);
        match recovery {
            SelectionRecovery::FellBackToMain { new_index, notice } => {
                assert_eq!(new_index, Some(0));
                assert!(notice.contains("/repo-wt/feature"));
                assert!(notice.to_lowercase().contains("main"));
            }
            other => panic!("expected FellBackToMain, got {other:?}"),
        }
    }

    #[test]
    fn a_selection_that_became_broken_also_falls_back_to_main() {
        let previous = item("/repo-wt/feature", false, None);
        let new_items = vec![
            item("/repo", true, None),
            item("/repo-wt/feature", false, Some("worktree is prunable")),
        ];
        let recovery = recover_selection(Some(&previous), &new_items);
        assert!(matches!(
            recovery,
            SelectionRecovery::FellBackToMain {
                new_index: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn falling_back_with_no_usable_main_at_all_reports_none() {
        let previous = item("/repo-wt/feature", false, None);
        let new_items: Vec<WorktreeItem> = Vec::new();
        let recovery = recover_selection(Some(&previous), &new_items);
        assert!(matches!(
            recovery,
            SelectionRecovery::FellBackToMain {
                new_index: None,
                ..
            }
        ));
    }
}
