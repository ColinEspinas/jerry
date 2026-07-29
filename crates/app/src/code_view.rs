//! Pure logic for Surface C's File view (`design_handoff_jerry_ade/README.md`'s "File view"
//! subsection): reads a file off disk, detects its line-ending style, picks a language label
//! from its extension, and - for `.rs` files - produces syntax-colored spans by parsing with
//! `tree-sitter` and walking the resulting AST. Deliberately `gpui`-window-free (only
//! [`gpui::Rgba`] is used, for plain colour data), mirroring this crate's split between pure
//! logic modules and `crate::root`'s `Div` construction.
//!
//! Only `.rs` files get syntax spans; other extensions render as plain monospace text. A second
//! grammar (`tree-sitter-toml`, ...) would just repeat [`highlight_rust`]'s parse-then-walk
//! shape, left for a later phase.
//!
//! ## `tree-sitter` API usage
//!
//! `tree_sitter::Parser::new()`, `set_language`, `Node::walk()`/`TreeCursor::goto_first_child`/
//! `goto_next_sibling`, `Parser::parse`/`Tree::root_node`, and `TreeCursor::field_name` are all
//! used below in their ordinary, documented shapes. Verified against
//! `vendor/zed/crates/language/src/language.rs:135,1376,1673` and
//! `vendor/zed/crates/language/src/outline.rs:102` (same `tree-sitter`/`tree-sitter-rust`
//! version pair as this crate's `Cargo.toml`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::{Rgba, SharedString};

use crate::changes;
use crate::theme;
use wt_core::diff::{DiffFile, DiffLineKind};

/// Cap on how many bytes of a file [`load_file`] will actually read and highlight, matching
/// `wt_core::diff`'s `MAX_DIFF_OUTPUT_BYTES` (`crates/wt-core/src/diff.rs:87`) so both caps stay
/// consistent rather than picking a second, arbitrary number.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Which of Surface C's two views is showing - `design_handoff_jerry_ade/README.md`'s
/// `code_view` state field (`Diff | File`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeView {
    #[default]
    Diff,
    File,
}

/// A file's detected line-ending style, read directly from its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }
}

/// Detects `bytes`' line-ending style from the byte immediately before its first `\n` - `Crlf`
/// if that byte is `\r`, `Lf` otherwise (including a file with no newline at all).
pub fn detect_line_ending(bytes: &[u8]) -> LineEnding {
    if let Some(newline_index) = bytes.iter().position(|&byte| byte == b'\n') {
        if newline_index > 0 && bytes[newline_index - 1] == b'\r' {
            return LineEnding::Crlf;
        }
    }
    LineEnding::Lf
}

/// How many of [`ParsedFile::lines`]' leading lines [`detect_indent_width`] scans before giving
/// up - bounded the same way `wt_core::diff::MAX_HUNK_LINES_PER_FILE`-style caps are, so a huge
/// file with no early space-indented line doesn't make this walk the whole thing.
const INDENT_DETECTION_LINE_SCAN_CAP: usize = 200;

/// A simple, honest heuristic for the file's real indent width, for the status bar's `N spaces`
/// item: scans up to [`INDENT_DETECTION_LINE_SCAN_CAP`] lines, tallies the leading-space count of
/// every line that starts with one or more spaces followed by real (non-whitespace-only)
/// content, and returns the *modal* (most common) count among them - not just the first one
/// found. Tab-indented lines are skipped entirely (a tab-indented file has no single "N spaces"
/// answer to report); a whitespace-only line is skipped too (it says nothing about the file's
/// real indent unit). Ties are broken by the smaller width, since a real single-width indent unit
/// is the more common convention than an arbitrary larger one.
///
/// Taking the modal count rather than the first match matters for two real, unremarkable file
/// shapes: a C-style block-comment header (` * Copyright ...`, a single leading space) and a
/// one-off hanging-indent continuation line, either of which would otherwise get naively picked
/// as "the" indent width just for appearing before any of the file's real, repeated body indent.
///
/// `None` if no space-indented line is found within the scanned window - an honestly-omitted
/// value, not a fabricated default like `4`.
pub fn detect_indent_width(lines: &[RenderedLine]) -> Option<usize> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for line in lines.iter().take(INDENT_DETECTION_LINE_SCAN_CAP) {
        let text = line.text.as_str();
        if text.starts_with('\t') {
            continue;
        }
        let leading_spaces = text.chars().take_while(|&ch| ch == ' ').count();
        if leading_spaces > 0 && leading_spaces < text.len() {
            *counts.entry(leading_spaces).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|(width_a, count_a), (width_b, count_b)| {
            count_a.cmp(count_b).then(width_b.cmp(width_a))
        })
        .map(|(width, _)| width)
}

