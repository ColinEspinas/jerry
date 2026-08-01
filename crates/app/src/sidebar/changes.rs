//! Pure logic for Zone 3's "Changes" list (`design_handoff_jerry_ade/README.md`'s Zone 3 spec)
//! and the fold-marker treatment used when rendering a file's hunks.
//!
//! Deliberately GPUI-window-free, mirroring `crate::work_surface::state`/`crate::rail::status`'s split: only
//! the mapping from `wt_core::diff` data to which colours/labels/counts a row or fold marker
//! shows lives here; `gpui::Div` construction happens in `crate::root`, which owns the
//! `Context<AdeApp>` the click handlers (review-toggle, open-in-centre) need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::Rgba;

use crate::theme;
use crate::work_surface::agents::AgentId;
use wt_core::diff::{DiffFile, DiffHunk, DiffLineKind, FileChangeStatus};

/// Which agent(s) wrote each changed file in one worktree's diff -
/// `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §4 ("Where numbers live")'s
/// `by: 's1'`, or `by: ['s1', 's9']` when more than one agent touched the same file. Keyed by a
/// [`DiffFile::path`] (or `old_path`, before a rename - callers decide which path they're asking
/// about; this type doesn't special-case renames itself).
///
/// **Not wired to any real tracking yet.** A separate, parallel piece of work watches an agent's
/// edit tool calls and is meant to call [`Self::record`] as it observes them; this phase only
/// defines the shape so that work has somewhere real to write into, and so every current consumer
/// (`crate::code_surface::tabs::AdeApp::load_diff`, which resets this to
/// [`Authorship::default`] on every worktree/diff reload - see that method's own docs) has a real,
/// empty value to thread through rather than a stub. An empty `Authorship` means exactly what an
/// empty [`Self::authors_for`] result says: "nobody's authorship has been recorded for this file
/// yet", never fabricated as "no agent touched it".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Authorship {
    by_path: HashMap<PathBuf, Vec<AgentId>>,
}

impl Authorship {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every agent id recorded as having written `path`, in the order [`Self::record`] saw
    /// them - empty (never fabricated) when nothing has recorded authorship for it yet, which
    /// today is every path (see this type's own docs).
    pub fn authors_for(&self, path: &Path) -> &[AgentId] {
        self.by_path.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Records `agent` as (one of) `path`'s authors. Idempotent: recording the same agent
    /// against the same path twice does not duplicate the entry, so [`Self::authors_for`]'s
    /// length is always the real distinct-author count, not an edit-tool-call count.
    pub fn record(&mut self, path: PathBuf, agent: AgentId) {
        let authors = self.by_path.entry(path).or_default();
        if !authors.contains(&agent) {
            authors.push(agent);
        }
    }

    /// `true` when more than one distinct agent has written `path` - the design's amber
    /// shared-file warning: the rail's worktree-row `⚠ N` and the Changes panel's amber
    /// author-chip ring both key off this per-file check.
    pub fn has_multiple_authors(&self, path: &Path) -> bool {
        self.authors_for(path).len() > 1
    }

    /// How many of `paths` have more than one recorded author - the worktree row's `⚠ N` count
    /// itself (§4: "files two agents both wrote").
    pub fn shared_file_count<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> usize {
        paths
            .into_iter()
            .filter(|path| self.has_multiple_authors(path))
            .count()
    }

    /// How many of `paths` `agent` is a recorded author of - the rail agent row's
    /// `review ready` trailing text (`design_handoff_jerry_ade/revision 3/
    /// REVISION-2026-07-31.md` §2.3: "`12 files` ... here the count **is** the ask - the size of
    /// the review handed to you"). Deliberately per-agent, unlike [`Self::shared_file_count`]
    /// (which is worktree-wide) - the whole point of this number is "how much of *this* review
    /// is mine".
    pub fn file_count_for<'a>(
        &self,
        agent: AgentId,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> usize {
        paths
            .into_iter()
            .filter(|path| self.authors_for(path).contains(&agent))
            .count()
    }
}

/// Added/removed line counts for one file, counted directly from its hunks.
/// `wt_core::diff::DiffFile` has no separate stored counter for this, so it's recomputed here
/// from the same `DiffLine`s the diff view renders - a single source of truth for a number the
/// design shows twice (the `+n`/`−n` label and the five-segment stat bar).
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

/// The Changes row's optional tag pill - `new`/`del`, derived from the file's `FileChangeStatus`.
/// A plain modification or rename gets no pill. There's deliberately no `conflict` case:
/// `wt_core::diff::diff_against_base` is a plain two-way diff against the merge-base with no
/// merge-conflict signal to derive one from (that would need e.g. `git status`'s unmerged-path
/// list), so this function never fabricates one.
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
                fg: fg.into(),
                bg: bg.into(),
            }
        }
        ChangeTag::Deleted => {
            let (fg, bg) = theme::tag::DELETED;
            TagStyle {
                label: "del",
                fg: fg.into(),
                bg: bg.into(),
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
        StatSegment::Add => theme::diff::ADD_SIGN.into(),
        StatSegment::Del => theme::diff::DEL_SIGN.into(),
        StatSegment::Empty => theme::diff::STAT_EMPTY.into(),
    }
}

