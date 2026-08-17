//! Pure logic for Zone 3's "Changes" list (`design_handoff_jerry_ade/README.md`'s Zone 3 spec)
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

/// Git's own status letter for one file row - `A`, `M` or `D`
/// (`design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4j).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLetter {
    Added,
    Modified,
    Deleted,
}

/// The letter for a file's `FileChangeStatus`.
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
mod diff_stat_tests {
    use crate::sidebar::changes::{
        diff_file_stats, staged_diff_stats, stat_bar_segments, StatSegment,
    };
    use std::path::PathBuf;
    use wt_core::diff::{DiffFile, DiffHunk, DiffLine, DiffLineKind, FileChangeStatus};

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            content: text.to_string(),
        }
    }

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            header: "@@ -1,2 +1,3 @@".to_string(),
            lines,
        }
    }

    fn file_with(status: FileChangeStatus, hunks: Vec<DiffHunk>) -> DiffFile {
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
        let mixed = file_with(
            FileChangeStatus::Modified,
            vec![hunk(vec![
                line(DiffLineKind::Context, "ctx"),
                line(DiffLineKind::Added, "a1"),
                line(DiffLineKind::Added, "a2"),
                line(DiffLineKind::Removed, "r1"),
            ])],
        );
        assert_eq!(diff_file_stats(&mixed), (2, 1));
        assert_eq!(
            diff_file_stats(&file_with(FileChangeStatus::Renamed, Vec::new())),
            (0, 0),
            "a file with no hunks at all contributes nothing"
        );
    }

    /// The bar is always five segments, split in proportion, with the "bump a nonzero category up
    /// to one segment" rule this function documents - so a single added line among 400 removed
    /// still shows.
    #[test]
    fn the_stat_bar_is_five_segments_split_in_proportion() {
        use StatSegment::{Add, Del, Empty};
        for (add, del, expected) in [
            (0, 0, [Empty, Empty, Empty, Empty, Empty]),
            (40, 0, [Add, Add, Add, Add, Add]),
            (0, 40, [Del, Del, Del, Del, Del]),
            (1, 399, [Add, Del, Del, Del, Del]),
        ] {
            assert_eq!(stat_bar_segments(add, del), expected, "+{add} -{del}");
        }
        for (add, del) in [(1, 0), (0, 1), (1, 1), (3, 400), (400, 3), (7, 7)] {
            assert_eq!(stat_bar_segments(add, del).len(), 5, "+{add} -{del}");
        }
    }

    fn changed_file(path: &str, add_lines: u32, del_lines: u32) -> DiffFile {
        let mut lines = Vec::new();
        for _ in 0..add_lines {
            lines.push(line(DiffLineKind::Added, "a"));
        }
        for _ in 0..del_lines {
            lines.push(line(DiffLineKind::Removed, "d"));
        }
        let mut file = file_with(FileChangeStatus::Modified, vec![hunk(lines)]);
        file.path = PathBuf::from(path);
        file
    }

    #[test]
    fn staged_diff_stats_sums_only_the_staged_files() {
        let a = changed_file("src/a.rs", 3, 1);
        let b = changed_file("src/b.rs", 2, 5);
        assert_eq!(staged_diff_stats(&[&a, &b]), (5, 6));
        assert_eq!(staged_diff_stats(&[&a]), (3, 1));
        assert_eq!(staged_diff_stats(&[]), (0, 0));
    }
}

#[cfg(test)]
mod status_letter_tests {
    use crate::sidebar::changes::{status_letter, StatusLetter};
    use crate::theme;
    use wt_core::diff::FileChangeStatus;

    /// `STAGE-A-CHANGELOG.md` §4j: every status maps to a letter, including the common case. The
    /// word pills this replaced returned `None` for `Modified`, which is the exact defect §4j
    /// names - "only the exceptions were marked". A rename is a modification as far as the letter
    /// column is concerned; the row's own `moved` chip is what states the rename.
    #[test]
    fn every_file_status_gets_a_letter_including_the_common_case() {
        for (status, letter) in [
            (FileChangeStatus::Added, StatusLetter::Added),
            (FileChangeStatus::Modified, StatusLetter::Modified),
            (FileChangeStatus::Deleted, StatusLetter::Deleted),
            (FileChangeStatus::Renamed, StatusLetter::Modified),
        ] {
            assert_eq!(status_letter(status), letter, "{status:?}");
        }
    }

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
}

#[cfg(test)]
mod staging_scope_tests {
    use crate::sidebar::changes::{
        commit_button_label, is_committed_clean, stageable_count, staged_subset, StagedProgress,
    };
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use wt_core::diff::{DiffFile, DiffHunk, DiffLine, DiffLineKind, FileChangeStatus};

