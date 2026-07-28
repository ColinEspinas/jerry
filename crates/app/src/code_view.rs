//! Pure logic for Surface C's File view (`design_handoff_jerry_ade/README.md`'s "File view"
//! subsection): reading a real file off disk, detecting its real line-ending style, picking its
//! real language label from its extension, and - for `.rs` files - producing real syntax-colored
//! spans by parsing the file with a real `tree-sitter` grammar and walking the resulting AST.
//! Deliberately `gpui`-window-free (only [`gpui::Rgba`] is used, for the same reason
//! `crate::changes`/`crate::file_tree` already use it: colours are still plain data, not a
//! rendered element), mirroring this crate's established split between pure logic modules and
//! `crate::root`'s actual `Div` construction.
//!
//! ## Scope: Rust only
//!
//! Only `.rs` files get real syntax spans. Every other extension (`.toml`, `.md`, `.sql`, ...)
//! renders as plain, unhighlighted monospace text - a documented scope limit for this phase, not
//! a bug: a second real grammar (`tree-sitter-toml`, ...) is a mechanical repeat of the same
//! parse-then-walk shape [`highlight_rust`] already establishes, not a new capability, so it's
//! left for a later phase rather than half-done here.
//!
//! ## The real `tree-sitter` API surface used here
//!
//! Verified directly against `vendor/zed/crates/language/src/language.rs`, the one real,
//! already-building consumer of this exact `tree-sitter`/`tree-sitter-rust` version pair in this
//! workspace (`vendor/zed/Cargo.toml`: `tree-sitter = "0.26.9"`, `tree-sitter-rust = "0.24.2"` -
//! this crate's own `Cargo.toml` pins the identical versions):
//!
//! - `tree_sitter::Parser::new()` - `language.rs:135`.
//! - `tree_sitter_rust::LANGUAGE` (a `LanguageFn`) converted to a real `tree_sitter::Language`
//!   via `.into()` - `language.rs:1672`'s own test (`test_with_parser_resets_after_cancellation`):
//!   `let rust_language: TsLanguage = tree_sitter_rust::LANGUAGE.into();`.
//! - `Parser::set_language(&self, language: &Language) -> Result<(), LanguageError>` -
//!   `language.rs:1690`'s `parser.set_language(&rust_language).unwrap()` and `language.rs:1376`'s
//!   `.set_language(&grammar.ts_language)`.
//! - `Node::walk() -> TreeCursor`, `TreeCursor::goto_first_child()`/`goto_next_sibling()` (real,
//!   ordinary tree-sitter cursor traversal) - `vendor/zed/crates/language/src/outline.rs:102`
//!   (`let mut cursor = node.walk();`) and `crates/language/src/buffer.rs:4120`/`4262`/`4329`/
//!   `4380`/`4422` (`let mut cursor = layer.node().walk();`, the same real pattern repeated at
//!   five separate real call sites in that file).
//! - `Parser::parse(&mut self, text: impl AsRef<[u8]>, old_tree: Option<&Tree>) -> Option<Tree>`
//!   and `Tree::root_node() -> Node` are tree-sitter's own long-stable top-level entry points
//!   (the simpler, non-cancellable sibling of the `parse_with_options` call `language.rs:1693`
//!   exercises directly) - not separately grepped for since `language.rs`'s own
//!   `parse_with_options` call confirms `Parser`/`Tree`'s real shape, and `cargo build` is the
//!   final, authoritative check that `parse`/`root_node` exist with this signature.
//! - `TreeCursor::field_name(&self) -> Option<&'static str>` - tree-sitter's own standard cursor
//!   introspection method (not separately exercised in `vendor/zed`'s own code, since Zed's
//!   incremental `SyntaxSnapshot` walks via its own `highlights_query`/`Query` machinery instead
//!   of a raw field-name check) - used here only to tell a `function_item`'s own name apart from
//!   a `call_expression`'s callee, both real, ordinary `Node`/`TreeCursor` methods whose presence
//!   `cargo build`/`cargo test` (run before this phase was reported done) confirm directly.