const STAT_BAR_LEN: usize = 5;

/// Splits `add`/`del` into the stat bar's five segments, proportionally - the README describes
/// the bar's look but not an exact allocation algorithm, so this is a judgment call: segment
/// counts are proportional-with-floor (`add * 5 / total`, `del * 5 / total`), except a nonzero
/// category that floors to zero is bumped up to one - otherwise "1 add line out of 400" would
/// render identical to "zero changes". Any resulting overflow past 5 total segments is trimmed
/// back down, preferring to keep both nonzero categories visible as long as either has more than
/// one segment to give up.
pub fn stat_bar_segments(add: u32, del: u32) -> [StatSegment; STAT_BAR_LEN] {
    let total = add + del;
    if total == 0 {
        return [StatSegment::Empty; STAT_BAR_LEN];
    }

    let len = STAT_BAR_LEN as u32;
    // Widened to `u64`: `add`/`del` are bounded today by `wt_core::diff`'s
    // `MAX_HUNK_LINES_PER_FILE`, so `add * len` can't overflow `u32` yet, but nothing local
    // enforces that - a `u32` multiply would silently wrap (or panic in debug) if the cap ever
    // changes, for a computation that's cheap to make correct unconditionally.
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

/// Review progress for the Changes header's `3 reviewed` label and progress bar - `reviewed`/
/// `total` are counted by the caller from real state (how many files are in the reviewed set),
/// never tracked as an independent counter that could drift.
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

/// Splits a diff file path into the Changes row / diff toolbar's `dir` and `name` fields
/// (`design_handoff_jerry_ade/README.md`: "`dir` 10.5px mono ... `name` 11.5px/450 mono").
/// `wt_core::diff` paths are repo-relative (stripped of the `a/`/`b/` prefixes `git diff`
/// prints - see that module's `parse_diff_git_header`), so a root-level file's `dir` is empty,
/// not `.`.
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

/// Whether `file` is a rename with a pre-rename path that actually differs from its current one,
/// the signal both [`rename_label`] and the Changes row's compact "moved" tag gate on.
/// `wt_core::diff::DiffFile::old_path` is documented as "only set for renames", so this is a
/// defensive `!=` check rather than independent rename detection.
pub fn is_real_rename(file: &DiffFile) -> bool {
    matches!(&file.old_path, Some(old) if old != &file.path)
}

/// The `old/path -> new/path` label for a renamed file's diff-surface toolbar - `None` unless
/// [`is_real_rename`] is true. Without it, a rename-only file (no content change, `hunks` empty)
/// would render as an indistinguishable plain filename with `+0 -0`.
pub fn rename_label(file: &DiffFile) -> Option<String> {
    if !is_real_rename(file) {
        return None;
    }
    file.old_path
        .as_ref()
        .map(|old| format!("{} \u{2192} {}", old.display(), file.path.display()))
}

/// The message for a changed file whose hunks are empty
/// (`crate::code_surface::diff_view::render_diff_file_detail`'s fallback once the binary-file branch is ruled out).
/// Distinguishes the common case (a rename with no content change, so `git diff` produced zero
/// `@@` hunks) from the generic case, rather than falling through to an empty container that
/// looks indistinguishable from a rendering bug.
pub fn empty_hunks_message(status: FileChangeStatus) -> &'static str {
    if status == FileChangeStatus::Renamed {
        "no line changes (rename only)"
    } else {
        "no line changes"
    }
}