/// The status bar's language label, derived from `path`'s extension - the same
/// `.rs`/`.toml`/`.md`/`.sql` set `crate::file_tree::lang_chip_for_name` recognizes
/// (case-insensitive), plus a generic fallback for anything else.
pub fn language_name_for_extension(extension: Option<&str>) -> &'static str {
    match extension.map(|ext| ext.to_ascii_lowercase()).as_deref() {
        Some("rs") => "Rust",
        Some("toml") => "TOML",
        Some("md") => "Markdown",
        Some("sql") => "SQL",
        _ => "Plain Text",
    }
}

/// A syntax span's classification - `design_handoff_jerry_ade/README.md`'s File view
/// syntax-colour table ("keyword ... function ... type ... literal/self ... comment ...
/// punctuation/text").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    Keyword,
    Function,
    Type,
    Literal,
    Comment,
    Text,
}

/// Maps a [`HighlightKind`] to its real `theme::syntax::*` colour, per
/// `design_handoff_jerry_ade/README.md`'s File view table.
pub fn color_for_kind(kind: HighlightKind) -> Rgba {
    match kind {
        HighlightKind::Keyword => theme::syntax::KEYWORD,
        HighlightKind::Function => theme::syntax::FUNCTION,
        HighlightKind::Type => theme::syntax::TYPE,
        HighlightKind::Literal => theme::syntax::LITERAL,
        HighlightKind::Comment => theme::syntax::COMMENT,
        HighlightKind::Text => theme::syntax::TEXT,
    }
}

/// One classified leaf token from a `tree-sitter` parse - byte offsets into the whole-file
/// source [`highlight_rust`] parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
}

/// Rust keyword tokens - tree-sitter-rust's grammar represents each as an unnamed leaf node
/// whose `kind()` is the literal keyword text (see this module's tests).
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "super", "trait", "type", "union", "unsafe", "use", "where",
    "while", "yield",
];

const LITERAL_KINDS: &[&str] = &[
    "string_literal",
    "raw_string_literal",
    "char_literal",
    "integer_literal",
    "float_literal",
    "boolean_literal",
];

const COMMENT_KINDS: &[&str] = &["line_comment", "block_comment"];

const TYPE_KINDS: &[&str] = &["type_identifier", "primitive_type"];

/// Parses `source` with `tree-sitter-rust` and walks the resulting AST into classified
/// [`HighlightSpan`]s. Returns an empty `Vec` (rather than panicking) if the grammar fails to
/// load or the parse produces no tree - neither expected in practice, but not assumed away.
pub fn highlight_rust(source: &str) -> Vec<HighlightSpan> {
    let mut parser = tree_sitter::Parser::new();
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    walk_node(tree.root_node(), None, &mut spans);
    spans
}

