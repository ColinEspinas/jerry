//! Pure (GPUI-free) state for the context bar's `Merge` action and Surface D's
//! conflict-resolution flow - mirrors `crate::rail`/`crate::status`'s own split: this module
//! only holds already-computed facts (from `wt_core::merge` calls made in `crate::root`, which
//! has the `Context<AdeApp>` background-thread access those calls need) and pure transitions
//! over them, so the flow logic is directly unit-testable without a live GPUI window.

use std::path::{Path, PathBuf};

use wt_core::merge::{ConflictSegment, ConflictedFile, ConflictedPath, UnmergeableReason};

use crate::sessions::SessionId;

/// The live state of one merge attempt/resolution, scoped to the session whose `Merge` button
/// started it.
pub struct MergeFlow {
    pub session_id: SessionId,
    pub state: MergeFlowState,
}

/// The state of a [`MergeFlow`] - every variant corresponds to an already-happened
/// `wt_core::merge` outcome (or an error from one), never a simulated intermediate state.
pub enum MergeFlowState {
    /// The `git merge` child process is still running on a background thread.
    Running,
    /// The session branch already contributes nothing new to the base branch - `git merge`
    /// exited successfully but there was nothing to merge.
    AlreadyUpToDate { base_branch: String },
    /// The merge completed with no conflicts and is staged, uncommitted - waiting for an
    /// explicit `complete_merge` click (see `wt_core::merge::complete_merge`'s docs for why
    /// this is never auto-committed).
    Clean {
        base_branch: String,
        base_worktree_path: PathBuf,
        files: Vec<PathBuf>,
    },
    /// The merge produced one or more conflicted files, each classified from git's own ground
    /// truth (`wt_core::merge::classify_conflicted_file`) into either a resolvable text
    /// conflict or one this app has no text-hunk resolution for (a modify/delete or binary
    /// conflict - see [`ConflictedPath`]'s docs). `files` holds their live, possibly
    /// partially-resolved state; `active_file`/`active_hunk` index into it for whichever hunk
    /// Surface D currently shows (meaningless, and never read, while [`first_unresolved`]
    /// returns `None`).
    Conflicted {
        base_branch: String,
        base_worktree_path: PathBuf,
        /// Files the merge resolved automatically because the two sides' edits didn't
        /// overlap - the design's pre-flight strip ("Jerry can auto-resolve N of M files").
        clean_files: Vec<PathBuf>,
        files: Vec<ConflictedPath>,
        active_file: usize,
        active_hunk: usize,
    },
    /// An error from `wt_core::merge` (a refused dirty base worktree, no detectable base
    /// branch, the base branch not checked out anywhere, a git failure, ...) or from
    /// resolving/writing a conflicted file. Repository state is left exactly as `wt_core::merge`
    /// (or git itself) left it - never silently discarded.
    Error {
        message: String,
        /// The base worktree to offer an `Abort merge` action against, if
        /// `wt_core::merge::find_in_progress_merge`/`merge_head_exists` found `MERGE_HEAD`
        /// present there when this error state was constructed - `None` when no merge is
        /// actually in progress. Without this, an error part-way through a merge attempt (e.g.
        /// a read failure after `git merge` already ran) would leave the UI with only a
        /// `Dismiss` action that makes no git calls - dismissing would silently abandon the
        /// worktree mid-merge (`MERGE_HEAD` still present), permanently refusing every future
        /// merge attempt against that base worktree until someone runs `git merge --abort` by
        /// hand.
        abortable_worktree: Option<PathBuf>,
    },
}

/// Finds the first (file, hunk) index, in order, that's still an unresolved
/// [`ConflictSegment::Conflict`] within a [`ConflictedPath::Text`] entry - `None` once every
/// such hunk is resolved, *or* if every remaining unresolved entry is
/// [`ConflictedPath::Unmergeable`] (which has no hunk for Surface D's two-column editor to show,
/// see [`unmergeable_paths`]). Used both to pick which hunk Surface D shows first, and to
/// advance to the next one after a resolve.
pub fn first_unresolved(files: &[ConflictedPath]) -> Option<(usize, usize)> {
    for (file_index, entry) in files.iter().enumerate() {
        let ConflictedPath::Text(file) = entry else {
            continue;
        };
        if let Some(hunk_index) = file
            .segments
            .iter()
            .position(|segment| matches!(segment, ConflictSegment::Conflict(_)))
        {
            return Some((file_index, hunk_index));
        }
    }
    None
}