use std::collections::HashSet;
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
/// `wt_core::diff`'s own `MAX_DIFF_OUTPUT_BYTES` cap (`crates/wt-core/src/diff.rs:87`, also
/// 2 MiB) - the same "don't let one pathological file stall the UI" reasoning applies equally
/// here, and reusing the already-established number keeps the two caps consistent rather than
/// picking a second, arbitrary one.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Which of Surface C's two views is currently showing for the file named by
/// `crate::root::AdeApp::open_change` - `design_handoff_jerry_ade/README.md`'s `code_view` state
/// field (`Diff | File`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeView {
    #[default]
    Diff,
    File,
}

/// A real file's detected line-ending style, read directly from its own bytes - never assumed.
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

/// Detects `bytes`' real line-ending style by inspecting the byte immediately before its first
/// `\n` - `Crlf` if that byte is `\r`, `Lf` otherwise (including files with no newline at all,
/// which have no evidence of `Crlf` to detect).
pub fn detect_line_ending(bytes: &[u8]) -> LineEnding {
    if let Some(newline_index) = bytes.iter().position(|&byte| byte == b'\n') {
        if newline_index > 0 && bytes[newline_index - 1] == b'\r' {
            return LineEnding::Crlf;
        }
    }
    LineEnding::Lf
}

/// The status bar's real language label, derived from `path`'s extension - the same
/// `.rs`/`.toml`/`.md`/`.sql` set `crate::file_tree::lang_chip_for_name` already recognizes
/// (case-insensitive, matching that function's own behavior), plus a generic fallback for
/// anything else rather than a blank label.
pub fn language_name_for_extension(extension: Option<&str>) -> &'static str {
    match extension.map(|ext| ext.to_ascii_lowercase()).as_deref() {
        Some("rs") => "Rust",
        Some("toml") => "TOML",
        Some("md") => "Markdown",
        Some("sql") => "SQL",
        _ => "Plain Text",
    }
}

/// A syntax span's real classification - `design_handoff_jerry_ade/README.md`'s File view
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

/// One real, classified leaf token from a real `tree-sitter` parse - byte offsets into the
/// whole-file source [`highlight_rust`] parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
}

/// Real Rust keyword tokens - tree-sitter-rust's grammar represents each of these as an
/// unnamed leaf node whose own `kind()` is the literal keyword text (verified by parsing real
/// Rust source and asserting on the resulting spans - see this module's tests).
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

/// Parses real Rust source with a real `tree-sitter` grammar and walks the resulting AST
/// (`tree_sitter::TreeCursor::goto_first_child`/`goto_next_sibling`, see this module's own docs
/// for exactly where each call is verified) into real, classified [`HighlightSpan`]s. Returns an
/// empty `Vec` (never a fabricated span) if the grammar fails to load or the parse itself
/// produces no tree - both defensive, since neither is expected to actually happen with the
/// bundled `tree-sitter-rust` grammar, but never assumed away.
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
            // A `function_item`'s own `name` field, or a `call_expression`'s direct `function`
            // field when the callee is a plain identifier (`foo()`, not `obj.foo()` - see this
            // module's doc comment on the deliberate `field_identifier`/method-call scope limit).
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

/// One real, already-highlighted display line: its own text (never including its line-ending
/// bytes) plus a real, gapless run list covering every byte of it (unhighlighted stretches are
/// real [`HighlightKind::Text`] runs, not left implicit) - computed once by [`build_lines`] and
/// cached in [`ParsedFile`], never recomputed per render.
///
/// Each run's text is a real, already-allocated [`SharedString`] (not a byte [`Range`] sliced
/// again on every render) - `crate::root::AdeApp::render_file_view`'s row builder used to call
/// `.to_string()` on a freshly-sliced range for every run of every visible row on every single
/// render (measured at ~24ms/frame at this crate's own render cap in a debug build - the same
/// "don't redo real work every frame" bug class [`ParsedFile`]'s own caching already avoids for
/// the parse itself, just showing up one level down). Allocating each run's owned text once here,
/// alongside the parse, and cloning the cheap, `Arc`-backed [`SharedString`] at render time
/// instead removes that per-frame allocation entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedLine {
    pub text: String,
    pub runs: Vec<(SharedString, HighlightKind)>,
}

