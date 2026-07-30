//! Pure logic for Surface C's File view *Diagnostic* state (`design_handoff_jerry_ade/
//! README.md`'s "Language server UI" subsection): turns a `Vec<lsp_types::Diagnostic>` (as
//! published by `rust-analyzer` via `lsp_core::LspClient::diagnostics_for`) into per-line,
//! byte-range-addressed data `crate::root`'s renderer draws a dotted underline/row tint/inline
//! message/card from. Deliberately `gpui`-window-free, mirroring `crate::code_surface::code_view`'s split
//! between pure logic and `crate::root`'s `Div` construction.
//!
//! Completions and Hover (`design_handoff_jerry_ade/README.md`'s `lsp_popup` state) are out of
//! scope here - `lsp_core::LspClient`'s generic `request`/`notify` methods are equally usable
//! for those later; only this module's mapping logic is diagnostics-specific.

use std::collections::HashMap;
use std::ops::Range;

use gpui::SharedString;
use lsp_core::lsp_types;

use crate::code_surface::code_view::{HighlightKind, RenderedLine};

/// A diagnostic's severity, collapsed from `lsp_types::DiagnosticSeverity`'s four levels into
/// this app's own type so `crate::root`'s rendering code doesn't need to depend on `lsp_core`
/// for a value this simple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    /// The LSP spec leaves an omitted severity to client interpretation; this client treats it
    /// as [`Severity::Error`] - the design only defines a treatment for errors, and
    /// `rust-analyzer` always sets a severity in practice, so this is a defensive fallback.
    pub fn from_lsp(severity: Option<lsp_types::DiagnosticSeverity>) -> Self {
        match severity {
            Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Information,
            Some(lsp_types::DiagnosticSeverity::HINT) => Severity::Hint,
            _ => Severity::Error,
        }
    }

    /// Ordering from most to least severe, used as the tie-break in [`Severity::worst`] when a
    /// line carries diagnostics of more than one severity - not "whichever is first in the
    /// `Vec`", which would depend on server-side ordering.
    fn rank(self) -> u8 {
        match self {
            Severity::Error => 3,
            Severity::Warning => 2,
            Severity::Information => 1,
            Severity::Hint => 0,
        }
    }

    /// The single most severe [`Severity`] among `diagnostics` - `None` only for an empty slice.
    /// "Worst wins" is the tie-break for a line's row-level treatment (underline colour, row
    /// background tint) when it carries diagnostics of mixed severity.
    pub fn worst(diagnostics: &[LineDiagnostic]) -> Option<Severity> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.severity)
            .max_by_key(|severity| severity.rank())
    }
}

