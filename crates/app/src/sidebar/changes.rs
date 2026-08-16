//! Pure logic for Zone 3's "Changes" list (`docs/design/sidebar.md`)
//! and the fold-marker treatment used when rendering a file's hunks.
//!
//! Deliberately GPUI-window-free, mirroring `crate::work_surface::state`/`crate::rail::status`'s split: only
//! the mapping from `wt_core::diff` data to which colours/labels/counts a row or fold marker
//! shows lives here; `gpui::Div` construction happens in `crate::root`, which owns the
//! `Context<AdeApp>` the click handlers (review-toggle, open-in-centre) need.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::Rgba;

use crate::root::plural;
use crate::theme;
use wt_core::diff::{DiffFile, DiffHunk, DiffLineKind, FileChangeStatus};

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

/// Git's own status letter for one file row - `A`, `M` or `D`.
///
/// **Total, not optional**, and that is the whole point: the `new`/`del` word pills this
/// replaced marked only the exceptions, so a *modified* file - the common case - got no mark at
/// all and "the row could not answer 'what happened to this file', which is the first thing a
/// reviewer asks". Every row gets a letter, in a fixed column, so every filename also starts on
/// the same x.
///
/// There is deliberately no `conflict` variant, and never was one to remove here: §4j's second
/// fault is that "`conflict` is not a git status - it was an overlay meaning 'two agents touched
/// this', which the pair of author chips beside it already states", and
/// `wt_core::diff::diff_against_base` is a plain two-way diff with no unmerged-path signal to
/// derive one from anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLetter {
    Added,
    Modified,
    Deleted,
}

/// The letter for a file's `FileChangeStatus`.
///
/// `Renamed` maps to `Modified`, not to a fourth `R` letter: §4j's table is exactly three
/// letters, each with one colour, and the rename fact already has its own dedicated channel on
/// the row - `crate::sidebar::render::render_moved_tag`'s neutral `moved` chip, which
/// [`is_real_rename`] gates and which this change does not touch. Adding `R` here would be a
/// fourth colour the design never allocated, stating a fact the row already states.
pub fn status_letter(status: FileChangeStatus) -> StatusLetter {
    match status {
        FileChangeStatus::Added => StatusLetter::Added,
        FileChangeStatus::Deleted => StatusLetter::Deleted,
        FileChangeStatus::Modified | FileChangeStatus::Renamed => StatusLetter::Modified,
    }
}

impl StatusLetter {
    /// The single character painted in the row's fixed status column.
    pub fn glyph(self) -> &'static str {
        match self {
            StatusLetter::Added => "A",
            StatusLetter::Modified => "M",
            StatusLetter::Deleted => "D",
        }
    }

    /// What the letter's own tooltip spells out - §4j: "Tooltips spell the letter out."
    pub fn tooltip(self) -> &'static str {
        match self {
            StatusLetter::Added => "Added",
            StatusLetter::Modified => "Modified",
            StatusLetter::Deleted => "Deleted",
        }
    }

    pub fn color(self) -> Rgba {
        match self {
            StatusLetter::Added => theme::tag::STATUS_ADDED.into(),
            StatusLetter::Modified => theme::tag::STATUS_MODIFIED.into(),
            StatusLetter::Deleted => theme::tag::STATUS_DELETED.into(),
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

/// Whether `path` is a change that is **already committed and clean** - it differs from the base
/// branch (that is why `wt_core::diff::diff_against_base` listed it at all), but a real commit on
/// this branch already holds that difference and nothing about it is uncommitted right now.
///
/// GitHub issue #220: `diff_against_base` diffs the working tree against the *merge-base with the
/// default branch*, so its file list deliberately mixes committed and uncommitted changes and
/// `DiffFile` itself carries no signal to tell them apart. `dirty` is that signal -
/// `wt_core::stage::dirty_paths`' real `git status --porcelain` result, as cached in
/// [`crate::root::AdeApp::dirty_files`].
///
/// `dirty` is an `Option` and `None` means **not known yet** (the query hasn't landed, or it
/// failed), never "nothing is dirty": with no evidence this returns `false`, so a row falls back
/// to the ordinary stageable presentation rather than claiming a file is committed on the strength
/// of an absent answer.
pub fn is_committed_clean(path: &Path, dirty: Option<&HashSet<PathBuf>>) -> bool {
    match dirty {
        Some(dirty) => !dirty.contains(path),
        None => false,
    }
}

/// How many of `files` actually have something to stage - i.e. everything
/// [`is_committed_clean`] doesn't rule out. The real denominator for the Changes header's staged
/// progress: counting a committed-clean file in it would make `1 staged` out of a 3-file list read
/// as two-thirds of the work outstanding when in truth there is nothing left to stage.
pub fn stageable_count(files: &[DiffFile], dirty: Option<&HashSet<PathBuf>>) -> usize {
    files
        .iter()
        .filter(|file| !is_committed_clean(&file.path, dirty))
        .count()
}

/// Staged progress for the Changes header's `N of M staged` label and progress bar (Revision R12
/// §5: the checkbox **is** staging, not "reviewed") - `staged`/`total` are counted by the caller
/// from real state (how many files are in [`crate::root::AdeApp::staged_files`], out of
/// [`stageable_count`]'s genuinely stageable files), never tracked as an independent counter that
/// could drift.
///
/// `total` is the *stageable* count, not the diff's whole file list: a committed-clean file
/// (GitHub issue #220) has nothing to stage, so including it would permanently pin the fraction
/// below 1.0 for a worktree where everything stageable really is staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StagedProgress {
    pub staged: usize,
    pub total: usize,
}

impl StagedProgress {
    /// `0.0` (not `NaN`) when there is nothing to stage.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.staged as f32 / self.total as f32
        }
    }
}

