//! Pure logic for Surface C's File view *Diagnostic* state (`design_handoff_jerry_ade/
//! README.md`'s "Language server UI" subsection): turning a real `Vec<lsp_types::Diagnostic>`
//! (as published by a real `rust-analyzer`, via `lsp_core::LspClient::diagnostics_for`) into
//! real, per-line, byte-range-addressed data `crate::root`'s renderer can draw a dotted
//! underline/row tint/inline message/card from - deliberately `gpui`-window-free, mirroring
//! `crate::code_view`'s and `crate::changes`'s own established split between pure logic and
//! `crate::root`'s actual `Div` construction.
//!
//! ## Completions/hover are explicitly out of scope here
//!
//! `design_handoff_jerry_ade/README.md`'s `lsp_popup` state also covers `Completions` and
//! `Hover` - both a later phase (H3)'s job, not this one's. Nothing in this module (or
//! `lsp_core` itself) is diagnostics-specific at the *protocol* layer - `lsp_core::LspClient`'s
//! generic `request`/`notify` methods are exactly as usable for `textDocument/completion` or
//! `textDocument/hover` later - only this module's own mapping logic is diagnostics-specific,
//! by design.

use std::collections::HashMap;
use std::ops::Range;

use gpui::SharedString;
use lsp_core::lsp_types;

use crate::code_view::{HighlightKind, RenderedLine};

/// A diagnostic's real severity, collapsed from `lsp_types::DiagnosticSeverity`'s four real
/// levels into this app's own type (kept separate from `lsp_types`'s so `crate::root`'s
/// rendering code never has to depend on `lsp_core` for a value this simple).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    /// The LSP spec: "The diagnostic's severity. Can be omitted. If omitted it is up to the
    /// client to interpret diagnostics as error, warning, info or hint." This client interprets
    /// an omitted severity as [`Severity::Error`] - the design's own Diagnostic state only
    /// defines a treatment for errors (`#e0625c` dotted underline, `#e3908b` card message), and
    /// real-world `rust-analyzer` always sets a real severity in practice, so this branch is a
    /// defensive fallback, not an expected real path.
    pub fn from_lsp(severity: Option<lsp_types::DiagnosticSeverity>) -> Self {
        match severity {
            Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Information,
            Some(lsp_types::DiagnosticSeverity::HINT) => Severity::Hint,
            _ => Severity::Error,
        }
    }

    /// Real ordering from most to least severe: `Error` > `Warning` > `Information` > `Hint` -
    /// the obvious, conventional real-editor ordering (matches e.g. how VS Code/`rustc` itself
    /// rank these), used as this app's documented tie-break (see [`Severity::worst`]) for what
    /// a *line's* single row-level treatment should be when it carries diagnostics of more than
    /// one real severity (e.g. a real `rust-analyzer` `Error` and a real secondary `Hint`
    /// annotation both touching the same line) - not left as "whichever happens to be first in
    /// the `Vec`", which would depend on nothing more meaningful than server-side ordering.
    fn rank(self) -> u8 {
        match self {
            Severity::Error => 3,
            Severity::Warning => 2,
            Severity::Information => 1,
            Severity::Hint => 0,
        }
    }

    /// The single most severe real [`Severity`] among `diagnostics` - `None` only for a genuinely
    /// empty slice. "Worst wins" is this app's documented, explicit tie-break for a line's own
    /// row-level treatment (underline colour, row background tint) when it carries diagnostics
    /// of mixed severity - see [`Severity::rank`]'s own docs for why this ordering, not some
    /// other one.
    pub fn worst(diagnostics: &[LineDiagnostic]) -> Option<Severity> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.severity)
            .max_by_key(|severity| severity.rank())
    }
}

/// One real diagnostic's contribution to one real, specific line of a file: the real UTF-8 byte
/// range *within that line's own text* the dotted underline should span (see
/// [`utf16_offset_to_byte_offset`] for the real position-encoding conversion this comes from),
/// plus the real message/source/code/severity to show in the inline message and card.
#[derive(Debug, Clone, PartialEq)]
pub struct LineDiagnostic {
    pub byte_range: Range<usize>,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// Converts an LSP `Position`'s `character` value (a UTF-16 code-unit offset - the LSP spec's
/// required-to-support default encoding, `PositionEncodingKind::UTF16`; this client never
/// negotiates a different one in `ClientCapabilities`, so a real server is only ever allowed to
/// send this kind) into a real UTF-8 byte offset within `line_text`. Clamps to `line_text`'s own
/// byte length for a `character` past the real line's end - the spec's own documented behavior
/// ("if the character value is greater than the line length it defaults back to the line
/// length"), not an out-of-bounds bug.
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

/// [`LineDiagnostic::code`]'s real source value - `lsp_types::NumberOrString` is a real,
/// genuine either/or (rust-analyzer sends its own error codes, like the real `E0308` observed
/// while writing this module's own tests, as a string, not a number - see
/// [`lsp_types::NumberOrString`]'s own docs), converted here to a plain `String` either way
/// since this app's own UI only ever displays it as text.
fn number_or_string_to_string(code: &lsp_types::NumberOrString) -> String {
    match code {
        lsp_types::NumberOrString::Number(number) => number.to_string(),
        lsp_types::NumberOrString::String(text) => text.clone(),
    }
}

/// Builds a real, per-line (1-based, matching `code_view`/`crate::root`'s own convention) index
/// of every diagnostic that touches at least one real line of `lines` (`code_view::RenderedLine`'s
/// own already-loaded text - never a second, independent file read). A diagnostic whose real LSP
/// range spans multiple lines is real-recorded on *every* line it touches, clipped to each line's
/// own bounds (the rest of the line, past the diagnostic's own start column, on every line but the
/// last; up to the diagnostic's own end column on the last) - real, visible coverage on every
/// affected row, not only the first. A diagnostic naming a line past the end of `lines` (should
/// not happen against a freshly-loaded file, but never assumed) is silently skipped rather than
/// panicking.
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
            // Defensive: never hand back an inverted range. A real single-point diagnostic
            // (start == end) legitimately produces a zero-width range here - real, not an error.
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

/// Splits `runs` (a [`RenderedLine::runs`] slice - already gapless/contiguous by byte length,
/// per that field's own docs, so each run's byte range can be reconstructed purely from a
/// running length total, with no separate byte-range field needed on `RenderedLine` itself)
/// further at any [`LineDiagnostic::byte_range`] boundary that falls inside a run, tagging each
/// resulting segment with whether it's real diagnostic-covered text - the real per-segment data
/// `crate::root::render_file_view_line` draws a dotted underline under, without needing to know
/// anything about diagnostics itself beyond "is this segment marked or not". A syntax highlight
/// run and a diagnostic range only rarely share an exact boundary, so this real intersection
/// (not just "which whole run does a diagnostic start in") is what keeps the rendered underline
/// aligned to the diagnostic's own real column range rather than rounded out to the nearest
/// syntax-highlight token boundary.
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
            if diagnostic.byte_range.start > run_start && diagnostic.byte_range.start < run_end {
                cut_points.push(diagnostic.byte_range.start);
            }
            if diagnostic.byte_range.end > run_start && diagnostic.byte_range.end < run_end {
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
            let segment_text = &text.as_ref()[local_start..local_end];
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
}