/// Whether every conflicted path is fully resolved - the condition that unlocks Surface D's
/// `Complete merge` action. An [`ConflictedPath::Unmergeable`] entry is *never* resolved by this
/// check (there is deliberately no automatic path from "unmergeable" to "resolved"): this app
/// has no text-hunk resolution action for a modify/delete or binary conflict, so one remaining
/// in `files` must keep blocking `Complete merge` like an unresolved text hunk would - never
/// silently count as done just because the conflict-marker parser found nothing to parse.
pub fn all_resolved(files: &[ConflictedPath]) -> bool {
    files.iter().all(|entry| match entry {
        ConflictedPath::Text(file) => file.is_resolved(),
        ConflictedPath::Unmergeable { .. } => false,
    })
}

/// The conflicted paths this app has no text-hunk resolution for at all - modify/delete or
/// binary conflicts (see [`ConflictedPath::Unmergeable`]'s docs). Surface D shows these in their
/// own panel rather than silently treating a merge as "resolved" once every *text* conflict is
/// handled while one of these still blocks `Complete merge`.
pub fn unmergeable_paths(files: &[ConflictedPath]) -> Vec<(&Path, UnmergeableReason)> {
    files
        .iter()
        .filter_map(|entry| match entry {
            ConflictedPath::Text(_) => None,
            ConflictedPath::Unmergeable {
                relative_path,
                reason,
            } => Some((relative_path.as_path(), *reason)),
        })
        .collect()
}

/// How many conflict hunks remain, total, in `file` - the pre-flight strip's "only N files need
/// you" count is `files.len() - clean_files.len()`, but this is the raw hunk-level count
/// Surface D's header (`hunk X of Y`) needs within the active file; see
/// [`hunk_position_in_file`].
pub fn hunk_count(file: &ConflictedFile) -> usize {
    file.segments
        .iter()
        .filter(|segment| matches!(segment, ConflictSegment::Conflict(_)))
        .count()
}

