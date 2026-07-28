//! Maps `wt-core`'s [`wt_core::WorktreeResult`] list into a UI-friendly, GPUI-independent
//! shape for the left sidebar. Kept separate from the rendering code so the mapping logic
//! (label derivation, error surfacing) can be unit tested without a GPUI window.

use std::path::{Path, PathBuf};

/// One row in the worktree sidebar: either a successfully-read worktree, or a note that one
/// entry couldn't be read (kept in the list, rather than silently dropped, so a single
/// unreadable worktree doesn't make the others disappear along with it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeItem {
    /// Absolute path to the worktree, used both as the row's unique id and as the
    /// directory a selected worktree's terminal/file tree operate on.
    pub path: PathBuf,
    /// Short label shown in the sidebar: the branch name if known, otherwise the
    /// worktree's directory name, otherwise the full path as a last resort.
    pub label: String,
    pub branch: Option<String>,
    pub is_main: bool,
    pub is_locked: bool,
    /// `Some` if this entry could not be fully read (mirrors `wt_core::WorktreeResult`'s
    /// per-entry `Err`); the row is still shown so the problem is visible rather than the
    /// entry silently vanishing from the list.
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
/// metadata failed to read (`Err` in the source list) becomes a row with `error: Some(..)`
/// and a best-effort label of "(unreadable worktree)", rather than being dropped: the
/// sidebar should reflect exactly what `list_worktrees` returned, problems included.
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
}
