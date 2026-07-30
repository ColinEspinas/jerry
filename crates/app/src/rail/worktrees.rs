//! Maps `wt-core`'s [`wt_core::WorktreeResult`] list into a UI-friendly, GPUI-independent
//! shape for the left sidebar. Kept separate from the rendering code so the mapping logic
//! is unit-testable without a GPUI window.

use std::path::{Path, PathBuf};

/// One row in the worktree sidebar: either a successfully-read worktree, or a note that one
/// entry couldn't be read (kept in the list rather than dropped, so one bad entry doesn't
/// hide the others).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeItem {
    /// Also the directory a selected worktree's terminal/file tree operate on.
    pub path: PathBuf,
    /// The branch name if known, otherwise the directory name, otherwise the full path.
    pub label: String,
    pub branch: Option<String>,
    pub is_main: bool,
    pub is_locked: bool,
    /// `Some` if this entry could not be fully read (mirrors `wt_core::WorktreeResult`'s
    /// per-entry `Err`).
    pub error: Option<String>,
}

fn label_for(path: &Path, branch: &Option<String>) -> String {
    if let Some(branch) = branch {
        return branch.clone();
    }
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

/// Converts `wt-core`'s raw per-worktree results into display rows. A worktree whose
/// metadata failed to read becomes a row with `error: Some(..)` and label
/// "(unreadable worktree)", rather than being dropped.
pub fn build_worktree_items(results: Vec<wt_core::WorktreeResult>) -> Vec<WorktreeItem> {
    results
        .into_iter()
        .map(|result| match result {
            Ok(worktree) => WorktreeItem {
                label: label_for(&worktree.path, &worktree.branch),
                path: worktree.path,
                branch: worktree.branch,
                is_main: worktree.is_main,
                is_locked: worktree.is_locked,
                error: None,
            },
            Err(err) => WorktreeItem {
                path: PathBuf::new(),
                label: "(unreadable worktree)".to_string(),
                branch: None,
                is_main: false,
                is_locked: false,
                error: Some(err.to_string()),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::{Error, Worktree};

    fn worktree(path: &str, branch: Option<&str>, is_main: bool) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            head_commit: Some("deadbeef".to_string()),
            is_main,
            is_locked: false,
            lock_reason: None,
        }
    }

    #[test]
    fn labels_use_branch_name_when_available() {
        let items = build_worktree_items(vec![Ok(worktree("/repo", Some("main"), true))]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "main");
        assert_eq!(items[0].path, PathBuf::from("/repo"));
        assert!(items[0].is_main);
        assert!(items[0].error.is_none());
    }

    #[test]
    fn labels_fall_back_to_directory_name_for_detached_head() {
        let items = build_worktree_items(vec![Ok(worktree("/repos/feature-x", None, false))]);
        assert_eq!(items[0].label, "feature-x");
        assert_eq!(items[0].branch, None);
        assert!(!items[0].is_main);
    }

    #[test]
    fn unreadable_entries_are_kept_as_error_rows_not_dropped() {
        let items = build_worktree_items(vec![
            Ok(worktree("/repo", Some("main"), true)),
            Err(Error::WorktreeIo(std::io::Error::other("boom"))),
        ]);
        assert_eq!(
            items.len(),
            2,
            "the unreadable entry must still produce a row"
        );
        assert!(items[1].error.is_some());
        assert!(items[1].error.as_ref().unwrap().contains("boom"));
    }

    #[test]
    fn empty_list_maps_to_empty_items() {
        assert_eq!(build_worktree_items(Vec::new()), Vec::new());
    }

    /// `is_locked` must survive the mapping unchanged - the rail's prune safety check
    /// (`crate::rail::state::WorktreeNote::is_locked`) depends on it.
    #[test]
    fn locked_state_is_preserved_from_the_real_worktree_result() {
        let mut locked = worktree("/repo-wt/locked", Some("locked-branch"), false);
        locked.is_locked = true;
        let items = build_worktree_items(vec![Ok(locked)]);
        assert_eq!(items.len(), 1);
        assert!(
            items[0].is_locked,
            "a locked worktree's is_locked must be preserved as true"
        );
    }

    #[test]
    fn unlocked_state_is_preserved_from_the_real_worktree_result() {
        let unlocked = worktree("/repo-wt/unlocked", Some("unlocked-branch"), false);
        let items = build_worktree_items(vec![Ok(unlocked)]);
        assert!(!items[0].is_locked);
    }
}