/// The 1-based position of `hunk_index` among only the `Conflict` segments of `file` (i.e.
/// "hunk 2 of 3", ignoring `Common` segments entirely) - `None` if `hunk_index` isn't a real
/// conflict segment (already resolved, or out of range).
pub fn hunk_position_in_file(file: &ConflictedFile, hunk_index: usize) -> Option<usize> {
    if !matches!(
        file.segments.get(hunk_index),
        Some(ConflictSegment::Conflict(_))
    ) {
        return None;
    }
    Some(
        file.segments[..=hunk_index]
            .iter()
            .filter(|segment| matches!(segment, ConflictSegment::Conflict(_)))
            .count(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::merge::ConflictHunk;

    fn conflict_hunk() -> ConflictHunk {
        ConflictHunk {
            ours_label: "HEAD".to_string(),
            ours: vec!["ours".to_string()],
            ours_start_line: 2,
            theirs_label: "feature".to_string(),
            theirs: vec!["theirs".to_string()],
            theirs_start_line: 4,
        }
    }

    fn conflicted_file(path: &str, hunks: usize) -> ConflictedFile {
        let mut segments = Vec::new();
        for _ in 0..hunks {
            segments.push(ConflictSegment::Common(vec!["context".to_string()]));
            segments.push(ConflictSegment::Conflict(conflict_hunk()));
        }
        ConflictedFile {
            relative_path: PathBuf::from(path),
            segments,
            trailing_newline: true,
        }
    }

    fn text(path: &str, hunks: usize) -> ConflictedPath {
        ConflictedPath::Text(conflicted_file(path, hunks))
    }

    fn resolve_all(file: &mut ConflictedFile) {
        for segment in &mut file.segments {
            if matches!(segment, ConflictSegment::Conflict(_)) {
                *segment = ConflictSegment::Common(vec!["resolved".to_string()]);
            }
        }
    }

    #[test]
    fn first_unresolved_finds_the_first_conflict_across_files() {
        let mut resolved = conflicted_file("a.txt", 1);
        resolve_all(&mut resolved);
        let unresolved = conflicted_file("b.txt", 2);

        let files = vec![
            ConflictedPath::Text(resolved),
            ConflictedPath::Text(unresolved),
        ];
        assert_eq!(first_unresolved(&files), Some((1, 1)));
    }

    #[test]
    fn first_unresolved_is_none_once_everything_is_resolved() {
        let mut file = conflicted_file("a.txt", 1);
        resolve_all(&mut file);
        assert_eq!(first_unresolved(&[ConflictedPath::Text(file)]), None);
    }

    #[test]
    fn first_unresolved_skips_unmergeable_entries_since_they_have_no_hunk_to_show() {
        let unmergeable = ConflictedPath::Unmergeable {
            relative_path: PathBuf::from("deleted.txt"),
            reason: UnmergeableReason::ModifyDelete,
        };
        let text_with_hunk = text("b.txt", 1);
        let files = vec![unmergeable, text_with_hunk];
        assert_eq!(
            first_unresolved(&files),
            Some((1, 1)),
            "an Unmergeable entry has no hunk index to return - the real text file must win"
        );
    }

    #[test]
    fn first_unresolved_is_none_when_only_unmergeable_entries_remain() {
        let unmergeable = ConflictedPath::Unmergeable {
            relative_path: PathBuf::from("deleted.txt"),
            reason: UnmergeableReason::Binary,
        };
        assert_eq!(
            first_unresolved(&[unmergeable]),
            None,
            "there is no real hunk anywhere to show Surface D's two-column editor"
        );
    }

    #[test]
    fn all_resolved_requires_every_file_to_have_zero_remaining_conflicts() {
        let mut a = conflicted_file("a.txt", 1);
        resolve_all(&mut a);
        let b = conflicted_file("b.txt", 1);
        assert!(!all_resolved(&[
            ConflictedPath::Text(a.clone()),
            ConflictedPath::Text(b.clone())
        ]));
        let mut b_resolved = b;
        resolve_all(&mut b_resolved);
        assert!(all_resolved(&[
            ConflictedPath::Text(a),
            ConflictedPath::Text(b_resolved)
        ]));
    }

    #[test]
    fn all_resolved_is_false_while_any_unmergeable_entry_remains() {
        // The exact real bug this exists to prevent: an Unmergeable entry must never be
        // silently treated as resolved just because it has zero `ConflictSegment::Conflict`
        // entries to parse (it has none *of any kind*, resolved or not - it was never a real
        // text conflict at all).
        let mut a = conflicted_file("a.txt", 1);
        resolve_all(&mut a);
        let unmergeable = ConflictedPath::Unmergeable {
            relative_path: PathBuf::from("deleted.txt"),
            reason: UnmergeableReason::ModifyDelete,
        };
        assert!(!all_resolved(&[ConflictedPath::Text(a), unmergeable]));
    }

    #[test]
    fn unmergeable_paths_reports_only_the_real_unmergeable_entries_with_their_real_reasons() {
        let resolved_text = text("a.txt", 1);
        let modify_delete = ConflictedPath::Unmergeable {
            relative_path: PathBuf::from("deleted.txt"),
            reason: UnmergeableReason::ModifyDelete,
        };
        let binary = ConflictedPath::Unmergeable {
            relative_path: PathBuf::from("blob.bin"),
            reason: UnmergeableReason::Binary,
        };
        let files = vec![resolved_text, modify_delete, binary];
        let result = unmergeable_paths(&files);
        assert_eq!(
            result,
            vec![
                (Path::new("deleted.txt"), UnmergeableReason::ModifyDelete),
                (Path::new("blob.bin"), UnmergeableReason::Binary),
            ]
        );
    }

    #[test]
    fn hunk_position_in_file_counts_only_conflict_segments() {
        let file = conflicted_file("a.txt", 3);
        // Segments: Common, Conflict(hunk1@1), Common, Conflict(hunk2@3), Common, Conflict(hunk3@5)
        assert_eq!(hunk_position_in_file(&file, 1), Some(1));
        assert_eq!(hunk_position_in_file(&file, 3), Some(2));
        assert_eq!(hunk_position_in_file(&file, 5), Some(3));
        // Index 0 is a `Common` segment, not a conflict.
        assert_eq!(hunk_position_in_file(&file, 0), None);
        assert_eq!(hunk_position_in_file(&file, 99), None);
    }

    #[test]
    fn hunk_count_matches_the_number_of_conflict_segments() {
        assert_eq!(hunk_count(&conflicted_file("a.txt", 3)), 3);
        assert_eq!(hunk_count(&conflicted_file("a.txt", 0)), 0);
    }
}