fn walk_node(
    node: tree_sitter::Node<'_>,
    field_name: Option<&str>,
    spans: &mut Vec<HighlightSpan>,
) {
    let kind = node.kind();

    if COMMENT_KINDS.contains(&kind) {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: HighlightKind::Comment,
        });
        return;
    }
    if LITERAL_KINDS.contains(&kind) || kind == "self" {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: HighlightKind::Literal,
        });
        return;
    }
    if TYPE_KINDS.contains(&kind) {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: HighlightKind::Type,
        });
        return;
    }

    if node.child_count() == 0 {
        let classified = if RUST_KEYWORDS.contains(&kind) {
            HighlightKind::Keyword
        } else if kind == "identifier" && matches!(field_name, Some("name") | Some("function")) {
            // A `function_item`'s `name` field, or a `call_expression`'s `function` field when
            // the callee is a plain identifier (`foo()`, not `obj.foo()`, which uses
            // `field_identifier` instead - out of scope here).
            HighlightKind::Function
        } else {
            HighlightKind::Text
        };
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: classified,
        });
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let child_field = cursor.field_name();
            walk_node(child, child_field, spans);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// One already-highlighted display line: its text (never including line-ending bytes) plus a
/// gapless run list covering every byte of it (unhighlighted stretches are explicit
/// [`HighlightKind::Text`] runs) - computed once by [`build_lines`] and cached in [`ParsedFile`],
/// never recomputed per render.
///
/// Each run's text is a pre-allocated [`SharedString`], not a byte [`Range`] re-sliced on every
/// render - `Arc`-backed, so cloning it at render time is cheap, avoiding a per-frame allocation
/// per run per visible row.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedLine {
    pub text: String,
    pub runs: Vec<(SharedString, HighlightKind)>,
}

/// Splits `source`'s line boundaries (LF or CRLF alike - the trailing `\r`, if any, is excluded)
/// into byte ranges, then clips `spans` against each one to build a gapless [`RenderedLine`]
/// list. A trailing range past the last `\n` is always included (a file with no trailing
/// newline), and an empty `source` still yields one empty line, matching how an editor shows an
/// empty file.
fn build_lines(source: &str, spans: &[HighlightSpan]) -> Vec<RenderedLine> {
    let line_ranges = line_ranges(source);

    let mut sorted_spans = spans.to_vec();
    sorted_spans.sort_by_key(|span| span.start);

    let mut lines = Vec::with_capacity(line_ranges.len());
    let mut span_index = 0usize;

    for range in &line_ranges {
        while span_index < sorted_spans.len() && sorted_spans[span_index].end <= range.start {
            span_index += 1;
        }

        let mut runs: Vec<(Range<usize>, HighlightKind)> = Vec::new();
        let mut cursor = range.start;
        let mut index = span_index;
        while index < sorted_spans.len() && sorted_spans[index].start < range.end {
            let span = sorted_spans[index];
            let clipped_start = span.start.max(range.start);
            let clipped_end = span.end.min(range.end);
            if clipped_start > cursor {
                runs.push((
                    cursor - range.start..clipped_start - range.start,
                    HighlightKind::Text,
                ));
            }
            if clipped_end > clipped_start {
                runs.push((
                    clipped_start - range.start..clipped_end - range.start,
                    span.kind,
                ));
                cursor = clipped_end;
            }
            index += 1;
        }
        if cursor < range.end {
            runs.push((
                cursor - range.start..range.end - range.start,
                HighlightKind::Text,
            ));
        }

        let line_text = source[range.clone()].to_string();
        // Sliced from `line_text` (relative to the line's own start) once here, not re-sliced by
        // `crate::root::render_file_view_line` on every render - see `RenderedLine`'s docs.
        let owned_runs = runs
            .into_iter()
            .map(|(relative_range, kind)| (SharedString::new(&line_text[relative_range]), kind))
            .collect();

        lines.push(RenderedLine {
            text: line_text,
            runs: owned_runs,
        });
    }

    lines
}

fn line_ranges(source: &str) -> Vec<Range<usize>> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut line_start = 0usize;

    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            let mut line_end = index;
            if line_end > line_start && bytes[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            ranges.push(line_start..line_end);
            line_start = index + 1;
        }
    }
    ranges.push(line_start..bytes.len());
    ranges
}

