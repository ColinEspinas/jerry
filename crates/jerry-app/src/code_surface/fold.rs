//! GitHub issue #202 ("Collapse code blocks"): which regions of one open file *can* be
//! collapsed, and what the File view's row list looks like once some of them are.

use super::code_view::{HighlightKind, RenderedLine};
use std::collections::HashSet;
use std::ops::Range;

/// The three bracket shapes folding tracks, mirroring `code_view::TRACKED_BRACKET_PAIRS` (which
/// is private to that module) so a foldable region and a rainbow-coloured bracket pair always
/// mean the same three shapes. Angle brackets are deliberately absent for the same reason that
/// pass gives: `<`/`>` are far more often comparison operators than a real pair.
const TRACKED_BRACKET_PAIRS: [(u8, u8); 3] = [(b'(', b')'), (b'[', b']'), (b'{', b'}')];

fn closer_for(opener: u8) -> Option<u8> {
    TRACKED_BRACKET_PAIRS
        .iter()
        .find_map(|&(open, close)| (open == opener).then_some(close))
}

fn is_tracked_closer(byte: u8) -> bool {
    TRACKED_BRACKET_PAIRS
        .iter()
        .any(|&(_, close)| close == byte)
}

/// Whether a run of this classification can contain a bracket that really opens or closes a
/// block. String and comment bodies cannot - a `{` in `"a { b"` or in `// close the { here` is
/// literal text, and pairing it up would produce fold regions that span nonsense.
fn kind_can_hold_brackets(kind: HighlightKind) -> bool {
    !matches!(
        kind,
        HighlightKind::String
            | HighlightKind::StringEscape
            | HighlightKind::Comment
            | HighlightKind::CommentDoc
            | HighlightKind::CommentDocTag
    )
}

/// One region the user can collapse: the line carrying an opening bracket, and the line carrying
/// its real matching closer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
}

impl FoldRange {
    /// The half-open range of 0-based line indices this region hides when it is folded.
    pub fn hidden_lines(&self) -> Range<usize> {
        self.start_line + 1..self.end_line + 1
    }

    /// How many real lines disappear when this region is folded - what the `⋯ N lines` marker
    /// reports, so the number on screen is always the real count rather than an estimate.
    pub fn hidden_count(&self) -> usize {
        self.end_line.saturating_sub(self.start_line)
    }
}

/// Every collapsible region in `lines`, sorted by [`FoldRange::start_line`], at most one per
/// start line.
pub fn foldable_ranges(lines: &[RenderedLine]) -> Vec<FoldRange> {
    // (expected closer, 0-based index of the line the opener is on).
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();

    for (line_index, line) in lines.iter().enumerate() {
        for (text, kind) in &line.runs {
            if !kind_can_hold_brackets(*kind) {
                continue;
            }
            // Byte-wise, not `chars()`: every tracked bracket is ASCII, and an ASCII byte can
            // never occur inside a multi-byte UTF-8 sequence, so this is exactly as correct as
            // decoding and materially cheaper on the whole-file scan this does.
            for &byte in text.as_bytes() {
                if let Some(closer) = closer_for(byte) {
                    stack.push((closer, line_index));
                } else if is_tracked_closer(byte) {
                    if let Some(&(expected, opener_line)) = stack.last() {
                        if expected == byte {
                            stack.pop();
                            if line_index > opener_line {
                                pairs.push((opener_line, line_index));
                            }
                        }
                    }
                }
            }
        }
    }

    pairs.sort_unstable();
    let mut ranges: Vec<FoldRange> = Vec::new();
    for (start_line, end_line) in pairs {
        match ranges.last_mut() {
            Some(last) if last.start_line == start_line => {
                last.end_line = last.end_line.max(end_line);
            }
            _ => ranges.push(FoldRange {
                start_line,
                end_line,
            }),
        }
    }
    ranges
}

/// The region starting exactly at `line`, if `line` carries a fold chevron at all - a binary
/// search over [`foldable_ranges`]' own already-sorted output, so the per-row lookup the File
/// view does for every visible row costs `O(log n)` rather than a scan.
pub fn range_starting_at(ranges: &[FoldRange], line: usize) -> Option<FoldRange> {
    ranges
        .binary_search_by_key(&line, |range| range.start_line)
        .ok()
        .map(|index| ranges[index])
}