/// Splits a diff file path into the Changes row / diff toolbar's `dir` and `name` fields.
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

/// The staged subset of `files`, in the same relative order - the one place the commit
/// composer's file list, diffstat, and action-button count are all derived from (Revision R12
/// §5: "derive the staged set once, early, since the header count/diffstat/composer/action-
/// button all read from it").
pub fn staged_subset<'a>(files: &'a [DiffFile], staged: &HashSet<PathBuf>) -> Vec<&'a DiffFile> {
    files
        .iter()
        .filter(|file| staged.contains(&file.path))
        .collect()
}

/// The staged subset's combined `+add`/`\u{2212}del`, summed the same way [`diff_file_stats`]
/// counts a single file - the commit composer header's right-aligned diffstat (`#5f9c78`).
pub fn staged_diff_stats(staged: &[&DiffFile]) -> (u32, u32) {
    staged.iter().fold((0, 0), |(add, del), file| {
        let (file_add, file_del) = diff_file_stats(file);
        (add + file_add, del + file_del)
    })
}

/// The commit composer's primary-button label: a ghost `Commit` with nothing staged, or `Commit
/// N files` (singular for exactly one) once something is.
pub fn commit_button_label(staged_count: usize) -> String {
    if staged_count == 0 {
        "Commit".to_string()
    } else {
        format!("Commit {}", plural::count(staged_count, "file", None))
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

    /// `STAGE-A-CHANGELOG.md` §4j: every status maps to a letter, including the common case.
    /// The word pills this replaced returned `None` for `Modified`, which is the exact defect
    /// §4j names - "only the exceptions were marked".
    #[test]
    fn every_file_status_gets_a_letter_including_the_common_case() {
        assert_eq!(status_letter(FileChangeStatus::Added), StatusLetter::Added);
        assert_eq!(
            status_letter(FileChangeStatus::Modified),
            StatusLetter::Modified
        );
        assert_eq!(
            status_letter(FileChangeStatus::Deleted),
            StatusLetter::Deleted
        );
        // A rename is a modification as far as the letter column is concerned - the row's own
        // `moved` chip is what states the rename. See `status_letter`'s own docs.
        assert_eq!(
            status_letter(FileChangeStatus::Renamed),
            StatusLetter::Modified
        );
    }

    #[test]
    fn the_letters_are_gits_own_and_their_tooltips_spell_them_out() {
        assert_eq!(StatusLetter::Added.glyph(), "A");
        assert_eq!(StatusLetter::Modified.glyph(), "M");
        assert_eq!(StatusLetter::Deleted.glyph(), "D");
        assert_eq!(StatusLetter::Added.tooltip(), "Added");
        assert_eq!(StatusLetter::Modified.tooltip(), "Modified");
        assert_eq!(StatusLetter::Deleted.tooltip(), "Deleted");
    }

    /// §4j's own colour table, and its stated reason for the middle row: added is green, deleted
    /// is red, and modified is *neutral* - "the common case does not shout". A `M` painted in
    /// either hue would put the loudest colour in the panel on the least remarkable row.
    #[test]
    fn modified_is_neutral_while_added_and_deleted_carry_their_hues() {
        assert_eq!(StatusLetter::Added.color(), theme::tag::STATUS_ADDED.into());
        assert_eq!(
            StatusLetter::Modified.color(),
            theme::tag::STATUS_MODIFIED.into()
        );
        assert_eq!(
            StatusLetter::Deleted.color(),
            theme::tag::STATUS_DELETED.into()
        );
        assert_ne!(StatusLetter::Modified.color(), StatusLetter::Added.color());
        assert_ne!(
            StatusLetter::Modified.color(),
            StatusLetter::Deleted.color()
        );
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
    fn a_path_missing_from_a_known_dirty_set_is_committed_clean() {
        let dirty: HashSet<PathBuf> = [PathBuf::from("src/edited.rs")].into_iter().collect();
        assert!(is_committed_clean(
            Path::new("src/committed.rs"),
            Some(&dirty)
        ));
        assert!(!is_committed_clean(
            Path::new("src/edited.rs"),
            Some(&dirty)
        ));
    }

    #[test]
    fn nothing_is_committed_clean_while_the_dirty_set_is_still_unknown() {
        // `None` is "the `git status` answer hasn't landed", not "nothing is dirty" - claiming a
        // file is committed on the strength of an absent answer is exactly the mislabelling
        // GitHub issue #220 is about, in the other direction.
        assert!(!is_committed_clean(Path::new("src/anything.rs"), None));
    }

    #[test]
    fn stageable_count_excludes_committed_clean_files() {
        let files = vec![
            changed_file("src/committed.rs", 4, 0),
            changed_file("src/edited.rs", 1, 1),
        ];
        let dirty: HashSet<PathBuf> = [PathBuf::from("src/edited.rs")].into_iter().collect();
        assert_eq!(stageable_count(&files, Some(&dirty)), 1);
    }

    #[test]
    fn stageable_count_falls_back_to_every_file_when_the_dirty_set_is_unknown() {
        let files = vec![
            changed_file("src/a.rs", 1, 0),
            changed_file("src/b.rs", 0, 1),
        ];
        assert_eq!(stageable_count(&files, None), 2);
    }

    #[test]
    fn a_fully_committed_branch_has_nothing_stageable_and_so_a_zero_fraction_not_a_partial_one() {
        let files = vec![
            changed_file("src/one.rs", 3, 0),
            changed_file("src/two.rs", 2, 0),
        ];
        let stageable = stageable_count(&files, Some(&HashSet::new()));
        assert_eq!(stageable, 0);
        let progress = StagedProgress {
            staged: 0,
            total: stageable,
        };
        assert_eq!(
            progress.fraction(),
            0.0,
            "with nothing left to stage the bar must not read as `0 of 2` outstanding work"
        );
    }

    #[test]
    fn staged_progress_fraction_handles_zero_total_without_dividing_by_zero() {
        let progress = StagedProgress {
            staged: 0,
            total: 0,
        };
        assert_eq!(progress.fraction(), 0.0);
    }

    #[test]
    fn staged_progress_fraction_is_staged_over_total() {
        let progress = StagedProgress {
            staged: 3,
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

    fn changed_file(path: &str, add_lines: u32, del_lines: u32) -> DiffFile {
        let mut lines = Vec::new();
        for _ in 0..add_lines {
            lines.push(line(DiffLineKind::Added, "a"));
        }
        for _ in 0..del_lines {
            lines.push(line(DiffLineKind::Removed, "d"));
        }
        DiffFile {
            path: PathBuf::from(path),
            old_path: None,
            status: FileChangeStatus::Modified,
            is_binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines,
            }],
            truncated: false,
        }
    }

    #[test]
    fn staged_subset_only_keeps_files_in_the_staged_set() {
        let files = vec![
            changed_file("src/a.rs", 1, 0),
            changed_file("src/b.rs", 2, 0),
            changed_file("src/c.rs", 0, 3),
        ];
        let mut staged = HashSet::new();
        staged.insert(PathBuf::from("src/b.rs"));

        let subset = staged_subset(&files, &staged);
        assert_eq!(subset.len(), 1);
        assert_eq!(subset[0].path, PathBuf::from("src/b.rs"));
    }

    #[test]
    fn staged_subset_is_empty_when_nothing_is_staged() {
        let files = vec![changed_file("src/a.rs", 1, 0)];
        let subset = staged_subset(&files, &HashSet::new());
        assert!(subset.is_empty());
    }

    #[test]
    fn staged_diff_stats_sums_only_the_staged_files() {
        let a = changed_file("src/a.rs", 3, 1);
        let b = changed_file("src/b.rs", 2, 5);
        assert_eq!(staged_diff_stats(&[&a, &b]), (5, 6));
        assert_eq!(staged_diff_stats(&[&a]), (3, 1));
        assert_eq!(staged_diff_stats(&[]), (0, 0));
    }

    #[test]
    fn commit_button_label_is_a_bare_ghost_commit_with_nothing_staged() {
        assert_eq!(commit_button_label(0), "Commit");
    }

    #[test]
    fn commit_button_label_is_singular_for_exactly_one_staged_file() {
        assert_eq!(commit_button_label(1), "Commit 1 file");
    }

    #[test]
    fn commit_button_label_is_plural_for_more_than_one_staged_file() {
        assert_eq!(commit_button_label(2), "Commit 2 files");
        assert_eq!(commit_button_label(3), "Commit 3 files");
    }
}