/// Parses a `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@...` hunk header's
/// new-file range. `wt_core::diff::DiffHunk` only re-exposes the header's original text (the
/// parser that produced it is private to `wt-core`), so the fold-marker treatment below needs
/// its own small re-parse of just the new-range half.
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

/// Parses a `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@...` hunk header's
/// old-file range - mirrors [`parse_hunk_new_range`] exactly, around the `-` half instead of the
/// `+` half.
pub fn parse_hunk_old_range(header: &str) -> Option<(usize, usize)> {
    let rest = header.strip_prefix("@@ ")?;
    let minus_index = rest.find('-')?;
    let after_minus = &rest[minus_index + 1..];
    let range_end = after_minus.find(' ')?;
    let range = &after_minus[..range_end];

    let mut parts = range.splitn(2, ',');
    let start: usize = parts.next()?.parse().ok()?;
    let count: usize = match parts.next() {
        Some(count_str) => count_str.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// The real old-file/new-file line number pair for every line in `hunk`, in order - the Diff
/// view's honest gutter data. Walks `hunk.lines` forward from the real `(old_start, new_start)`
/// parsed off `hunk.header`, advancing exactly like [`crate::code_surface::code_view::changed_line_set`]
/// already does for the File view's git gutter: `Context` advances both counters (both `Some`);
/// `Added` only exists in the new file (old side `None`); `Removed` only exists in the old file
/// (new side `None`). `(None, None)` for every line if the header itself couldn't be parsed -
/// real derived data or an honest blank, never a fabricated guess.
pub fn hunk_line_numbers(hunk: &DiffHunk) -> Vec<(Option<usize>, Option<usize>)> {
    let mut old_line = parse_hunk_old_range(&hunk.header).map(|(start, _)| start);
    let mut new_line = parse_hunk_new_range(&hunk.header).map(|(start, _)| start);

    hunk.lines
        .iter()
        .map(|line| {
            let pair = match line.kind {
                DiffLineKind::Context => (old_line, new_line),
                DiffLineKind::Added => (None, new_line),
                DiffLineKind::Removed => (old_line, None),
            };
            match line.kind {
                DiffLineKind::Context => {
                    old_line = old_line.map(|n| n + 1);
                    new_line = new_line.map(|n| n + 1);
                }
                DiffLineKind::Added => new_line = new_line.map(|n| n + 1),
                DiffLineKind::Removed => old_line = old_line.map(|n| n + 1),
            }
            pair
        })
        .collect()
}

/// How many unchanged lines sit between the end of one hunk and the start of the next, in the
/// same file - the gap the design's fold marker (`⋯ N unchanged lines`) reports. `None` if
/// either header couldn't be parsed, or the computed gap isn't positive (adjacent/back-to-back
/// hunks - not expected from `git diff`, but not asserted away either).
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
    fn parse_hunk_old_range_reads_an_explicit_count() {
        assert_eq!(
            parse_hunk_old_range("@@ -10,5 +14,9 @@ fn foo() {"),
            Some((10, 5))
        );
    }

    #[test]
    fn parse_hunk_old_range_defaults_a_missing_count_to_one() {
        assert_eq!(parse_hunk_old_range("@@ -1 +1 @@"), Some((1, 1)));
    }

    #[test]
    fn parse_hunk_old_range_rejects_a_malformed_header() {
        assert_eq!(parse_hunk_old_range("not a hunk header"), None);
    }

    #[test]
    fn hunk_line_numbers_advances_old_and_new_counters_per_real_line_kind() {
        // Old range starts at 10 (5 lines); new range starts at 10 (6 lines, since one line was
        // added) - a realistic mixed hunk: one context line, one removed, one added, one context.
        let hunk = DiffHunk {
            header: "@@ -10,5 +10,6 @@".to_string(),
            lines: vec![
                line(DiffLineKind::Context, "ctx1"),
                line(DiffLineKind::Removed, "old line"),
                line(DiffLineKind::Added, "new line a"),
                line(DiffLineKind::Added, "new line b"),
                line(DiffLineKind::Context, "ctx2"),
            ],
        };
        assert_eq!(
            hunk_line_numbers(&hunk),
            vec![
                (Some(10), Some(10)),
                (Some(11), None),
                (None, Some(11)),
                (None, Some(12)),
                (Some(12), Some(13)),
            ]
        );
    }

    #[test]
    fn hunk_line_numbers_is_all_none_for_an_unparseable_header() {
        let hunk = DiffHunk {
            header: "garbage".to_string(),
            lines: vec![line(DiffLineKind::Context, "ctx")],
        };
        assert_eq!(hunk_line_numbers(&hunk), vec![(None, None)]);
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

    #[test]
    fn a_fresh_authorship_has_no_authors_for_any_path() {
        let authorship = Authorship::new();
        assert_eq!(
            authorship.authors_for(Path::new("src/main.rs")),
            &[] as &[AgentId]
        );
        assert!(!authorship.has_multiple_authors(Path::new("src/main.rs")));
    }

    #[test]
    fn recording_one_agent_makes_it_the_sole_author() {
        let mut authorship = Authorship::new();
        authorship.record(PathBuf::from("src/main.rs"), 1);
        assert_eq!(authorship.authors_for(Path::new("src/main.rs")), &[1]);
        assert!(!authorship.has_multiple_authors(Path::new("src/main.rs")));
    }

    #[test]
    fn recording_the_same_agent_twice_does_not_duplicate_it() {
        let mut authorship = Authorship::new();
        authorship.record(PathBuf::from("src/main.rs"), 1);
        authorship.record(PathBuf::from("src/main.rs"), 1);
        assert_eq!(authorship.authors_for(Path::new("src/main.rs")), &[1]);
    }

    #[test]
    fn two_distinct_agents_writing_the_same_file_is_a_shared_file() {
        let mut authorship = Authorship::new();
        authorship.record(PathBuf::from("src/main.rs"), 1);
        authorship.record(PathBuf::from("src/main.rs"), 9);
        assert_eq!(authorship.authors_for(Path::new("src/main.rs")), &[1, 9]);
        assert!(authorship.has_multiple_authors(Path::new("src/main.rs")));
    }

    #[test]
    fn shared_file_count_only_counts_paths_with_more_than_one_author() {
        let mut authorship = Authorship::new();
        authorship.record(PathBuf::from("src/main.rs"), 1);
        authorship.record(PathBuf::from("src/main.rs"), 9);
        authorship.record(PathBuf::from("src/lib.rs"), 1);

        let paths = [
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("src/untouched.rs"),
        ];
        assert_eq!(
            authorship.shared_file_count(paths.iter().map(PathBuf::as_path)),
            1
        );
    }

    #[test]
    fn different_paths_never_share_recorded_authors() {
        let mut authorship = Authorship::new();
        authorship.record(PathBuf::from("src/main.rs"), 1);
        assert!(authorship.authors_for(Path::new("src/other.rs")).is_empty());
    }

    #[test]
    fn file_count_for_only_counts_paths_this_agent_authored() {
        let mut authorship = Authorship::new();
        authorship.record(PathBuf::from("src/main.rs"), 1);
        authorship.record(PathBuf::from("src/main.rs"), 9);
        authorship.record(PathBuf::from("src/lib.rs"), 1);
        authorship.record(PathBuf::from("src/other.rs"), 9);

        let paths = [
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("src/other.rs"),
            PathBuf::from("src/untouched.rs"),
        ];
        assert_eq!(
            authorship.file_count_for(1, paths.iter().map(PathBuf::as_path)),
            2,
            "agent 1 wrote main.rs and lib.rs, not other.rs or untouched.rs"
        );
        assert_eq!(
            authorship.file_count_for(9, paths.iter().map(PathBuf::as_path)),
            2,
            "agent 9 wrote main.rs and other.rs, not lib.rs or untouched.rs"
        );
        assert_eq!(
            authorship.file_count_for(42, paths.iter().map(PathBuf::as_path)),
            0,
            "an agent with no recorded authorship anywhere gets a real 0, not a stray match"
        );
    }
}