/// The translation between the File view `uniform_list`'s own **visual row** indices and the
/// buffer's **line** indices, for one file, as of one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldMap {
    total_lines: usize,
    /// Merged, sorted, disjoint, non-adjacent half-open ranges of hidden 0-based line indices.
    hidden: Vec<Range<usize>>,
    /// The first visible line of each contiguous run of visible lines; `hidden.len() + 1` entries,
    /// strictly increasing.
    segment_start_line: Vec<usize>,
    /// The visual row that same segment starts at; index-aligned with `segment_start_line`.
    segment_start_row: Vec<usize>,
    visible_rows: usize,
}

impl FoldMap {
    /// The identity map: nothing folded, so visual row `n` is buffer line `n`. This is the
    /// overwhelmingly common case and every method below short-circuits on it, so an unfolded
    /// file pays no measurable cost for folding existing.
    pub fn unfolded(total_lines: usize) -> Self {
        Self::from_hidden(total_lines, Vec::new())
    }

    /// The map for `folded_starts` (0-based [`FoldRange::start_line`]s the user has actually
    /// collapsed) against `ranges`, this buffer's currently-detected regions.
    pub fn new(total_lines: usize, ranges: &[FoldRange], folded_starts: &HashSet<usize>) -> Self {
        if folded_starts.is_empty() {
            return Self::unfolded(total_lines);
        }
        let mut hidden: Vec<Range<usize>> = ranges
            .iter()
            .filter(|range| folded_starts.contains(&range.start_line))
            .filter_map(|range| {
                let hidden = range.hidden_lines();
                let end = hidden.end.min(total_lines);
                (hidden.start < end).then_some(hidden.start..end)
            })
            .collect();
        hidden.sort_unstable_by_key(|range| range.start);

        // Merge overlapping *and* touching ranges (nested folds always overlap; two sibling
        // folds can end up exactly adjacent). Merging the touching case too is what guarantees
        // every segment below is non-empty, which `segment_start_line` being strictly increasing
        // - and therefore the binary searches - relies on.
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(hidden.len());
        for range in hidden {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        Self::from_hidden(total_lines, merged)
    }

    fn from_hidden(total_lines: usize, hidden: Vec<Range<usize>>) -> Self {
        let mut segment_start_line = Vec::with_capacity(hidden.len() + 1);
        let mut segment_start_row = Vec::with_capacity(hidden.len() + 1);
        segment_start_line.push(0);
        segment_start_row.push(0);
        let mut hidden_so_far = 0usize;
        for range in &hidden {
            hidden_so_far += range.end - range.start;
            segment_start_line.push(range.end);
            segment_start_row.push(range.end - hidden_so_far);
        }
        let visible_rows = total_lines.saturating_sub(hidden_so_far);
        Self {
            total_lines,
            hidden,
            segment_start_line,
            segment_start_row,
            visible_rows,
        }
    }

    /// What `uniform_list` must be told its item count is.
    pub fn visible_row_count(&self) -> usize {
        self.visible_rows
    }

    /// `true` when nothing at all is folded - the fast path the File view's row builder takes to
    /// skip every conversion below.
    pub fn is_identity(&self) -> bool {
        self.hidden.is_empty()
    }

    /// Whether `line` (0-based) is currently hidden inside some collapsed region.
    pub fn is_hidden(&self, line: usize) -> bool {
        self.enclosing_hidden(line).is_some()
    }

    /// The 0-based buffer line visual row `row` shows.
    pub fn line_for_row(&self, row: usize) -> usize {
        let last_line = self.total_lines.saturating_sub(1);
        if self.hidden.is_empty() {
            return row.min(last_line);
        }
        let segment = self
            .segment_start_row
            .partition_point(|&start| start <= row)
            .saturating_sub(1);
        let line = self.segment_start_line[segment] + (row - self.segment_start_row[segment]);
        line.min(last_line)
    }

    /// The visual row showing 0-based buffer line `line`.
    pub fn row_for_line(&self, line: usize) -> usize {
        if self.hidden.is_empty() {
            return line.min(self.visible_rows.saturating_sub(1));
        }
        let line = match self.enclosing_hidden(line) {
            Some(hidden) => hidden.start.saturating_sub(1),
            None => line,
        };
        let segment = self
            .segment_start_line
            .partition_point(|&start| start <= line)
            .saturating_sub(1);
        let row = self.segment_start_row[segment] + (line - self.segment_start_line[segment]);
        row.min(self.visible_rows.saturating_sub(1))
    }

    fn enclosing_hidden(&self, line: usize) -> Option<Range<usize>> {
        let index = self
            .hidden
            .partition_point(|range| range.start <= line)
            .checked_sub(1)?;
        let candidate = self.hidden[index].clone();
        (line < candidate.end).then_some(candidate)
    }
}

#[cfg(test)]
mod fold_range_tests {
    use super::*;
    use crate::code_surface::code_view;

