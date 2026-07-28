//! Pure logic for Zone 3's "Changes" list (`design_handoff_jerry_ade/README.md`'s Zone 3
//! spec) and the fold-marker treatment used when rendering a file's real hunks (the changes
//! list's own "click a row to open its diff in the centre" flow).
//!
//! Deliberately GPUI-window-free, mirroring `crate::work_surface`/`crate::status`'s own split:
//! only the mapping from already-real `wt_core::diff` data to which colours/labels/counts a row
//! or fold marker should show lives here; turning that into actual `gpui::Div` trees happens in
//! `crate::root`, which owns the `Context<AdeApp>` real click handlers (review-toggle,
//! open-in-centre) need.

use std::path::Path;

use gpui::Rgba;

use crate::theme;
use wt_core::diff::{DiffFile, DiffLineKind, FileChangeStatus};

/// Real added/removed line counts for one file, counted directly from its already-loaded real
/// hunks. `wt_core::diff::DiffFile` has no separate stored counter for this, so it's recomputed
/// here from the same real `DiffLine`s the diff view itself renders - never a second,
/// independently-drifting source of truth for a number the design shows twice (the `+n`/`−n`
/// label and the five-segment stat bar both come from this one function).
pub fn diff_file_stats(file: &DiffFile) -> (u32, u32) {
    let mut add = 0u32;
    let mut del = 0u32;
    for hunk in &file.hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Added => add += 1,
                DiffLineKind::Removed => del += 1,
                DiffLineKind::Context => {}
            }
        }
    }
    (add, del)
}

/// The Changes row's optional tag pill - `new`/`del`, derived directly from the file's real
/// `FileChangeStatus`. A plain modification or rename gets no pill at all (matching the
/// design's "optional tag pill"), and there is deliberately no `conflict` case: this app's diff
/// data (`wt_core::diff::diff_against_base`, a plain two-way diff against the merge-base) has no
/// real merge-conflict signal to derive one from - a genuine conflict indicator would need e.g.
/// `git status`'s unmerged-path list, or knowing which *other* session has touched the same
/// file, neither of which this phase's data model carries. Rather than fabricate a `conflict`
/// pill from data that can't actually express it, this function simply never produces one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeTag {
    New,
    Deleted,
}

pub fn change_tag(status: FileChangeStatus) -> Option<ChangeTag> {
    match status {
        FileChangeStatus::Added => Some(ChangeTag::New),
        FileChangeStatus::Deleted => Some(ChangeTag::Deleted),
        FileChangeStatus::Modified | FileChangeStatus::Renamed => None,
    }
}

pub struct TagStyle {
    pub label: &'static str,
    pub fg: Rgba,
    pub bg: Rgba,
}

pub fn tag_style(tag: ChangeTag) -> TagStyle {
    match tag {
        ChangeTag::New => {
            let (fg, bg) = theme::tag::NEW;
            TagStyle {
                label: "new",
                fg,
                bg,
            }
        }
        ChangeTag::Deleted => {
            let (fg, bg) = theme::tag::DELETED;
            TagStyle {
                label: "del",
                fg,
                bg,
            }
        }
    }
}

/// One 3×8 segment of the Changes row's five-segment stat bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatSegment {
    Add,
    Del,
    Empty,
}

pub fn stat_segment_color(segment: StatSegment) -> Rgba {
    match segment {
        StatSegment::Add => theme::diff::ADD_SIGN,
        StatSegment::Del => theme::diff::DEL_SIGN,
        StatSegment::Empty => theme::diff::STAT_EMPTY,
    }
}

const STAT_BAR_LEN: usize = 5;