/// A file's parsed-and-highlighted content, cached in `crate::root::AdeApp::file_view_cache` so
/// [`load_file`]/[`highlight_rust`] run at most once per file-content change - see
/// [`cache_is_fresh`] for the staleness check.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    pub len: u64,
    pub language: &'static str,
    pub line_ending: LineEnding,
    /// `true` if the file on disk was larger than [`MAX_FILE_BYTES`] and this is only a prefix
    /// of it (cut back to the last line boundary within the cap - see [`load_file`]).
    pub truncated: bool,
    /// `true` if the file's real raw bytes (up to the [`MAX_FILE_BYTES`] cap) were genuinely
    /// valid UTF-8 - a real `std::str::from_utf8` check at load time, not assumed. `false` means
    /// [`load_file`]'s `String::from_utf8_lossy` decode silently replaced at least one invalid
    /// byte sequence with a `U+FFFD` replacement character (a Latin-1/UTF-16/binary-ish file),
    /// so what's on screen is no longer a faithful rendering of the file's real bytes. The status
    /// bar's `UTF-8` label reads this rather than assuming every loaded file is what it claims to
    /// be.
    pub is_valid_utf8: bool,
    pub lines: Vec<RenderedLine>,
}

/// Reads a file from disk, caps it at [`MAX_FILE_BYTES`], detects its line-ending style and
/// language, and - for a `.rs` file - runs it through [`highlight_rust`]. The `io::Error` is
/// propagated rather than swallowed; the caller renders it as an honest error message.
pub fn load_file(path: &Path) -> io::Result<ParsedFile> {
    let metadata = fs::metadata(path)?;
    let len = metadata.len();
    let mtime = metadata.modified().ok();

    let mut bytes = fs::read(path)?;
    let truncated = bytes.len() > MAX_FILE_BYTES;
    if truncated {
        bytes.truncate(MAX_FILE_BYTES);
        if let Some(last_newline) = bytes.iter().rposition(|&byte| byte == b'\n') {
            bytes.truncate(last_newline + 1);
        }
    }

    let line_ending = detect_line_ending(&bytes);
    // Checked before the lossy decode below (which borrows `bytes`, so this doesn't need to run
    // first, but conceptually it's answering "were the real bytes valid" before they get
    // silently repaired) - a real, cheap validity check, not a hardcoded assumption.
    let is_valid_utf8 = std::str::from_utf8(&bytes).is_ok();
    let source = String::from_utf8_lossy(&bytes).into_owned();

    let extension = path.extension().and_then(|ext| ext.to_str());
    let language = language_name_for_extension(extension);
    let is_rust = extension
        .map(|ext| ext.eq_ignore_ascii_case("rs"))
        .unwrap_or(false);
    let spans = if is_rust {
        highlight_rust(&source)
    } else {
        Vec::new()
    };
    let lines = build_lines(&source, &spans);

    Ok(ParsedFile {
        path: path.to_path_buf(),
        mtime,
        len,
        language,
        line_ending,
        truncated,
        is_valid_utf8,
        lines,
    })
}

/// Whether `cached` is still an up-to-date parse of `path` - true iff the path matches and both
/// the freshly-read `mtime`/`len` are unchanged from what produced `cached`. Used by
/// `crate::root::AdeApp::render_file_view` to decide whether to reuse `cached` or call
/// [`load_file`] again.
pub fn cache_is_fresh(
    cached: &ParsedFile,
    path: &Path,
    mtime: Option<SystemTime>,
    len: u64,
) -> bool {
    cached.path == path && cached.mtime == mtime && cached.len == len
}