/// One diagnostic's contribution to a specific line: the UTF-8 byte range *within that line's
/// text* the dotted underline should span (see [`utf16_offset_to_byte_offset`] for the
/// position-encoding conversion this comes from), plus the message/source/code/severity to show
/// in the inline message and card.
#[derive(Debug, Clone, PartialEq)]
pub struct LineDiagnostic {
    pub byte_range: Range<usize>,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// Converts an LSP `Position`'s `character` value (a UTF-16 code-unit offset - the spec's
/// default `PositionEncodingKind::UTF16`, and the only kind this client ever negotiates) into a
/// UTF-8 byte offset within `line_text`. Clamps to `line_text`'s byte length for a `character`
/// past the line's end, per the spec ("defaults back to the line length").
fn utf16_offset_to_byte_offset(line_text: &str, utf16_offset: u32) -> usize {
    let mut utf16_count = 0u32;
    for (byte_index, ch) in line_text.char_indices() {
        if utf16_count >= utf16_offset {
            return byte_index;
        }
        utf16_count += ch.len_utf16() as u32;
    }
    line_text.len()
}

/// [`LineDiagnostic::code`]'s source value - `lsp_types::NumberOrString` is an either/or
/// (rust-analyzer sends its own error codes, e.g. `E0308`, as a string, not a number), converted
/// here to a plain `String` since this app's UI only ever displays it as text.
fn number_or_string_to_string(code: &lsp_types::NumberOrString) -> String {
    match code {
        lsp_types::NumberOrString::Number(number) => number.to_string(),
        lsp_types::NumberOrString::String(text) => text.clone(),
    }
}

/// Builds a per-line (1-based, matching `code_view`/`crate::root`'s convention) index of every
/// diagnostic that touches at least one line of `lines`. A diagnostic whose range spans multiple
/// lines is recorded on *every* line it touches, clipped to each line's own bounds (from the
/// start column on the first line, to the end column on the last, the whole line in between) -
/// visible coverage on every affected row, not only the first. A diagnostic naming a line past
/// the end of `lines` is silently skipped rather than panicking.
pub fn index_diagnostics_by_line(
    diagnostics: &[lsp_types::Diagnostic],
    lines: &[RenderedLine],
) -> HashMap<usize, Vec<LineDiagnostic>> {
    let mut by_line: HashMap<usize, Vec<LineDiagnostic>> = HashMap::new();

    for diagnostic in diagnostics {
        let start_line = diagnostic.range.start.line as usize;
        let end_line = diagnostic.range.end.line as usize;
        for line_index in start_line..=end_line {
            let Some(line) = lines.get(line_index) else {
                continue;
            };
            let start_character = if line_index == start_line {
                diagnostic.range.start.character
            } else {
                0
            };
            let is_last_line = line_index == end_line;

            let start_byte = utf16_offset_to_byte_offset(&line.text, start_character);
            let end_byte = if is_last_line {
                utf16_offset_to_byte_offset(&line.text, diagnostic.range.end.character)
            } else {
                line.text.len()
            };
            // Never hand back an inverted range. A single-point diagnostic (start == end)
            // legitimately produces a zero-width range here.
            let end_byte = end_byte.max(start_byte);

            by_line
                .entry(line_index + 1)
                .or_default()
                .push(LineDiagnostic {
                    byte_range: start_byte..end_byte,
                    severity: Severity::from_lsp(diagnostic.severity),
                    message: diagnostic.message.clone(),
                    source: diagnostic.source.clone(),
                    code: diagnostic.code.as_ref().map(number_or_string_to_string),
                });
        }
    }

    by_line
}

/// Splits `runs` (a [`RenderedLine::runs`] slice - gapless/contiguous by byte length, so each
/// run's byte range is reconstructed from a running length total) further at any
/// [`LineDiagnostic::byte_range`] boundary that falls inside a run, tagging each resulting
/// segment with whether it's diagnostic-covered - the per-segment data
/// `crate::code_surface::file_view::render_file_view_line` draws a dotted underline under. A syntax-highlight run
/// and a diagnostic range rarely share an exact boundary, so this intersection keeps the
/// underline aligned to the diagnostic's own column range rather than rounded out to the
/// nearest syntax-highlight token boundary.
///
/// Defensive against a real, live-reproduced panic: `diagnostics` is always indexed against
/// whichever content was current when the language server last published it, which - for Surface
/// C's editable File view (Revision R8.5a) - can genuinely diverge from `runs`' own text once a
/// real edit changes the line (e.g. a multi-byte character shifts a byte offset that used to be a
/// real char boundary onto one that no longer is). `crate::code_surface` gates diagnostic
/// rendering off entirely for a dirty buffer specifically to make this unreachable in practice
/// (see that module's own docs), but this function is still hardened independently, as a real,
/// deliberate second line of defense, not left to rely solely on that caller-side gate: every
/// candidate cut point is only ever added if it's a genuine char boundary within the run's own
/// text, and the final slice is looked up with [`str::get`] (never a raw index) so a boundary that
/// slips through anyway is skipped rather than panicking.
pub fn overlay_diagnostic_runs(
    runs: &[(SharedString, HighlightKind)],
    diagnostics: &[LineDiagnostic],
) -> Vec<(SharedString, HighlightKind, bool)> {
    if diagnostics.is_empty() {
        return runs
            .iter()
            .map(|(text, kind)| (text.clone(), *kind, false))
            .collect();
    }

    let mut output = Vec::new();
    let mut cursor = 0usize;
    for (text, kind) in runs {
        let run_start = cursor;
        let run_end = run_start + text.len();
        cursor = run_end;

        let mut cut_points: Vec<usize> = vec![run_start, run_end];
        for diagnostic in diagnostics {
            if diagnostic.byte_range.start > run_start
                && diagnostic.byte_range.start < run_end
                && text.is_char_boundary(diagnostic.byte_range.start - run_start)
            {
                cut_points.push(diagnostic.byte_range.start);
            }
            if diagnostic.byte_range.end > run_start
                && diagnostic.byte_range.end < run_end
                && text.is_char_boundary(diagnostic.byte_range.end - run_start)
            {
                cut_points.push(diagnostic.byte_range.end);
            }
        }
        cut_points.sort_unstable();
        cut_points.dedup();

        for window in cut_points.windows(2) {
            let (segment_start, segment_end) = (window[0], window[1]);
            if segment_start >= segment_end {
                continue;
            }
            let local_start = segment_start - run_start;
            let local_end = segment_end - run_start;
            // `get`, not a raw index: `local_start`/`local_end` are only ever real char
            // boundaries of `text` by construction above (`run_start`/`run_end` themselves are
            // always real boundaries - every `RenderedLine::runs` entry is a whole, valid `&str`
            // - and every diagnostic-derived cut point in between was already boundary-checked),
            // but this is still a genuine, independent defensive layer, not a redundant one: skip
            // a segment that somehow isn't real, sliceable text rather than risk a panic.
            let Some(segment_text) = text.as_ref().get(local_start..local_end) else {
                continue;
            };
            let is_diagnostic = diagnostics
                .iter()
                .any(|d| d.byte_range.start < segment_end && d.byte_range.end > segment_start);
            output.push((SharedString::new(segment_text), *kind, is_diagnostic));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> RenderedLine {
        RenderedLine {
            text: text.to_string(),
            runs: Vec::new(),
        }
    }

    fn diagnostic(
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
        message: &str,
    ) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: start_line,
                    character: start_char,
                },
                end: lsp_types::Position {
                    line: end_line,
                    character: end_char,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: Some(lsp_types::NumberOrString::String("E0308".to_string())),
            code_description: None,
            source: Some("rustc".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn utf16_offset_to_byte_offset_is_identity_for_pure_ascii() {
        assert_eq!(utf16_offset_to_byte_offset("let x = 1;", 4), 4);
    }

    #[test]
    fn utf16_offset_to_byte_offset_accounts_for_a_real_multi_byte_char() {
        // "café x" - 'é' is 2 UTF-8 bytes but only 1 UTF-16 code unit, so the real byte offset
        // of the 'x' that follows it must be 6 (c=1,a=1,f=1,é=2 bytes,space=1 => byte 6), even
        // though its UTF-16 offset is only 5 (c=1,a=1,f=1,é=1 unit,space=1 => unit 5).
        let text = "caf\u{e9} x"; // '\u{e9}' is 'é'
        let x_byte_offset = text.find('x').expect("'x' present");
        assert_eq!(x_byte_offset, 6);
        assert_eq!(utf16_offset_to_byte_offset(text, 5), x_byte_offset);
    }

    #[test]
    fn utf16_offset_to_byte_offset_clamps_past_the_real_line_end() {
        assert_eq!(utf16_offset_to_byte_offset("abc", 999), 3);
    }

    #[test]
    fn a_single_line_diagnostic_indexes_onto_exactly_that_line() {
        let lines = vec![
            line("fn main() {"),
            line("    let x: i32 = \"y\";"),
            line("}"),
        ];
        let diagnostics = vec![diagnostic(1, 17, 1, 20, "mismatched types")];

        let by_line = index_diagnostics_by_line(&diagnostics, &lines);

        assert_eq!(by_line.len(), 1);
        let entries = by_line
            .get(&2)
            .expect("line 2 (1-based) should have an entry");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "mismatched types");
        assert_eq!(entries[0].severity, Severity::Error);
        assert_eq!(entries[0].source.as_deref(), Some("rustc"));
        assert_eq!(entries[0].code.as_deref(), Some("E0308"));
        assert_eq!(entries[0].byte_range, 17..20);
    }

    #[test]
    fn a_multi_line_diagnostic_indexes_onto_every_real_touched_line() {
        let lines = vec![line("let x = foo("), line("    1,"), line(");")];
        let diagnostics = vec![diagnostic(0, 8, 2, 1, "mismatched arguments")];

        let by_line = index_diagnostics_by_line(&diagnostics, &lines);

        assert_eq!(
            by_line.len(),
            3,
            "all three real lines the range touches should be indexed"
        );
        // First line: from the start column to the real end of that line's own text.
        assert_eq!(by_line[&1][0].byte_range, 8..lines[0].text.len());
        // Middle line: the whole real line.
        assert_eq!(by_line[&2][0].byte_range, 0..lines[1].text.len());
        // Last line: from 0 up to the end column.
        assert_eq!(by_line[&3][0].byte_range, 0..1);
    }

    #[test]
    fn a_diagnostic_naming_a_line_past_the_end_of_the_file_is_skipped_not_a_panic() {
        let lines = vec![line("fn main() {}")];
        let diagnostics = vec![diagnostic(5, 0, 5, 3, "out of range")];

        let by_line = index_diagnostics_by_line(&diagnostics, &lines);
        assert!(by_line.is_empty());
    }

    #[test]
    fn multiple_diagnostics_on_the_same_line_both_appear() {
        let lines = vec![line("let x: i32 = bar(1, 2);")];
        let diagnostics = vec![
            diagnostic(0, 13, 0, 16, "first"),
            diagnostic(0, 18, 0, 19, "second"),
        ];

        let by_line = index_diagnostics_by_line(&diagnostics, &lines);
        assert_eq!(by_line[&1].len(), 2);
    }

    #[test]
    fn an_omitted_severity_is_treated_as_error() {
        assert_eq!(Severity::from_lsp(None), Severity::Error);
    }

    #[test]
    fn worst_severity_is_none_for_an_empty_slice() {
        assert_eq!(Severity::worst(&[]), None);
    }

    #[test]
    fn worst_severity_picks_error_over_hint_regardless_of_vec_order() {
        let mut a = line_diag(0..1);
        a.severity = Severity::Hint;
        let mut b = line_diag(0..1);
        b.severity = Severity::Error;
        // Error listed second, on purpose - the real ordering, not "first in the Vec", must win.
        assert_eq!(
            Severity::worst(&[a.clone(), b.clone()]),
            Some(Severity::Error)
        );
        // Same pair, reversed order - the answer must not depend on Vec order either way.
        assert_eq!(Severity::worst(&[b, a]), Some(Severity::Error));
    }

    #[test]
    fn worst_severity_ranks_warning_above_information_and_hint() {
        let mut hint = line_diag(0..1);
        hint.severity = Severity::Hint;
        let mut info = line_diag(0..1);
        info.severity = Severity::Information;
        let mut warning = line_diag(0..1);
        warning.severity = Severity::Warning;
        assert_eq!(
            Severity::worst(&[hint, info, warning]),
            Some(Severity::Warning)
        );
    }

    #[test]
    fn a_number_diagnostic_code_is_stringified() {
        assert_eq!(
            number_or_string_to_string(&lsp_types::NumberOrString::Number(308)),
            "308"
        );
    }

    fn line_diag(byte_range: Range<usize>) -> LineDiagnostic {
        LineDiagnostic {
            byte_range,
            severity: Severity::Error,
            message: "mismatched types".to_string(),
            source: Some("rustc".to_string()),
            code: Some("E0308".to_string()),
        }
    }

    #[test]
    fn no_diagnostics_leaves_every_run_untouched_and_unmarked() {
        let runs = vec![
            (SharedString::new("let x: i32 = "), HighlightKind::Keyword),
            (SharedString::new("\"y\""), HighlightKind::Literal),
        ];
        let overlaid = overlay_diagnostic_runs(&runs, &[]);
        assert_eq!(overlaid.len(), 2);
        assert!(overlaid.iter().all(|(_, _, marked)| !marked));
        assert_eq!(overlaid[0].0.as_ref(), "let x: i32 = ");
        assert_eq!(overlaid[1].0.as_ref(), "\"y\"");
    }

    #[test]
    fn a_diagnostic_range_inside_a_single_run_splits_it_into_three_segments() {
        // "let x: i32 = " is one 13-byte keyword run; a diagnostic covering bytes 4..5 ("x")
        // should split it into "let "(unmarked) + "x"(marked) + ": i32 = "(unmarked).
        let runs = vec![(SharedString::new("let x: i32 = "), HighlightKind::Keyword)];
        let diagnostics = vec![line_diag(4..5)];

        let overlaid = overlay_diagnostic_runs(&runs, &diagnostics);

        assert_eq!(overlaid.len(), 3);
        assert_eq!(
            overlaid[0],
            (SharedString::new("let "), HighlightKind::Keyword, false)
        );
        assert_eq!(
            overlaid[1],
            (SharedString::new("x"), HighlightKind::Keyword, true)
        );
        assert_eq!(
            overlaid[2],
            (SharedString::new(": i32 = "), HighlightKind::Keyword, false)
        );

        let reconstructed: String = overlaid.iter().map(|(text, _, _)| text.as_ref()).collect();
        assert_eq!(reconstructed, "let x: i32 = ");
    }

    #[test]
    fn a_diagnostic_range_spanning_two_runs_marks_the_tail_of_one_and_head_of_the_next() {
        let runs = vec![
            (SharedString::new("let "), HighlightKind::Keyword), // bytes 0..4
            (SharedString::new("x"), HighlightKind::Text),       // bytes 4..5
            (SharedString::new(": i32"), HighlightKind::Type),   // bytes 5..10
        ];
        // "let x: i32" - byte 3 is the space after "let", byte 6 is the space after ":", so
        // 3..7 covers " x: " (space, x, colon, space).
        let diagnostics = vec![line_diag(3..7)];

        let overlaid = overlay_diagnostic_runs(&runs, &diagnostics);

        let reconstructed: String = overlaid.iter().map(|(text, _, _)| text.as_ref()).collect();
        assert_eq!(reconstructed, "let x: i32");

        let marked: String = overlaid
            .iter()
            .filter(|(_, _, marked)| *marked)
            .map(|(text, _, _)| text.as_ref())
            .collect();
        assert_eq!(marked, " x: ");
    }

    /// HIGH regression coverage (finding 3): the real, live-reproduced panic an audit caught -
    /// stale diagnostics indexed against last-*saved* content get sliced against the *edited*
    /// buffer's real text once the File view (Revision R8.5a) makes that divergence possible.
    /// "caf\u{e9}" - '\u{e9}' ("\u{e9}", i.e. é) is a real 2-byte UTF-8 character starting at byte
    /// 3; byte 4 falls strictly inside it. A diagnostic computed against different, stale content
    /// can end up with exactly this shape once a real edit shifts/changes the line's bytes -
    /// `overlay_diagnostic_runs` must never index directly into that offset.
    #[test]
    fn a_diagnostic_byte_range_landing_mid_character_after_a_real_edit_does_not_panic() {
        let runs = vec![(SharedString::new("caf\u{e9}"), HighlightKind::Text)];
        // end (4) is not a real char boundary in "caf\u{e9}".
        let diagnostics = vec![line_diag(2..4)];

        let overlaid = overlay_diagnostic_runs(&runs, &diagnostics); // must not panic

        let reconstructed: String = overlaid.iter().map(|(text, _, _)| text.as_ref()).collect();
        assert_eq!(
            reconstructed, "caf\u{e9}",
            "no real bytes should be lost or duplicated even when a diagnostic's own byte range \
             can no longer be sliced exactly"
        );
    }

    /// The mirror case: the diagnostic's own *start* (not end) lands mid-character.
    #[test]
    fn a_diagnostic_byte_range_starting_mid_character_after_a_real_edit_does_not_panic() {
        let runs = vec![(SharedString::new("caf\u{e9} x"), HighlightKind::Text)];
        // start (4) is not a real char boundary in "caf\u{e9} x".
        let diagnostics = vec![line_diag(4..6)];

        let overlaid = overlay_diagnostic_runs(&runs, &diagnostics); // must not panic

        let reconstructed: String = overlaid.iter().map(|(text, _, _)| text.as_ref()).collect();
        assert_eq!(reconstructed, "caf\u{e9} x");
    }
}