/// Splits `add`/`del` into the stat bar's five segments, proportionally - `design_handoff_
/// jerry_ade/README.md` describes the bar's look (`#4e8c68` / `#a35f5b` / `#22262a`) but not an
/// exact allocation algorithm, so this is a documented judgment call: segment counts are
/// proportional-with-floor (`add * 5 / total`, `del * 5 / total`), except a nonzero category
/// that floors to zero segments is bumped up to one - otherwise "1 add line out of 400" would
/// render visually identical to "zero changes", which defeats the bar's whole purpose. Any
/// resulting overflow past 5 total segments (possible after that bump) is trimmed back down,
/// preferring to keep both nonzero categories visible for as long as either still has more than
/// one segment to give up.
pub fn stat_bar_segments(add: u32, del: u32) -> [StatSegment; STAT_BAR_LEN] {
    let total = add + del;
    if total == 0 {
        return [StatSegment::Empty; STAT_BAR_LEN];
    }

    let len = STAT_BAR_LEN as u32;
    // Widened to `u64` for the multiplication: `add`/`del` are themselves bounded today (by
    // `wt_core::diff`'s own `MAX_HUNK_LINES_PER_FILE` cap), so `add * len` can't actually
    // overflow `u32` yet, but there's no local invariant enforcing that - a `u32` multiply
    // here would silently become a debug-build panic (release: silent wraparound) the moment
    // that cap ever changes, for a computation that's cheap to make correct unconditionally.
    let mut add_segments = (add as u64 * len as u64 / total as u64) as u32;
    let mut del_segments = (del as u64 * len as u64 / total as u64) as u32;
    if add > 0 && add_segments == 0 {
        add_segments = 1;
    }
    if del > 0 && del_segments == 0 {
        del_segments = 1;
    }
    while add_segments + del_segments > len {
        if add_segments >= del_segments && add_segments > 1 {
            add_segments -= 1;
        } else if del_segments > 1 {
            del_segments -= 1;
        } else {
            break;
        }
    }

    let mut segments = [StatSegment::Empty; STAT_BAR_LEN];
    let mut index = 0usize;
    for _ in 0..add_segments {
        segments[index] = StatSegment::Add;
        index += 1;
    }
    for _ in 0..del_segments {
        segments[index] = StatSegment::Del;
        index += 1;
    }
    segments
}

/// Real review progress for the Changes header's `3 reviewed` label and progress bar -
/// `reviewed`/`total` are both counted by the caller from real state (how many of the diff's
/// actual files are in the reviewed set), never tracked as an independent counter that could
/// drift from the real per-file set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReviewProgress {
    pub reviewed: usize,
    pub total: usize,
}

impl ReviewProgress {
    /// `0.0` (not `NaN`) when there is nothing to review.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.reviewed as f32 / self.total as f32
        }
    }
}

/// Splits a real diff file path into the Changes row / diff toolbar's `dir` and `name` fields
/// (`design_handoff_jerry_ade/README.md`: "`dir` 10.5px mono ... `name` 11.5px/450 mono").
/// `wt_core::diff` paths are repo-relative (stripped of the `a/`/`b/` prefixes `git diff`
/// itself prints - see that module's `parse_diff_git_header`), so a root-level file's `dir` is
/// simply empty, not `.`.
pub fn split_dir_name(path: &Path) -> (String, String) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.display().to_string(),
        _ => String::new(),
    };
    (dir, name)
}

/// Whether `file` is a real rename with a pre-rename path that actually differs from its
/// current one - the signal both [`rename_label`] and the Changes row's compact "moved" tag
/// gate on. `wt_core::diff::DiffFile::old_path`'s own docs say it's "only set for renames", so
/// this is really just a defensive `!=` check (never assumed away) rather than a second,
/// independent rename detection.
pub fn is_real_rename(file: &DiffFile) -> bool {
    matches!(&file.old_path, Some(old) if old != &file.path)
}

/// The real `old/path -> new/path` label for a renamed file's diff-surface toolbar - `None`
/// unless [`is_real_rename`] is true. A rename-only file (no content change - `hunks` is empty)
/// otherwise renders as an indistinguishable plain filename with `+0 -0`, which is exactly the
/// regression this exists to fix (see `crate::root::render_diff_surface`'s use of it).
pub fn rename_label(file: &DiffFile) -> Option<String> {
    if !is_real_rename(file) {
        return None;
    }
    file.old_path
        .as_ref()
        .map(|old| format!("{} \u{2192} {}", old.display(), file.path.display()))
}

/// The real, honest message for a changed file whose real hunks are empty
/// (`crate::root::render_diff_file_detail`'s fallback once the binary-file branch is ruled
/// out) - distinguishes the common real cause (a rename with no content change, so `git diff`
/// produced zero `@@` hunks for it) from the generic case, rather than falling through to an
/// empty container that looks indistinguishable from a rendering bug.
pub fn empty_hunks_message(status: FileChangeStatus) -> &'static str {
    if status == FileChangeStatus::Renamed {
        "no line changes (rename only)"
    } else {
        "no line changes"
    }
}