/// The File view breadcrumb's path segments (`design_handoff_jerry_ade/README.md`: "`src ›
/// db › query_builder.rs`") - every `Normal` path component of `path`, in order. Root/prefix/
/// `.`/`..` components are skipped.
pub fn breadcrumb_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// The File view's 3px git-gutter marker set (`design_handoff_jerry_ade/README.md`: "a 3px git
/// gutter (`#2c6244` for agent-touched lines, transparent otherwise)") - the new-file line
/// numbers (1-indexed) a hunk actually *added*, derived from `file`'s hunks via
/// `crate::changes::parse_hunk_new_range`. Context lines advance the new-file line counter
/// without being marked; removed lines don't exist in the new file, so they never advance it.
pub fn changed_line_set(file: &DiffFile) -> HashSet<usize> {
    let mut changed = HashSet::new();
    for hunk in &file.hunks {
        let Some((mut new_line, _)) = changes::parse_hunk_new_range(&hunk.header) else {
            continue;
        };
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Added => {
                    changed.insert(new_line);
                    new_line += 1;
                }
                DiffLineKind::Context => {
                    new_line += 1;
                }
                DiffLineKind::Removed => {}
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wt_core::diff::{DiffHunk, DiffLine, FileChangeStatus};

    #[test]
    fn detects_lf_from_real_bytes() {
        assert_eq!(detect_line_ending(b"fn main() {\n}\n"), LineEnding::Lf);
    }

    #[test]
    fn detects_crlf_from_real_bytes() {
        assert_eq!(
            detect_line_ending(b"fn main() {\r\n}\r\n"),
            LineEnding::Crlf
        );
    }

    #[test]
    fn a_file_with_no_newline_at_all_defaults_to_lf() {
        assert_eq!(detect_line_ending(b"no newline here"), LineEnding::Lf);
    }

    fn plain_line(text: &str) -> RenderedLine {
        RenderedLine {
            text: text.to_string(),
            runs: Vec::new(),
        }
    }

    #[test]
    fn detect_indent_width_finds_the_first_real_space_indented_line() {
        let lines: Vec<RenderedLine> = ["fn main() {", "    let x = 1;", "}"]
            .iter()
            .map(|text| plain_line(text))
            .collect();
        assert_eq!(detect_indent_width(&lines), Some(4));
    }

    #[test]
    fn detect_indent_width_skips_tab_indented_lines() {
        let lines: Vec<RenderedLine> = ["fn main() {", "\tlet x = 1;", "  let y = 2;"]
            .iter()
            .map(|text| plain_line(text))
            .collect();
        assert_eq!(
            detect_indent_width(&lines),
            Some(2),
            "a tab-indented line has no single \"N spaces\" answer - keep scanning for a real \
             space-indented one"
        );
    }

    #[test]
    fn detect_indent_width_skips_whitespace_only_lines() {
        let lines: Vec<RenderedLine> = ["fn main() {", "    ", "      let x = 1;"]
            .iter()
            .map(|text| plain_line(text))
            .collect();
        assert_eq!(
            detect_indent_width(&lines),
            Some(6),
            "a blank/whitespace-only line says nothing about the real indent unit"
        );
    }

    #[test]
    fn detect_indent_width_with_no_indentation_anywhere_is_none() {
        let lines: Vec<RenderedLine> = ["fn main() {", "}"]
            .iter()
            .map(|text| plain_line(text))
            .collect();
        assert_eq!(detect_indent_width(&lines), None);
    }

    #[test]
    fn detect_indent_width_on_an_empty_file_is_none() {
        assert_eq!(detect_indent_width(&[]), None);
    }

    /// The audit's exact reproduction: a C-style block-comment header's single leading space
    /// must not be naively picked as "the" indent width just for appearing before the file's
    /// real, repeated 4-space body indent.
    #[test]
    fn detect_indent_width_is_not_fooled_by_a_single_block_comment_header_line() {
        let lines: Vec<RenderedLine> = [
            "/**",
            " * Copyright 2024 Example Corp.",
            " */",
            "fn main() {",
            "    let x = 1;",
            "    let y = 2;",
            "    let z = 3;",
            "}",
        ]
        .iter()
        .map(|text| plain_line(text))
        .collect();
        assert_eq!(
            detect_indent_width(&lines),
            Some(4),
            "the real, repeated 4-space body indent should win over a one-off single leading \
             space from a C-style block comment header"
        );
    }

    /// The audit's other exact reproduction: a one-off hanging-indent continuation line must not
    /// out-vote the file's real, repeated indent unit.
    #[test]
    fn detect_indent_width_is_not_fooled_by_a_single_hanging_indent_continuation_line() {
        let lines: Vec<RenderedLine> = [
            "let long_name =",
            "  some_call(a, b);",
            "fn main() {",
            "    let x = 1;",
            "    let y = 2;",
            "}",
        ]
        .iter()
        .map(|text| plain_line(text))
        .collect();
        assert_eq!(
            detect_indent_width(&lines),
            Some(4),
            "a one-off 2-space hanging-indent continuation line must not out-vote the file's \
             real, repeated 4-space indent"
        );
    }

    #[test]
    fn detect_indent_width_breaks_a_tie_toward_the_smaller_width() {
        let lines: Vec<RenderedLine> = ["  two spaces", "    four spaces"]
            .iter()
            .map(|text| plain_line(text))
            .collect();
        assert_eq!(
            detect_indent_width(&lines),
            Some(2),
            "an exact tie in occurrence count should prefer the smaller, more conventional width"
        );
    }

    #[test]
    fn language_name_covers_every_documented_extension() {
        assert_eq!(language_name_for_extension(Some("rs")), "Rust");
        assert_eq!(language_name_for_extension(Some("RS")), "Rust");
        assert_eq!(language_name_for_extension(Some("toml")), "TOML");
        assert_eq!(language_name_for_extension(Some("md")), "Markdown");
        assert_eq!(language_name_for_extension(Some("sql")), "SQL");
        assert_eq!(language_name_for_extension(Some("png")), "Plain Text");
        assert_eq!(language_name_for_extension(None), "Plain Text");
    }

    fn find_span<'a>(
        spans: &'a [HighlightSpan],
        source: &str,
        text: &str,
    ) -> Option<&'a HighlightSpan> {
        let start = source.find(text)?;
        let end = start + text.len();
        spans
            .iter()
            .find(|span| span.start == start && span.end == end)
    }

    const SAMPLE_RUST: &str =
        "/// Adds one.\nfn add(left: i32) -> i32 {\n    let name = \"x\";\n    left + 1\n}\n";

    #[test]
    fn fn_keyword_is_classified_as_keyword() {
        let spans = highlight_rust(SAMPLE_RUST);
        let span = find_span(&spans, SAMPLE_RUST, "fn").expect("fn span");
        assert_eq!(span.kind, HighlightKind::Keyword);
    }

    #[test]
    fn a_string_literal_is_classified_as_literal() {
        let spans = highlight_rust(SAMPLE_RUST);
        let span = find_span(&spans, SAMPLE_RUST, "\"x\"").expect("string literal span");
        assert_eq!(span.kind, HighlightKind::Literal);
    }

    #[test]
    fn a_function_name_is_classified_as_function() {
        let spans = highlight_rust(SAMPLE_RUST);
        let span = find_span(&spans, SAMPLE_RUST, "add").expect("function name span");
        assert_eq!(span.kind, HighlightKind::Function);
    }

    #[test]
    fn a_type_identifier_is_classified_as_type() {
        let spans = highlight_rust(SAMPLE_RUST);
        // "i32" appears twice (parameter type, return type); just confirm at least one
        // occurrence was classified as Type.
        let type_spans: Vec<_> = spans
            .iter()
            .filter(|span| SAMPLE_RUST[span.start..span.end] == *"i32")
            .collect();
        assert!(!type_spans.is_empty());
        assert!(type_spans
            .iter()
            .all(|span| span.kind == HighlightKind::Type));
    }

    #[test]
    fn a_doc_comment_is_classified_as_comment() {
        let spans = highlight_rust(SAMPLE_RUST);
        // The `line_comment` node's byte range includes its trailing newline; it's treated as
        // one span rather than recursed into, since its children are just lexical pieces, not
        // separately-colourable syntax.
        let span = find_span(&spans, SAMPLE_RUST, "/// Adds one.\n").expect("doc comment span");
        assert_eq!(span.kind, HighlightKind::Comment);
    }

    #[test]
    fn self_is_classified_as_literal_not_keyword() {
        let source = "impl Foo {\n    fn bar(&self) -> i32 {\n        self.value\n    }\n}\n";
        let spans = highlight_rust(source);
        let span = find_span(&spans, source, "self").expect("self span");
        assert_eq!(span.kind, HighlightKind::Literal);
    }

    #[test]
    fn highlighting_invalid_rust_still_returns_a_real_non_empty_span_list() {
        // Tree-sitter produces a best-effort tree for malformed input rather than failing
        // outright - confirm this doesn't panic and still classifies the keyword token present.
        let spans = highlight_rust("fn (((( broken");
        assert!(spans.iter().any(|span| span.kind == HighlightKind::Keyword));
    }

    #[test]
    fn build_lines_covers_every_byte_of_every_line_with_no_gaps() {
        let source = "let x = 1;\nlet y = 2;\n";
        let spans = highlight_rust(source);
        let lines = build_lines(source, &spans);
        assert_eq!(lines.len(), 3, "two real lines plus the trailing empty one");
        for line in &lines {
            // Every run's text, concatenated in order, must reconstruct the line's text exactly
            // - no gap, overlap, or out-of-order run.
            let reconstructed: String = line.runs.iter().map(|(text, _)| text.as_ref()).collect();
            assert_eq!(reconstructed, line.text);
            assert!(
                line.runs.iter().all(|(text, _)| !text.is_empty()),
                "a real run should never be an empty string - that would be a zero-width byte \
                 range that never should have been pushed in the first place"
            );
        }
    }

    #[test]
    fn build_lines_on_a_non_rust_file_is_all_plain_text() {
        let source = "key = \"value\"\n";
        let lines = build_lines(source, &[]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].runs.len(), 1);
        assert_eq!(lines[0].runs[0].1, HighlightKind::Text);
    }

    #[test]
    fn an_empty_source_still_yields_one_empty_line() {
        let lines = build_lines("", &[]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "");
    }

    #[test]
    fn breadcrumb_segments_splits_a_real_nested_path() {
        let segments = breadcrumb_segments(Path::new("src/db/query_builder.rs"));
        assert_eq!(segments, vec!["src", "db", "query_builder.rs"]);
    }

    #[test]
    fn breadcrumb_segments_on_a_root_level_file_is_a_single_segment() {
        let segments = breadcrumb_segments(Path::new("Cargo.toml"));
        assert_eq!(segments, vec!["Cargo.toml"]);
    }

    fn hunk(header: &str, lines: Vec<(DiffLineKind, &str)>) -> DiffHunk {
        DiffHunk {
            header: header.to_string(),
            lines: lines
                .into_iter()
                .map(|(kind, text)| DiffLine {
                    kind,
                    content: text.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn changed_line_set_marks_only_real_added_new_file_lines() {
        let file = DiffFile {
            path: PathBuf::from("src/main.rs"),
            old_path: None,
            status: FileChangeStatus::Modified,
            is_binary: false,
            hunks: vec![hunk(
                "@@ -10,3 +10,4 @@",
                vec![
                    (DiffLineKind::Context, "fn main() {"),
                    (DiffLineKind::Added, "    println!(\"new\");"),
                    (DiffLineKind::Removed, "    println!(\"old\");"),
                    (DiffLineKind::Context, "}"),
                ],
            )],
            truncated: false,
        };
        // new-file line numbering starting at 10: 10 = context "fn main() {", 11 = the real
        // added line, 12 = "}" (the removed line never occupies a new-file line number).
        let changed = changed_line_set(&file);
        assert_eq!(changed, HashSet::from([11]));
    }

    #[test]
    fn changed_line_set_is_empty_for_a_file_with_no_hunks() {
        let file = DiffFile {
            path: PathBuf::from("src/main.rs"),
            old_path: None,
            status: FileChangeStatus::Renamed,
            is_binary: false,
            hunks: Vec::new(),
            truncated: false,
        };
        assert!(changed_line_set(&file).is_empty());
    }

    #[test]
    fn cache_is_fresh_requires_matching_path_mtime_and_len() {
        let cached = ParsedFile {
            path: PathBuf::from("src/main.rs"),
            mtime: None,
            len: 42,
            language: "Rust",
            line_ending: LineEnding::Lf,
            truncated: false,
            is_valid_utf8: true,
            lines: Vec::new(),
        };
        assert!(cache_is_fresh(&cached, Path::new("src/main.rs"), None, 42));
        assert!(!cache_is_fresh(
            &cached,
            Path::new("src/other.rs"),
            None,
            42
        ));
        assert!(!cache_is_fresh(&cached, Path::new("src/main.rs"), None, 43));
    }

    #[test]
    fn load_file_reads_a_real_temp_file_and_detects_its_real_properties() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("sample.rs");
        fs::write(&path, "fn main() {\r\n    let x = 1;\r\n}\r\n").expect("write");

        let parsed = load_file(&path).expect("load_file");
        assert_eq!(parsed.language, "Rust");
        assert_eq!(parsed.line_ending, LineEnding::Crlf);
        assert!(!parsed.truncated);
        assert!(
            parsed.is_valid_utf8,
            "a genuinely valid UTF-8 file must be reported as such"
        );
        assert_eq!(parsed.lines.len(), 4);
        assert_eq!(parsed.lines[0].text, "fn main() {");
        let has_keyword_run = parsed.lines[0]
            .runs
            .iter()
            .any(|(_, kind)| *kind == HighlightKind::Keyword);
        assert!(
            has_keyword_run,
            "the real \"fn\" token should be highlighted"
        );
    }

    #[test]
    fn load_file_on_a_missing_path_returns_a_real_io_error() {
        let missing = PathBuf::from("/definitely/not/a/real/path/for/ade/code-view-test.rs");
        assert!(load_file(&missing).is_err());
    }

    /// The audit's exact reproduction: a file whose real raw bytes are not valid UTF-8 (here,
    /// Latin-1-encoded, containing a byte sequence that isn't valid UTF-8 at all) must be
    /// reported as such, not silently labeled `UTF-8` after `String::from_utf8_lossy` quietly
    /// replaces the invalid bytes with `U+FFFD`.
    #[test]
    fn load_file_detects_a_real_non_utf8_file_as_lossily_decoded() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("latin1.txt");
        // 0xE9 alone (Latin-1 for "é") is not a valid UTF-8 byte sequence in this position.
        let mut bytes = b"caf".to_vec();
        bytes.push(0xE9);
        bytes.extend_from_slice(b" latin-1\n");
        fs::write(&path, &bytes).expect("write");

        let parsed = load_file(&path).expect("load_file");
        assert!(
            !parsed.is_valid_utf8,
            "a real non-UTF-8 file must not be reported as valid UTF-8"
        );
    }

    #[test]
    fn load_file_detects_a_real_ascii_file_as_valid_utf8() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ascii.txt");
        fs::write(&path, "plain ascii content\n").expect("write");

        let parsed = load_file(&path).expect("load_file");
        assert!(parsed.is_valid_utf8);
    }

    #[test]
    fn load_file_truncates_a_file_larger_than_the_cap_at_a_real_line_boundary() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("big.rs");
        let line = "let value = 1;\n";
        let mut content = String::new();
        while content.len() < MAX_FILE_BYTES + line.len() * 10 {
            content.push_str(line);
        }
        fs::write(&path, &content).expect("write");

        let parsed = load_file(&path).expect("load_file");
        assert!(parsed.truncated);
        // Every real line kept is a complete, real line - never a partial one.
        for rendered in &parsed.lines {
            assert!(rendered.text.is_empty() || rendered.text == "let value = 1;");
        }
    }
}