/// Splits `source`'s real line boundaries (LF or CRLF alike - the trailing `\r`, if any, is
/// excluded from the returned range, matching [`RenderedLine::text`] never including line-ending
/// bytes) into byte ranges, then clips `spans` against each one to build a real, gapless
/// [`RenderedLine`] list. A trailing byte range past the last `\n` is always included (covering a
/// file with no trailing newline), and a completely empty `source` still yields exactly one
/// empty line - matching how a real editor shows an empty file as one empty line, not zero.
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
        // Each run's owned `SharedString` is sliced from `line_text` (a range relative to the
        // *line's own* start, matching how the ranges above were built) once, here - not
        // re-sliced/re-allocated by `crate::root::render_file_view_line` on every render (see
        // this struct's own docs).
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

/// A real file's parsed-and-highlighted content, cached in `crate::root::AdeApp::file_view_cache`
/// so [`load_file`]/[`highlight_rust`] run at most once per real file-content change - see
/// [`cache_is_fresh`]'s docs for the exact staleness check `crate::root::AdeApp::render_file_view`
/// runs before reusing (rather than recomputing) one of these on every render.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    pub len: u64,
    pub language: &'static str,
    pub line_ending: LineEnding,
    /// `true` if the real file on disk was larger than [`MAX_FILE_BYTES`] and this is only a
    /// prefix of it (cut back to the last real line boundary within the cap - see [`load_file`]).
    pub truncated: bool,
    pub lines: Vec<RenderedLine>,
}

/// Reads a real file from disk (reusing `std::fs::read`/`std::fs::metadata`, the same real I/O
/// primitives `crate::file_tree` already uses), caps it at [`MAX_FILE_BYTES`], detects its real
/// line-ending style and language, and - for a `.rs` file - runs it through a real
/// [`highlight_rust`] parse. Never fabricates content for a file that fails to read; the error is
/// propagated to the caller (`crate::root::AdeApp::render_file_view`), which renders it as a
/// real, honest message rather than silently showing nothing.
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
        lines,
    })
}

/// Whether `cached` is still a real, up-to-date parse of `path` - true iff the path matches and
/// both the real, freshly-read `mtime`/`len` are unchanged from what produced `cached`. Used by
/// `crate::root::AdeApp::render_file_view` to decide whether to reuse `cached` as-is or call
/// [`load_file`] again - the caching half of this phase's "don't reparse every frame" requirement
/// (see `crate::root::AdeApp::file_view_cache`'s own docs for how that's actually verified).
pub fn cache_is_fresh(
    cached: &ParsedFile,
    path: &Path,
    mtime: Option<SystemTime>,
    len: u64,
) -> bool {
    cached.path == path && cached.mtime == mtime && cached.len == len
}

/// The File view breadcrumb's real path segments (`design_handoff_jerry_ade/README.md`: "`src ›
/// db › query_builder.rs`") - every real `Normal` path component of `path`, in order. Root/prefix/
/// `.`/`..` components are skipped (a repo-relative diff/file-tree path never has any in practice,
/// but this is never assumed - see this module's tests).
pub fn breadcrumb_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// The File view's real 3px git-gutter marker set (`design_handoff_jerry_ade/README.md`: "a 3px
/// git gutter (`#2c6244` for agent-touched lines, transparent otherwise)") - the real, new-file
/// line numbers (1-indexed) that a hunk actually *added*, derived from `file`'s own real hunks
/// (reusing `crate::changes::parse_hunk_new_range`'s real `@@ ... @@` header parse - never a
/// second, hand-rolled diff). Context lines advance the new-file line counter without being
/// marked (they already existed before the change); removed lines don't exist in the new file at
/// all, so they never advance it or get marked.
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
        // "i32" is a primitive type token, appearing twice (parameter type, return type) -
        // just confirm at least one real occurrence was classified as Type.
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
        // The real `line_comment` node's own byte range includes its trailing newline (verified
        // by walking a real parse tree - see this module's docs) - the whole node is treated as
        // one comment span (never recursed into, unlike a plain expression node) since its own
        // children (`outer_doc_comment_marker`, `doc_comment`) are just its lexical pieces, not
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
        // Tree-sitter always produces a best-effort tree for malformed input rather than
        // failing outright - confirm this doesn't panic and still classifies the one real
        // keyword token present.
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
            // Every run's own real, already-allocated text, concatenated back together in
            // order, must reconstruct the line's own text exactly - no gap, overlap, or
            // out-of-order run (the same "gapless coverage" property the old byte-`Range`-based
            // version of this test checked, just verified against owned `SharedString`s now
            // that `RenderedLine::runs` caches those instead of ranges - see that struct's docs
            // for why).
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