/// Parses a `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@...` hunk header's
/// new-file range. `wt_core::diff::DiffHunk` only re-exposes the header's original text (its own
/// parser that produced the hunk in the first place is private to the `wt-core` crate), so the
/// fold-marker treatment below needs its own small, independent re-parse of just the new-range
/// half of that same header - never a fabricated or estimated value.
pub fn parse_hunk_new_range(header: &str) -> Option<(usize, usize)> {
    let rest = header.strip_prefix("@@ ")?;
    let plus_index = rest.find('+')?;
    let after_plus = &rest[plus_index + 1..];
    let range_end = after_plus.find(' ')?;
    let range = &after_plus[..range_end];

    let mut parts = range.splitn(2, ',');
    let start: usize = parts.next()?.parse().ok()?;
    let count: usize = match parts.next() {
        Some(count_str) => count_str.parse().ok()?,
        // A range with no explicit `,<count>` means a count of exactly 1, per the unified
        // diff format (the same shorthand `wt_core::diff`'s own private parser documents).
        None => 1,
    };
    Some((start, count))
}

/// How many real unchanged lines sit between the end of one hunk and the start of the next, in
/// the same file - the gap the design's fold marker (`⋯ N unchanged lines`) reports. `None` if
/// either header couldn't be parsed (defensive: never fabricate a count from an unparseable
/// header) or the computed gap isn't positive (adjacent/back-to-back hunks - not expected from a
/// real `git diff`, but not asserted away either).
pub fn fold_gap_between(prev_header: &str, next_header: &str) -> Option<usize> {
    let (prev_start, prev_count) = parse_hunk_new_range(prev_header)?;
    let (next_start, _) = parse_hunk_new_range(next_header)?;
    let prev_end = prev_start + prev_count;
    if next_start > prev_end {
        Some(next_start - prev_end)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wt_core::diff::{DiffHunk, DiffLine};

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            content: text.to_string(),
        }
    }

    fn sample_file(status: FileChangeStatus, hunks: Vec<DiffHunk>) -> DiffFile {
        DiffFile {
            path: PathBuf::from("src/main.rs"),
            old_path: None,
            status,
            is_binary: false,
            hunks,
            truncated: false,
        }
    }

    #[test]
    fn diff_file_stats_counts_added_and_removed_lines_only() {
        let file = sample_file(
            FileChangeStatus::Modified,
            vec![DiffHunk {
                header: "@@ -1,2 +1,3 @@".to_string(),
                lines: vec![
                    line(DiffLineKind::Context, "ctx"),
                    line(DiffLineKind::Added, "a1"),
                    line(DiffLineKind::Added, "a2"),
                    line(DiffLineKind::Removed, "r1"),
                ],
            }],
        );
        assert_eq!(diff_file_stats(&file), (2, 1));
    }

    #[test]
    fn diff_file_stats_is_zero_for_a_file_with_no_hunks() {
        let file = sample_file(FileChangeStatus::Renamed, Vec::new());
        assert_eq!(diff_file_stats(&file), (0, 0));
    }

    #[test]
    fn added_files_get_the_new_tag_and_deleted_files_get_the_del_tag() {
        assert_eq!(change_tag(FileChangeStatus::Added), Some(ChangeTag::New));
        assert_eq!(
            change_tag(FileChangeStatus::Deleted),
            Some(ChangeTag::Deleted)
        );
    }

    #[test]
    fn modified_and_renamed_files_get_no_tag() {
        assert_eq!(change_tag(FileChangeStatus::Modified), None);
        assert_eq!(change_tag(FileChangeStatus::Renamed), None);
    }

    #[test]
    fn zero_changes_is_an_all_empty_bar() {
        assert_eq!(stat_bar_segments(0, 0), [StatSegment::Empty; 5]);
    }

    #[test]
    fn pure_additions_fill_the_entire_bar_with_add_segments() {
        assert_eq!(
            stat_bar_segments(40, 0),
            [
                StatSegment::Add,
                StatSegment::Add,
                StatSegment::Add,
                StatSegment::Add,
                StatSegment::Add
            ]
        );
    }

    #[test]
    fn pure_deletions_fill_the_entire_bar_with_del_segments() {
        assert_eq!(
            stat_bar_segments(0, 40),
            [
                StatSegment::Del,
                StatSegment::Del,
                StatSegment::Del,
                StatSegment::Del,
                StatSegment::Del
            ]
        );
    }

    #[test]
    fn every_stat_bar_is_exactly_five_segments() {
        for (add, del) in [(0, 0), (1, 0), (0, 1), (1, 1), (3, 400), (400, 3), (7, 7)] {
            assert_eq!(stat_bar_segments(add, del).len(), 5);
        }
    }

    #[test]
    fn a_tiny_minority_category_still_gets_at_least_one_visible_segment() {
        // 1 out of 400 lines added would floor to 0/5 segments without the "bump nonzero up
        // to 1" rule this function documents - confirm the bump actually applies.
        let segments = stat_bar_segments(1, 399);
        assert!(segments.contains(&StatSegment::Add));
    }

    #[test]
    fn review_progress_fraction_handles_zero_total_without_dividing_by_zero() {
        let progress = ReviewProgress {
            reviewed: 0,
            total: 0,
        };
        assert_eq!(progress.fraction(), 0.0);
    }

    #[test]
    fn review_progress_fraction_is_reviewed_over_total() {
        let progress = ReviewProgress {
            reviewed: 3,
            total: 12,
        };
        assert!((progress.fraction() - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn split_dir_name_separates_a_nested_path() {
        let (dir, name) = split_dir_name(Path::new("src/db/query_builder.rs"));
        assert_eq!(dir, "src/db");
        assert_eq!(name, "query_builder.rs");
    }

    #[test]
    fn split_dir_name_leaves_dir_empty_for_a_root_level_file() {
        let (dir, name) = split_dir_name(Path::new("Cargo.toml"));
        assert_eq!(dir, "");
        assert_eq!(name, "Cargo.toml");
    }

    #[test]
    fn parse_hunk_new_range_reads_an_explicit_count() {
        assert_eq!(
            parse_hunk_new_range("@@ -10,5 +14,9 @@ fn foo() {"),
            Some((14, 9))
        );
    }

    #[test]
    fn parse_hunk_new_range_defaults_a_missing_count_to_one() {
        assert_eq!(parse_hunk_new_range("@@ -1 +1 @@"), Some((1, 1)));
    }

    #[test]
    fn parse_hunk_new_range_rejects_a_malformed_header() {
        assert_eq!(parse_hunk_new_range("not a hunk header"), None);
    }

    #[test]
    fn fold_gap_between_computes_the_real_unchanged_span() {
        // First hunk covers new lines 10..=14 (start 10, count 5); the next starts at 40 -
        // 25 real unchanged lines sit between them.
        assert_eq!(
            fold_gap_between("@@ -10,5 +10,5 @@", "@@ -30,5 +40,5 @@"),
            Some(25)
        );
    }

    #[test]
    fn fold_gap_between_is_none_for_back_to_back_hunks() {
        assert_eq!(fold_gap_between("@@ -1,5 +1,5 @@", "@@ -6,5 +6,5 @@"), None);
    }

    #[test]
    fn fold_gap_between_is_none_when_a_header_is_unparseable() {
        assert_eq!(fold_gap_between("garbage", "@@ -6,5 +6,5 @@"), None);
    }

    fn renamed_file(old_path: Option<PathBuf>) -> DiffFile {
        DiffFile {
            path: PathBuf::from("src/new_name.rs"),
            old_path,
            status: FileChangeStatus::Renamed,
            is_binary: false,
            hunks: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn a_rename_with_a_real_different_old_path_is_a_real_rename() {
        let file = renamed_file(Some(PathBuf::from("src/old_name.rs")));
        assert!(is_real_rename(&file));
        assert_eq!(
            rename_label(&file),
            Some("src/old_name.rs \u{2192} src/new_name.rs".to_string())
        );
    }

    #[test]
    fn no_old_path_is_not_a_real_rename() {
        let file = renamed_file(None);
        assert!(!is_real_rename(&file));
        assert_eq!(rename_label(&file), None);
    }

    #[test]
    fn an_old_path_identical_to_the_current_path_is_not_a_real_rename() {
        // Defensive: `wt_core::diff` shouldn't produce this, but never assume it away.
        let file = renamed_file(Some(PathBuf::from("src/new_name.rs")));
        assert!(!is_real_rename(&file));
        assert_eq!(rename_label(&file), None);
    }

    #[test]
    fn empty_hunks_message_names_rename_only_for_a_renamed_file() {
        assert_eq!(
            empty_hunks_message(FileChangeStatus::Renamed),
            "no line changes (rename only)"
        );
    }

    #[test]
    fn empty_hunks_message_is_generic_for_a_non_renamed_file() {
        assert_eq!(
            empty_hunks_message(FileChangeStatus::Added),
            "no line changes"
        );
        assert_eq!(
            empty_hunks_message(FileChangeStatus::Modified),
            "no line changes"
        );
        assert_eq!(
            empty_hunks_message(FileChangeStatus::Deleted),
            "no line changes"
        );
    }
}