    fn changed_file(path: &str, add_lines: u32, del_lines: u32) -> DiffFile {
        let mut lines = Vec::new();
        for _ in 0..add_lines {
            lines.push(DiffLine {
                kind: DiffLineKind::Added,
                content: "a".to_string(),
            });
        }
        for _ in 0..del_lines {
            lines.push(DiffLine {
                kind: DiffLineKind::Removed,
                content: "d".to_string(),
            });
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

    /// `None` is "the `git status` answer hasn't landed", not "nothing is dirty" - claiming a file
    /// is committed on the strength of an absent answer is exactly the mislabelling GitHub issue
    /// #220 is about, in the other direction.
    #[test]
    fn only_a_path_absent_from_a_known_dirty_set_is_committed_clean() {
        let dirty: HashSet<PathBuf> = [PathBuf::from("src/edited.rs")].into_iter().collect();
        for (path, known_dirty, expected) in [
            ("src/committed.rs", Some(&dirty), true),
            ("src/edited.rs", Some(&dirty), false),
            ("src/anything.rs", None, false),
        ] {
            assert_eq!(
                is_committed_clean(Path::new(path), known_dirty),
                expected,
                "{path} with dirty set {known_dirty:?}"
            );
        }
    }

    #[test]
    fn stageable_count_excludes_committed_clean_files_and_falls_back_when_unknown() {
        let files = vec![
            changed_file("src/committed.rs", 4, 0),
            changed_file("src/edited.rs", 1, 1),
        ];
        let dirty: HashSet<PathBuf> = [PathBuf::from("src/edited.rs")].into_iter().collect();
        assert_eq!(stageable_count(&files, Some(&dirty)), 1);
        assert_eq!(
            stageable_count(&files, None),
            2,
            "with no `git status` answer yet, every changed file is still a candidate"
        );
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
    fn staged_progress_is_staged_over_total_and_never_divides_by_zero() {
        for (staged, total, fraction) in [(0usize, 0usize, 0.0f32), (3, 12, 0.25)] {
            let progress = StagedProgress { staged, total };
            assert!(
                (progress.fraction() - fraction).abs() < f32::EPSILON,
                "{staged} of {total}"
            );
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
        assert!(
            staged_subset(&files, &HashSet::new()).is_empty(),
            "and nothing staged is an empty subset, not the whole list"
        );
    }

    #[test]
    fn the_commit_button_names_the_real_staged_count() {
        for (staged, label) in [
            (0, "Commit"),
            (1, "Commit 1 file"),
            (2, "Commit 2 files"),
            (3, "Commit 3 files"),
        ] {
            assert_eq!(commit_button_label(staged), label, "{staged} staged");
        }
    }
}

#[cfg(test)]
mod hunk_header_tests {
    use crate::sidebar::changes::{
        fold_gap_between, hunk_line_numbers, parse_hunk_new_range, parse_hunk_old_range,
    };
    use wt_core::diff::{DiffHunk, DiffLine, DiffLineKind};

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            content: text.to_string(),
        }
    }

    /// Both sides of a hunk header, an explicit count and the `@@ -1 +1 @@` form that means one,
    /// and a header that isn't one at all.
    #[test]
    fn a_hunk_header_yields_both_ranges_or_none_at_all() {
        for (header, old, new) in [
            ("@@ -10,5 +14,9 @@ fn foo() {", Some((10, 5)), Some((14, 9))),
            ("@@ -1 +1 @@", Some((1, 1)), Some((1, 1))),
            ("not a hunk header", None, None),
        ] {
            assert_eq!(parse_hunk_old_range(header), old, "old range of {header:?}");
            assert_eq!(parse_hunk_new_range(header), new, "new range of {header:?}");
        }
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

    /// The fold marker's own number: the unchanged span between two hunks, and the two cases that
    /// have no marker to show - back-to-back hunks, and a header that couldn't be read.
    #[test]
    fn the_fold_gap_is_the_real_unchanged_span_between_two_hunks() {
        // First hunk covers new lines 10..=14 (start 10, count 5); the next starts at 40 -
        // 25 real unchanged lines sit between them.
        for (prev, next, gap) in [
            ("@@ -10,5 +10,5 @@", "@@ -30,5 +40,5 @@", Some(25)),
            ("@@ -1,5 +1,5 @@", "@@ -6,5 +6,5 @@", None),
            ("garbage", "@@ -6,5 +6,5 @@", None),
        ] {
            assert_eq!(fold_gap_between(prev, next), gap, "{prev:?} -> {next:?}");
        }
    }
}

#[cfg(test)]
mod row_label_tests {
    use crate::sidebar::changes::{
        empty_hunks_message, is_real_rename, rename_label, split_dir_name,
    };
    use std::path::{Path, PathBuf};
    use wt_core::diff::{DiffFile, FileChangeStatus};

    #[test]
    fn split_dir_name_separates_a_nested_path_and_leaves_a_root_level_one_bare() {
        assert_eq!(
            split_dir_name(Path::new("src/db/query_builder.rs")),
            ("src/db".to_string(), "query_builder.rs".to_string())
        );
        assert_eq!(
            split_dir_name(Path::new("Cargo.toml")),
            (String::new(), "Cargo.toml".to_string())
        );
    }

    /// A rename is only real when git reports an old path that genuinely differs - an absent one,
    /// or one identical to the current path (defensive: `wt_core::diff` shouldn't produce it, but
    /// never assume it away), is not a rename and gets no label.
    #[test]
    fn only_a_genuinely_different_old_path_is_a_rename() {
        for (old_path, label) in [
            (
                Some("src/old_name.rs"),
                Some("src/old_name.rs \u{2192} src/new_name.rs".to_string()),
            ),
            (None, None),
            (Some("src/new_name.rs"), None),
        ] {
            let file = DiffFile {
                path: PathBuf::from("src/new_name.rs"),
                old_path: old_path.map(PathBuf::from),
                status: FileChangeStatus::Renamed,
                is_binary: false,
                hunks: Vec::new(),
                truncated: false,
            };
            assert_eq!(is_real_rename(&file), label.is_some(), "{old_path:?}");
            assert_eq!(rename_label(&file), label, "{old_path:?}");
        }
    }

    #[test]
    fn empty_hunks_message_names_rename_only_for_a_renamed_file() {
        assert_eq!(
            empty_hunks_message(FileChangeStatus::Renamed),
            "no line changes (rename only)"
        );
        for status in [
            FileChangeStatus::Added,
            FileChangeStatus::Modified,
            FileChangeStatus::Deleted,
        ] {
            assert_eq!(empty_hunks_message(status), "no line changes", "{status:?}");
        }
    }
}