    /// Real `RenderedLine`s, built by the same real highlighter + line splitter the File view
    /// itself renders from - not hand-assembled runs, so these tests exercise the actual runs
    /// `foldable_ranges` will see in the app (including tree-sitter's own string/comment
    /// classification, which is the whole basis for this module's string-awareness).
    fn lines_of(source: &str, extension: Option<&str>) -> Vec<code_view::RenderedLine> {
        let spans = code_view::HighlightOptions::default()
            .highlight(source, code_view::highlighter_for_extension(extension));
        code_view::build_lines(source, &spans)
    }

    #[test]
    fn a_real_rust_function_body_is_one_foldable_region() {
        let source = "fn alpha() {\n    let x = 1;\n    let y = 2;\n}\n";
        let ranges = foldable_ranges(&lines_of(source, Some("rs")));
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3
            }],
            "the `{{` on line 0 closes on line 3, so folding line 0 must hide lines 1..=3"
        );
        assert_eq!(ranges[0].hidden_lines(), 1..4);
        assert_eq!(ranges[0].hidden_count(), 3);
    }

    #[test]
    fn nested_blocks_each_get_their_own_region() {
        let source = "fn alpha() {\n    if x {\n        y();\n    }\n}\n";
        let ranges = foldable_ranges(&lines_of(source, Some("rs")));
        assert_eq!(
            ranges,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 4
                },
                FoldRange {
                    start_line: 1,
                    end_line: 3
                },
            ]
        );
    }

    #[test]
    fn a_pair_that_opens_and_closes_on_one_line_is_not_foldable() {
        let source = "fn alpha() {}\nfn beta() {}\n";
        assert!(
            foldable_ranges(&lines_of(source, Some("rs"))).is_empty(),
            "there is nothing to hide, so offering a chevron would be a lie"
        );
    }

    #[test]
    fn braces_inside_a_string_literal_never_open_a_region() {
        let source = "fn alpha() {\n    let s = \"{\";\n    let t = 1;\n}\n";
        let ranges = foldable_ranges(&lines_of(source, Some("rs")));
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3
            }],
            "the `{{` inside the string must not become a second, unbalanced region"
        );
    }

    #[test]
    fn braces_inside_a_comment_never_open_a_region() {
        let source = "fn alpha() {\n    // opens a { here\n    let t = 1;\n}\n";
        let ranges = foldable_ranges(&lines_of(source, Some("rs")));
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3
            }]
        );
    }

    #[test]
    fn one_line_opening_two_brackets_yields_a_single_widest_region() {
        let source = "call({\n    a: 1,\n}\n);\n";
        let ranges = foldable_ranges(&lines_of(source, Some("ts")));
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 3
            }],
            "the `(` closing on line 3 must win over the `{{` closing on line 2: {ranges:?}"
        );
    }

    #[test]
    fn an_unmatched_opener_yields_no_region_at_all() {
        let source = "fn alpha() {\n    let x = 1;\n";
        assert!(foldable_ranges(&lines_of(source, Some("rs"))).is_empty());
    }

    #[test]
    fn a_file_with_no_grammar_still_folds_on_real_brackets() {
        let source = "outer [\n  inner\n]\n";
        let ranges = foldable_ranges(&lines_of(source, None));
        assert_eq!(
            ranges,
            vec![FoldRange {
                start_line: 0,
                end_line: 2
            }]
        );
    }

    #[test]
    fn range_starting_at_finds_only_a_real_start_line() {
        let ranges = foldable_ranges(&lines_of(
            "fn alpha() {\n    if x {\n        y();\n    }\n}\n",
            Some("rs"),
        ));
        assert_eq!(
            range_starting_at(&ranges, 1),
            Some(FoldRange {
                start_line: 1,
                end_line: 3
            })
        );
        assert_eq!(range_starting_at(&ranges, 2), None);
    }
}

#[cfg(test)]
mod fold_map_tests {
    use super::*;

    fn folded(starts: &[usize]) -> HashSet<usize> {
        starts.iter().copied().collect()
    }

    const RANGES: [FoldRange; 2] = [
        FoldRange {
            start_line: 0,
            end_line: 3,
        },
        FoldRange {
            start_line: 5,
            end_line: 8,
        },
    ];

    #[test]
    fn an_unfolded_map_is_the_identity_it_always_was() {
        let map = FoldMap::new(10, &RANGES, &folded(&[]));
        assert!(map.is_identity());
        assert_eq!(map.visible_row_count(), 10);
        for line in 0..10 {
            assert_eq!(map.line_for_row(line), line);
            assert_eq!(map.row_for_line(line), line);
        }
    }

    #[test]
    fn folding_one_region_removes_exactly_its_hidden_lines_from_the_row_count() {
        let map = FoldMap::new(10, &RANGES, &folded(&[0]));
        assert_eq!(
            map.visible_row_count(),
            7,
            "10 lines minus lines 1, 2 and 3"
        );
        assert_eq!(map.line_for_row(0), 0);
        assert_eq!(map.line_for_row(1), 4);
        assert_eq!(map.line_for_row(2), 5);
        assert_eq!(map.line_for_row(6), 9);
    }

    #[test]
    fn a_hidden_line_maps_back_to_the_row_of_the_region_that_swallowed_it() {
        let map = FoldMap::new(10, &RANGES, &folded(&[0]));
        assert!(map.is_hidden(1) && map.is_hidden(2) && map.is_hidden(3));
        assert!(!map.is_hidden(0) && !map.is_hidden(4));
        for hidden_line in 1..=3 {
            assert_eq!(
                map.row_for_line(hidden_line),
                0,
                "a scroll request for a folded-away line must land on the collapsed row"
            );
        }
        assert_eq!(map.row_for_line(4), 1);
        assert_eq!(map.row_for_line(9), 6);
    }

    #[test]
    fn two_separate_folded_regions_compose() {
        let map = FoldMap::new(10, &RANGES, &folded(&[0, 5]));
        assert_eq!(map.visible_row_count(), 4, "lines 0, 4, 5 and 9 remain");
        assert_eq!(map.line_for_row(0), 0);
        assert_eq!(map.line_for_row(1), 4);
        assert_eq!(map.line_for_row(2), 5);
        assert_eq!(map.line_for_row(3), 9);
        assert_eq!(map.row_for_line(9), 3);
    }

    #[test]
    fn folding_a_region_and_a_region_nested_inside_it_does_not_double_count() {
        let ranges = [
            FoldRange {
                start_line: 0,
                end_line: 6,
            },
            FoldRange {
                start_line: 1,
                end_line: 4,
            },
        ];
        let map = FoldMap::new(8, &ranges, &folded(&[0, 1]));
        assert_eq!(
            map.visible_row_count(),
            2,
            "lines 1..=6 are hidden once, not twice: line 0 and line 7 remain"
        );
        assert_eq!(map.line_for_row(0), 0);
        assert_eq!(map.line_for_row(1), 7);
    }

    #[test]
    fn exactly_adjacent_hidden_regions_merge_cleanly() {
        let ranges = [
            FoldRange {
                start_line: 0,
                end_line: 2,
            },
            FoldRange {
                start_line: 2,
                end_line: 4,
            },
        ];
        let map = FoldMap::new(6, &ranges, &folded(&[0, 2]));
        assert_eq!(map.visible_row_count(), 2);
        assert_eq!(map.line_for_row(0), 0);
        assert_eq!(map.line_for_row(1), 5);
        assert_eq!(map.row_for_line(3), 0);
    }

    #[test]
    fn a_folded_start_line_that_is_no_longer_a_region_hides_nothing() {
        let map = FoldMap::new(10, &RANGES, &folded(&[7]));
        assert_eq!(map.visible_row_count(), 10);
        assert!(map.is_identity());
    }

    #[test]
    fn a_region_running_past_the_end_of_the_buffer_is_clamped_not_panicked_on() {
        let ranges = [FoldRange {
            start_line: 0,
            end_line: 99,
        }];
        let map = FoldMap::new(4, &ranges, &folded(&[0]));
        assert_eq!(map.visible_row_count(), 1);
        assert_eq!(map.line_for_row(0), 0);
        assert_eq!(map.line_for_row(5), 3);
    }

    #[test]
    fn an_empty_buffer_produces_no_rows_and_no_panics() {
        let map = FoldMap::new(0, &[], &folded(&[]));
        assert_eq!(map.visible_row_count(), 0);
        assert_eq!(map.line_for_row(0), 0);
        assert_eq!(map.row_for_line(0), 0);
    }

    #[test]
    fn line_for_row_and_row_for_line_round_trip_across_every_visible_row() {
        let map = FoldMap::new(10, &RANGES, &folded(&[0, 5]));
        for row in 0..map.visible_row_count() {
            let line = map.line_for_row(row);
            assert!(!map.is_hidden(line), "row {row} resolved to a hidden line");
            assert_eq!(
                map.row_for_line(line),
                row,
                "round trip failed for row {row}"
            );
        }
    }
}
