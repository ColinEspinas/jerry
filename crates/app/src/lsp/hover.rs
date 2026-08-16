//! Pure logic for Surface C's File view *Hover* state (`design_handoff_jerry_ade/README.md`'s
//! "Language server UI" subsection) - turns an `lsp_types::Hover` response (returned by
//! `rust-analyzer` via `lsp_core::LspClient::request`, see `crate::root::AdeApp::request_hover`)
//! into a render model `crate::root` draws a signature/doc/module-path card from, plus the
//! position-encoding helper the hover trigger needs to build that request. Deliberately
//! `gpui`-window-free (no `gpui` type is used at all - output is plain `String`s, wrapped in a
//! `SharedString` by `crate::root` at render time).

use std::ops::Range;

use lsp_core::lsp_types;

/// Converts a UTF-8 byte offset within `line_text` into the UTF-16 code-unit offset the LSP
/// `character` position encoding requires - the reverse direction of
/// `crate::lsp::diagnostics::index_diagnostics_by_line`'s UTF-16-to-byte conversion. Clamps to
/// `line_text`'s length for a `byte_offset` past the line's end, and never panics on an offset
/// that doesn't land on a `char` boundary (walks `char_indices` rather than slicing).
pub fn byte_offset_to_utf16_offset(line_text: &str, byte_offset: usize) -> u32 {
    let clamped = byte_offset.min(line_text.len());
    line_text
        .char_indices()
        .take_while(|(index, _)| *index < clamped)
        .map(|(_, ch)| ch.len_utf16() as u32)
        .sum()
}

/// Builds the `lsp_types::Position` for a click at `byte_offset` within `line_text`, on line
/// `line_index_zero_based` (0-based, one less than `crate::root::AdeApp::code_cursor`'s 1-based
/// convention) - the position `crate::root::AdeApp::request_hover`/`trigger_goto_definition`
/// send in a `textDocument/hover`/`textDocument/definition` request.
pub fn position_for_line_byte_offset(
    line_index_zero_based: u32,
    line_text: &str,
    byte_offset: usize,
) -> lsp_types::Position {
    lsp_types::Position {
        line: line_index_zero_based,
        character: byte_offset_to_utf16_offset(line_text, byte_offset),
    }
}

/// An already-parsed hover response, ready for `crate::code_surface::lsp_ui::render_hover_card` to draw - see
/// this module's top-level docs for how [`build_hover_render_model`] derives these three fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverRenderModel {
    /// The crate/module path prefix (`design_handoff_jerry_ade/README.md`: "`core::convert`"),
    /// when the hovered symbol is path-qualified and rust-analyzer's response included one -
    /// `None` for a local/parameter/literal/anything else with no module path.
    pub module_path: Option<String>,
    /// The signature/type line - always present for a non-empty hover response.
    pub signature: String,
    /// Remaining doc/explanatory prose, if any - `None` for a symbol with no doc comment, never
    /// a fabricated empty string standing in for "no doc".
    pub doc: Option<String>,
}

/// A doc body's real JSDoc-style structure (GitHub issue #200) - a leading, untagged description
/// paragraph, then zero or more recognized block tags. Parsed from [`HoverRenderModel::doc`]/
/// `crate::lsp::completion::completion_documentation_text`'s own already-degraded plain text (see
/// [`parse_doc_sections`]'s own docs), not from raw Markdown - a real rustdoc-style doc comment
/// (Rust's own convention: Markdown `#`-headers, no `@`-tags at all in the overwhelming majority
/// of real crates) simply never matches any of these tags and comes back as `description` alone,
/// unchanged from today's plain rendering.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedDoc {
    /// Everything before the first real `@tag` line - `None` for a doc body that is *only*
    /// tags, with no leading prose at all (rare, but real - some hand-written JSDoc comments
    /// open directly with `@param`).
    pub description: Option<String>,
    /// Each real `@param`/`@arg`/`@argument` tag, in source order, as `(name, description)` -
    /// see [`split_param_name`] for how the name is separated from an optional `{Type}` prefix
    /// and the description that follows it.
    pub params: Vec<(String, String)>,
    /// The real `@returns`/`@return` tag's own body, if the doc had one. Only the *last* one
    /// survives if a (malformed) doc comment somehow has more than one - the same "last write
    /// wins" a hand-authored doc comment with a real duplicate tag would produce from any real
    /// JSDoc-aware tool, not a fabricated merge of both.
    pub returns: Option<String>,
    /// Each real `@example`/`@examples` tag's own body, in source order - deliberately a `Vec`,
    /// not a single `Option`, since a real doc comment legitimately shows more than one usage
    /// example.
    pub examples: Vec<String>,
    /// Every other real recognized `@tag` (`@throws`, `@deprecated`, `@see`, `@since`, `@default`,
    /// `@template`, `@remarks`, ...) this parser has no dedicated bucket for, as `(tag, body)` -
    /// still real, still worth showing, just without params/returns/examples' own specialized
    /// layout.
    pub other: Vec<(String, String)>,
}

/// Splits a real `@param`/`@arg`/`@argument` tag's own remainder (the text immediately after the
/// tag word itself) into `(name, description)` - the real shapes JSDoc's own spec documents:
/// `name description`, `name - description`, `{Type} name description`, and an optional
/// `[name=default]` bracket form for an optional parameter. A leading `{Type}` annotation is
/// dropped rather than shown - this app has no separate slot for a param's own declared type yet
/// (the same real scope [`HoverRenderModel`] itself has always drawn: `name`/description, no
/// type column). `[`/`]` around an optional name, and any trailing `=default` inside them, are
/// stripped so `name` always reads as a plain bare identifier either way.
fn split_param_name(rest: &str) -> (String, String) {
    let mut rest = rest.trim_start();
    if let Some(after_open_brace) = rest.strip_prefix('{') {
        if let Some(close) = after_open_brace.find('}') {
            rest = after_open_brace[close + 1..].trim_start();
        }
    }
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let mut name = rest[..name_end].to_string();
    if let Some(bracketed) = name
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
    {
        name = bracketed.split('=').next().unwrap_or(bracketed).to_string();
    }
    let description = strip_separator_dash(rest[name_end..].trim_start());
    (name, description.to_string())
}

/// Drops the dash a doc tag's own prose is commonly separated from its tag/name by, so a rendered
/// description never opens with a stray dangling dash.
fn strip_separator_dash(text: &str) -> &str {
    text.strip_prefix(['-', '\u{2013}', '\u{2014}'])
        .map(str::trim_start)
        .unwrap_or(text)
}

/// Appends a real continuation line (any line of `doc` that didn't itself start a new `@tag`) to
/// `existing`, joined by a real newline rather than concatenated flat - a multi-line `@example`'s
/// own internal line breaks are real structure (it's code), and even a prose tag's own wrapped
/// second line reads better as a real paragraph break than run-on text.
fn append_doc_line(existing: &mut String, line: &str) {
    if !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(line);
}

/// Which real section of a [`ParsedDoc`] a continuation line currently being scanned belongs to -
/// [`parse_doc_sections`]'s own scan state, updated every time a new `@tag` line starts a section.
enum DocSectionCursor {
    Description,
    Param(usize),
    Returns,
    Example(usize),
    Other(usize),
}

/// Parses `doc`'s own real JSDoc-style structure - see [`ParsedDoc`]'s own docs for the shape and
/// why this reads already-degraded plain text, not raw Markdown. A real, line-oriented scan (not
/// a regex/grammar): any line whose first non-whitespace character is `@` immediately followed by
/// one or more ASCII letters starts a new tagged section; every line before the first one is the
/// real `description`; every line after one, up to the next `@tag` line or the end of `doc`, is a
/// real continuation of that same section (see [`append_doc_line`]).
pub fn parse_doc_sections(doc: &str) -> ParsedDoc {
    let mut sections = ParsedDoc::default();
    let mut description_lines: Vec<&str> = Vec::new();
    let mut cursor = DocSectionCursor::Description;

    for raw_line in doc.lines() {
        let trimmed = raw_line.trim_start();
        if let Some(after_at) = trimmed.strip_prefix('@') {
            let tag_len = after_at
                .find(|character: char| !character.is_ascii_alphabetic())
                .unwrap_or(after_at.len());
            if tag_len > 0 {
                let tag = &after_at[..tag_len];
                let rest = after_at[tag_len..].trim_start();
                match tag {
                    "param" | "arg" | "argument" => {
                        sections.params.push(split_param_name(rest));
                        cursor = DocSectionCursor::Param(sections.params.len() - 1);
                    }
                    "returns" | "return" => {
                        sections.returns = Some(strip_separator_dash(rest).to_string());
                        cursor = DocSectionCursor::Returns;
                    }
                    "example" | "examples" => {
                        sections.examples.push(rest.to_string());
                        cursor = DocSectionCursor::Example(sections.examples.len() - 1);
                    }
                    other => {
                        sections
                            .other
                            .push((other.to_string(), strip_separator_dash(rest).to_string()));
                        cursor = DocSectionCursor::Other(sections.other.len() - 1);
                    }
                }
                continue;
            }
        }
        match &cursor {
            DocSectionCursor::Description => description_lines.push(raw_line),
            DocSectionCursor::Param(index) => {
                append_doc_line(&mut sections.params[*index].1, raw_line)
            }
            DocSectionCursor::Returns => {
                if let Some(returns) = &mut sections.returns {
                    append_doc_line(returns, raw_line);
                }
            }
            DocSectionCursor::Example(index) => {
                append_doc_line(&mut sections.examples[*index], raw_line)
            }
            DocSectionCursor::Other(index) => {
                append_doc_line(&mut sections.other[*index].1, raw_line)
            }
        }
    }

    let description = description_lines.join("\n").trim().to_string();
    sections.description = (!description.is_empty()).then_some(description);
    for param in &mut sections.params {
        param.1 = param.1.trim().to_string();
    }
    if let Some(returns) = &mut sections.returns {
        *returns = returns.trim().to_string();
    }
    for example in &mut sections.examples {
        *example = example.trim().to_string();
    }
    for other in &mut sections.other {
        other.1 = other.1.trim().to_string();
    }
    sections
}

/// Item-signature keywords `rust-analyzer`'s hover convention was observed to always start a
/// signature paragraph with (see this module's top-level docs). Deliberately not `let ` - a
/// local variable's hover (e.g. `"let result: i32"`) starts with it too, but a local has no
/// module path preceding it; including `let` here would make [`looks_like_item_signature`]
/// misfire on the two-paragraph local-variable shape this module's tests capture.
const ITEM_SIGNATURE_KEYWORDS: &[&str] = &[
    "fn ",
    "struct ",
    "enum ",
    "trait ",
    "impl ",
    "const ",
    "static ",
    "type ",
    "mod ",
    "union ",
    "macro_rules!",
    "extern ",
];

/// Whether `paragraph` looks like a Rust item signature (see [`ITEM_SIGNATURE_KEYWORDS`]) -
/// deliberately narrow evidence [`build_hover_render_model`] uses to decide whether the
/// paragraph *before* this one is a module path or should itself be treated as the signature
/// (a local/parameter/literal's hover, which has no module path). A leading visibility qualifier
/// (`pub`, `pub(crate)`, `pub(super)`, `pub(in ...)`) is stripped first, so `"pub fn foo()"` is
/// recognized identically to `"fn foo()"`.
fn looks_like_item_signature(paragraph: &str) -> bool {
    let mut rest = paragraph;
    if let Some(after_pub) = rest.strip_prefix("pub") {
        rest = after_pub.trim_start();
        if let Some(after_open_paren) = rest.strip_prefix('(') {
            if let Some(close_index) = after_open_paren.find(')') {
                rest = after_open_paren[close_index + 1..].trim_start();
            }
        }
    }
    ITEM_SIGNATURE_KEYWORDS
        .iter()
        .any(|keyword| rest.starts_with(keyword))
        || looks_like_field_declaration(rest)
        || looks_like_enum_variant(rest)
}

/// The leading identifier-shaped prefix of `text` - the run of ASCII alphanumeric/`_`
/// characters starting at byte `0`, as long as `text` starts with an identifier (an ASCII letter
/// or `_`, never a digit). `0` for anything that doesn't start with an identifier at all -
/// [`looks_like_field_declaration`]/[`looks_like_enum_variant`] both treat that as "not a match".
fn leading_identifier_len(text: &str) -> usize {
    match text.chars().next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => text
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(text.len()),
        _ => 0,
    }
}

/// Whether `text` (already stripped of leading `pub`/`pub(...)` visibility, see
/// [`looks_like_item_signature`]) looks like a Rust struct field declaration - captured
/// `rust-analyzer` shape for a field's hover, e.g. `"h3probe::Point\n\npub x: f64"`: after
/// visibility is stripped, `"x: f64"` is a bare identifier immediately followed by `:`.
/// Deliberately requires the colon to follow with only optional whitespace in between, so this
/// doesn't misfire on the literal-value doc-prose paragraph (`"value of literal: 41 ..."`) -
/// there, `value` is followed by `" of literal: ..."`, not directly by a colon.
fn looks_like_field_declaration(text: &str) -> bool {
    let identifier_len = leading_identifier_len(text);
    if identifier_len == 0 {
        return false;
    }
    text[identifier_len..].trim_start().starts_with(':')
}

/// Whether `text` (already stripped of leading visibility, same as
/// [`looks_like_field_declaration`]) looks like a Rust enum variant's hover - captured
/// `rust-analyzer` shape: a bare, capitalized identifier, standing alone (a unit variant, e.g.
/// `"Red"`) or immediately followed by a tuple/struct variant's `(`/`{` (e.g. `"Rgb(u8, u8,
/// u8)"`/`"Rgb { r: u8, g: u8, b: u8 }"` - same variant-declaration grammar, not directly
/// captured but following the same pattern). The leading uppercase letter is load-bearing: every
/// other paragraph shape this module parses starts lowercase in practice, keeping this from
/// misfiring on an ordinary capitalized type name.
fn looks_like_enum_variant(text: &str) -> bool {
    match text.chars().next() {
        Some(first) if first.is_ascii_uppercase() => {}
        _ => return false,
    }
    let identifier_len = leading_identifier_len(text);
    if identifier_len == 0 {
        return false;
    }
    let rest = text[identifier_len..].trim_start();
    rest.is_empty() || rest.starts_with('(') || rest.starts_with('{')
}

/// Splits `text` into non-empty "paragraphs" - runs of text separated by one or more blank lines
/// (rust-analyzer's hover convention uses both a single blank line, between a module path and
/// its signature, and a double blank line, between a signature and its doc prose - see this
/// module's top-level docs). `str::split`'s non-overlapping-match semantics mean a `"\n\n\n"` run
/// still yields exactly one boundary, not a spurious empty paragraph in between.
fn split_paragraphs(text: &str) -> Vec<&str> {
    text.split("\n\n")
        .map(|segment| segment.trim_matches('\n').trim())
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Text content from any of `lsp_types::HoverContents`' three shapes - `Markup` is the only one
/// ever actually observed from any of this app's supported servers (see this module's top-level
/// docs), but `Scalar`/`Array` (`lsp_types::MarkedString`, the LSP's older, deprecated hover
/// shape) are still handled rather than assumed unreachable - a `LanguageString`'s `value` (its
/// code text, with the `language` tag dropped, since there's no place to show it) stands in for
/// a bare string the same way a `Markup`'s `value` does. A `Markup` response whose real `kind` is
/// `Markdown` is passed through [`degrade_markdown_to_plain_text`] first - see this module's
/// top-level docs.
fn markup_text(contents: &lsp_types::HoverContents) -> String {
    match contents {
        lsp_types::HoverContents::Markup(markup) => match markup.kind {
            lsp_types::MarkupKind::Markdown => degrade_markdown_to_plain_text(&markup.value),
            lsp_types::MarkupKind::PlainText => markup.value.clone(),
        },
        lsp_types::HoverContents::Scalar(marked) => marked_string_text(marked),
        lsp_types::HoverContents::Array(items) => items
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Degrades genuinely `Markdown`-kinded hover content into readable plain text - not a real
/// structured Markdown parse (this module's own paragraph/signature heuristics stay Rust-hover-
/// shaped regardless of what feeds them, see this module's top-level docs), just enough that a
/// real fenced code block, heading, list bullet, or bold marker never renders as literal
/// backtick/hash/asterisk syntax on screen. Verified against real, directly-observed
/// `typescript-language-server` hover output (see this function's own tests, which reuse the
/// exact captured sample `lsp_core::client`'s real TypeScript hover test produced), not
/// synthetic Markdown invented for this function alone.
pub(crate) fn degrade_markdown_to_plain_text(markdown: &str) -> String {
    let mut out_lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw_line in markdown.lines() {
        if raw_line.trim_start().starts_with("```") {
            let was_in_fence = in_fence;
            in_fence = !in_fence;
            if was_in_fence {
                // Closing a fence: start a new paragraph after the code block rather than
                // running it straight into whatever prose follows with no boundary at all.
                out_lines.push(String::new());
            }
            continue;
        }

        let mut line = raw_line.to_string();
        if !in_fence {
            let heading_stripped = line.trim_start_matches('#');
            if heading_stripped.len() != line.len() {
                line = heading_stripped.trim_start().to_string();
            }
            if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                line = rest.to_string();
            }
            // `**`/`` ` `` markers are only ever real Markdown emphasis/inline-code syntax
            // *outside* a fence - real code (a JS/TS template literal's own backtick, a Python
            // `2 ** 10`) uses the exact same bytes with a completely different meaning, and a
            // fenced `@example` block is exactly where that collision is most likely to appear.
            // Stripped per line, guarded by `!in_fence`, rather than once over the whole joined
            // string at the end (this function's own previous shape), which stripped these bytes
            // unconditionally - real, live-reported corruption of example code.
            line = line.replace("**", "").replace('`', "");
            line = unwrap_paired_emphasis(&line);
        }
        out_lines.push(line);
    }

    out_lines.join("\n")
}

/// Unwraps real single-`*` Markdown emphasis (`*@param*` -> `@param`), leaving an asterisk that
/// isn't real emphasis exactly where it was.
fn unwrap_paired_emphasis(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'*' {
            let start = index;
            while index < bytes.len() && bytes[index] != b'*' {
                index += 1;
            }
            out.push_str(&line[start..index]);
            continue;
        }
        // `index` is at a candidate opening marker: real emphasis never has whitespace (or
        // another marker) directly inside it.
        let content_start = index + 1;
        let opens = bytes
            .get(content_start)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'*');
        let close = opens
            .then(|| {
                bytes[content_start..]
                    .iter()
                    .position(|byte| *byte == b'*')
                    .map(|offset| content_start + offset)
            })
            .flatten()
            .filter(|close| *close > content_start && !bytes[close - 1].is_ascii_whitespace());
        match close {
            Some(close) => {
                out.push_str(&line[content_start..close]);
                index = close + 1;
            }
            // Not a real pair - keep the byte verbatim.
            None => {
                out.push('*');
                index += 1;
            }
        }
    }
    out
}

fn marked_string_text(marked: &lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(text) => text.clone(),
        lsp_types::MarkedString::LanguageString(language_string) => language_string.value.clone(),
    }
}

/// Parses an `lsp_types::Hover` response into a [`HoverRenderModel`] - see this module's
/// top-level docs for how the observed paragraph convention is decoded. `None` only for an
/// empty/whitespace-only response - an honest "nothing to show", never a fabricated empty-string
/// signature.
pub fn build_hover_render_model(hover: &lsp_types::Hover) -> Option<HoverRenderModel> {
    let text = markup_text(&hover.contents);
    let paragraphs = split_paragraphs(&text);
    let first = *paragraphs.first()?;

    let (module_path, signature, doc_start) = match paragraphs.get(1) {
        Some(second) if looks_like_item_signature(second) => {
            (Some(first.to_string()), (*second).to_string(), 2)
        }
        _ => (None, first.to_string(), 1),
    };

    let doc_paragraphs = paragraphs.get(doc_start..).unwrap_or_default();
    let doc = if doc_paragraphs.is_empty() {
        None
    } else {
        Some(doc_paragraphs.join("\n\n"))
    };

    Some(HoverRenderModel {
        module_path,
        signature,
        doc,
    })
}

/// Picks the first usable `(Uri, Range)` out of an `lsp_types::GotoDefinitionResponse`, an
/// untagged three-way union per the LSP spec (a single `Location`, a `Vec<Location>`, or a
/// `Vec<LocationLink>`) - `lsp_core::client`'s end-to-end definition test observes rust-analyzer
/// replying with the `Array` shape for a call-site query. Only the *first* location is used:
/// `design_handoff_jerry_ade/README.md`'s `F12 definition` footer navigates to one place, not a
/// disambiguation list. `Range` (not just a line number) is returned so the caller can navigate
/// without a second lookup, even though `crate::root::AdeApp::trigger_goto_definition` only uses
/// `range.start.line`.
pub fn first_definition_location(
    response: &lsp_types::GotoDefinitionResponse,
) -> Option<(lsp_types::Uri, lsp_types::Range)> {
    match response {
        lsp_types::GotoDefinitionResponse::Scalar(location) => {
            Some((location.uri.clone(), location.range))
        }
        lsp_types::GotoDefinitionResponse::Array(locations) => locations
            .first()
            .map(|location| (location.uri.clone(), location.range)),
        lsp_types::GotoDefinitionResponse::Link(links) => links
            .first()
            .map(|link| (link.target_uri.clone(), link.target_selection_range)),
    }
}

/// Which byte range of `line.text` a hover-triggering click (an already-rendered run's byte
/// span - see `crate::code_surface::file_view::render_file_view_line`'s click handler) should be underlined with
/// [`crate::theme::syntax::HOVER_UNDERLINE`] - run-level granularity (see this module's
/// top-level docs on position precision), not re-derived from rust-analyzer's returned
/// `Hover::range`, since the clicked run's byte range is already known exactly.
pub type HoverByteRange = Range<usize>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_doc_sections_splits_a_real_typescript_jsdoc_shape() {
        let doc = "Adds two numbers.\n\n@param left the first number\n@param right the second \
                    number\n@returns the sum\n@example\nadd(1, 2); // 3";
        let sections = parse_doc_sections(doc);
        assert_eq!(sections.description.as_deref(), Some("Adds two numbers."));
        assert_eq!(
            sections.params,
            vec![
                ("left".to_string(), "the first number".to_string()),
                ("right".to_string(), "the second number".to_string()),
            ]
        );
        assert_eq!(sections.returns.as_deref(), Some("the sum"));
        assert_eq!(sections.examples, vec!["add(1, 2); // 3".to_string()]);
        assert!(sections.other.is_empty());
    }

    #[test]
    fn parse_doc_sections_strips_a_real_type_annotation_and_bracketed_optional_name() {
        let doc = "@param {number} x the value\n@param [y=0] an optional value";
        let sections = parse_doc_sections(doc);
        assert_eq!(
            sections.params,
            vec![
                ("x".to_string(), "the value".to_string()),
                ("y".to_string(), "an optional value".to_string()),
            ]
        );
    }

    #[test]
    fn parse_doc_sections_reads_a_real_dash_separated_param_description() {
        let sections = parse_doc_sections("@param count - how many times");
        assert_eq!(
            sections.params,
            vec![("count".to_string(), "how many times".to_string())]
        );
    }

    #[test]
    fn parse_doc_sections_keeps_a_real_multi_line_example_as_one_block_with_real_line_breaks() {
        let doc = "@example\nconst x = 1;\nconst y = 2;\nconsole.log(x + y);";
        let sections = parse_doc_sections(doc);
        assert_eq!(
            sections.examples,
            vec!["const x = 1;\nconst y = 2;\nconsole.log(x + y);".to_string()]
        );
    }

    #[test]
    fn parse_doc_sections_collects_more_than_one_real_example() {
        let doc = "@example\nfirst();\n@example\nsecond();";
        let sections = parse_doc_sections(doc);
        assert_eq!(
            sections.examples,
            vec!["first();".to_string(), "second();".to_string()]
        );
    }

    #[test]
    fn parse_doc_sections_buckets_an_unrecognized_real_tag_as_other() {
        let sections = parse_doc_sections("@throws {Error} when x is negative");
        assert_eq!(
            sections.other,
            vec![(
                "throws".to_string(),
                "{Error} when x is negative".to_string()
            )]
        );
    }

    #[test]
    fn parse_doc_sections_leaves_a_real_untagged_rustdoc_style_comment_as_plain_description() {
        let doc = "Adds one to the given number.\n\nReturns the incremented value.";
        let sections = parse_doc_sections(doc);
        assert_eq!(sections.description.as_deref(), Some(doc));
        assert!(sections.params.is_empty());
        assert!(sections.returns.is_none());
        assert!(sections.examples.is_empty());
        assert!(sections.other.is_empty());
    }

    #[test]
    fn parse_doc_sections_has_no_description_when_the_doc_opens_directly_with_a_tag() {
        let sections = parse_doc_sections("@param x the value");
        assert_eq!(sections.description, None);
    }

    #[test]
    fn byte_offset_to_utf16_offset_is_identity_for_pure_ascii() {
        assert_eq!(byte_offset_to_utf16_offset("let x = 1;", 4), 4);
    }

    #[test]
    fn byte_offset_to_utf16_offset_accounts_for_a_real_multi_byte_char() {
        // "café x" - 'é' is 2 UTF-8 bytes but only 1 UTF-16 code unit, so the real UTF-16 offset
        // of the 'x' that follows it must be 5 (c=1,a=1,f=1,é=1 unit,space=1 => unit 5), even
        // though its real byte offset is 6.
        let text = "caf\u{e9} x";
        let x_byte_offset = text.find('x').expect("'x' present");
        assert_eq!(x_byte_offset, 6);
        assert_eq!(byte_offset_to_utf16_offset(text, x_byte_offset), 5);
    }

    #[test]
    fn byte_offset_to_utf16_offset_clamps_past_the_real_line_end() {
        assert_eq!(byte_offset_to_utf16_offset("abc", 999), 3);
    }

    #[test]
    fn position_for_line_byte_offset_builds_a_real_position() {
        let position = position_for_line_byte_offset(7, "    let result = add_one(41);", 21);
        assert_eq!(position.line, 7);
        assert_eq!(position.character, 21);
    }

    fn markup_hover(value: &str) -> lsp_types::Hover {
        lsp_types::Hover {
            contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::PlainText,
                value: value.to_string(),
            }),
            range: None,
        }
    }

    fn markdown_hover(value: &str) -> lsp_types::Hover {
        lsp_types::Hover {
            contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: value.to_string(),
            }),
            range: None,
        }
    }

    /// The exact real string captured from a real, running `typescript-language-server` (see
    /// `lsp_core::client::tests::typescript_language_server_returns_a_real_hover_for_a_documented_function`,
    /// whose own pinned `MarkupKind::Markdown` assertion guards this sample from silently going
    /// stale) - not synthetic Markdown invented for this test.
    const REAL_TYPESCRIPT_MARKDOWN_HOVER: &str =
        "\n```typescript\nfunction addOne(x: number): number\n```\nAdds one to the given number.";

    /// The exact real hover string a live `typescript-language-server` 5.3.0 returned for a
    /// genuinely JSDoc-tagged function (captured verbatim from a real spawned server against a
    /// real scratch project, not synthesized). The shape that matters, and that no synthetic
    /// fixture in this module had ever exercised before: **every tag is wrapped in real Markdown
    /// emphasis** (`*@param*`, not a bare `@param`), the name is inline code, the separator is a
    /// real em dash, each line ends in a Markdown hard break (two trailing spaces), and the
    /// `@example` body arrives as a real fenced code block rather than indented continuation
    /// lines.
    const REAL_TYPESCRIPT_JSDOC_MARKDOWN_HOVER: &str = concat!(
        "\n```typescript\nfunction formatName(first: string, last: string): string\n```\n",
        "Formats a user's display name for the header bar.\n\n",
        "*@param* `first` \u{2014} The user's given name.  \n\n",
        "*@param* `last` \u{2014} The user's family name.  \n\n",
        "*@returns* \u{2014} The formatted display name, trimmed.  \n\n",
        "*@example*  \n```typescript\nconst name = formatName(\"Ada\", \"Lovelace\");\n",
        "console.log(name);\n```"
    );

    #[test]
    fn degrade_markdown_unwraps_the_real_emphasis_typescript_wraps_every_jsdoc_tag_in() {
        let plain = degrade_markdown_to_plain_text(REAL_TYPESCRIPT_JSDOC_MARKDOWN_HOVER);
        assert!(
            plain.contains("\n@param first \u{2014} The user's given name."),
            "a real emphasized tag must degrade to a bare, parseable tag line, got: {plain:?}"
        );
        assert!(
            plain.contains("\n@returns \u{2014} The formatted display name, trimmed."),
            "got: {plain:?}"
        );
        assert!(
            !plain.contains('*'),
            "no literal Markdown emphasis marker should survive degrading, got: {plain:?}"
        );
    }

    #[test]
    fn degrade_markdown_leaves_a_real_unpaired_asterisk_alone() {
        assert_eq!(
            degrade_markdown_to_plain_text("the product 3 * 4 * 5 and a lone *args prefix"),
            "the product 3 * 4 * 5 and a lone *args prefix"
        );
    }

    #[test]
    fn a_real_typescript_jsdoc_hover_parses_into_real_structured_sections() {
        let hover = markdown_hover(REAL_TYPESCRIPT_JSDOC_MARKDOWN_HOVER);
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(
            model.signature,
            "function formatName(first: string, last: string): string"
        );
        let sections = parse_doc_sections(model.doc.as_deref().expect("a real doc body"));
        assert_eq!(
            sections.description.as_deref(),
            Some("Formats a user's display name for the header bar.")
        );
        assert_eq!(
            sections.params,
            vec![
                ("first".to_string(), "The user's given name.".to_string()),
                ("last".to_string(), "The user's family name.".to_string()),
            ],
            "the real em-dash separator typescript-language-server uses must be stripped the \
             same way an ASCII hyphen already was, not left leading every description"
        );
        assert_eq!(
            sections.returns.as_deref(),
            Some("The formatted display name, trimmed.")
        );
        assert_eq!(
            sections.examples,
            vec!["const name = formatName(\"Ada\", \"Lovelace\");\nconsole.log(name);".to_string()],
            "a real fenced @example body must survive as real, multi-line example code"
        );
    }

    #[test]
    fn degrade_markdown_to_plain_text_strips_a_real_fenced_code_block_and_starts_a_new_paragraph() {
        let plain = degrade_markdown_to_plain_text(REAL_TYPESCRIPT_MARKDOWN_HOVER);
        assert_eq!(
            plain,
            "\nfunction addOne(x: number): number\n\nAdds one to the given number."
        );
        assert!(!plain.contains('`'), "no backtick should survive degrading");
    }

    #[test]
    fn a_real_markdown_typescript_hover_still_splits_into_a_readable_signature_and_doc() {
        let hover = markdown_hover(REAL_TYPESCRIPT_MARKDOWN_HOVER);
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.signature, "function addOne(x: number): number");
        assert_eq!(model.doc.as_deref(), Some("Adds one to the given number."));
        assert!(!model.signature.contains('`'));
        assert!(!model.doc.unwrap().contains('`'));
    }

    #[test]
    fn degrade_markdown_to_plain_text_never_mangles_real_code_inside_a_fence() {
        let plain = degrade_markdown_to_plain_text(
            "```typescript\nconst greeting = `Hello, ${name}`;\nconst power = 2 ** 10;\n```\n\
             \n@example\n```typescript\nconst x = `template`;\n```",
        );
        assert!(
            plain.contains("const greeting = `Hello, ${name}`;"),
            "a real template literal's own backticks must survive - got: {plain:?}"
        );
        assert!(
            plain.contains("const power = 2 ** 10;"),
            "a real exponent's own `**` must survive - got: {plain:?}"
        );
        assert!(
            plain.contains("const x = `template`;"),
            "a second, later fenced block's own content must survive too - got: {plain:?}"
        );
    }

    #[test]
    fn degrade_markdown_to_plain_text_strips_headings_bullets_and_star_bold_markers() {
        let plain = degrade_markdown_to_plain_text(
            "## Foo\n\n- first point\n* second point\n\n**bold** and `code`",
        );
        assert!(!plain.contains('#'));
        assert!(!plain.contains('*'));
        assert!(!plain.contains('`'));
        assert!(plain.contains("Foo"));
        assert!(plain.contains("first point"));
        assert!(plain.contains("second point"));
        assert!(plain.contains("bold and code"));
    }

    #[test]
    fn degrade_markdown_to_plain_text_never_mangles_a_real_double_underscore_identifier() {
        let plain = degrade_markdown_to_plain_text(
            "Access `__proto__` or `__dirname` directly, or use **bold** text.",
        );
        assert!(
            plain.contains("__proto__"),
            "a real identifier containing a double underscore must survive intact, got: {plain:?}"
        );
        assert!(
            plain.contains("__dirname"),
            "a real identifier containing a double underscore must survive intact, got: {plain:?}"
        );
        assert!(!plain.contains("**"));
        assert!(plain.contains("bold"));
    }

    #[test]
    fn a_plaintext_markup_response_is_never_run_through_the_markdown_degrader() {
        // A `PlainText`-kinded response containing literal backtick-looking text (e.g. a doc
        // comment that itself quotes code with backticks) must pass through completely
        // unmodified - only a real `Markdown` kind should ever trigger stripping.
        let hover = markup_hover("`not actually markdown` stays as-is");
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.signature, "`not actually markdown` stays as-is");
    }

    // The four real strings captured from a real, running `rust-analyzer` - see this module's
    // top-level docs for how and why.

    #[test]
    fn a_real_documented_function_hover_splits_into_module_path_signature_and_doc() {
        let hover = markup_hover(
            "hover_probe_fixture\n\nfn add_one(x: i32) -> i32\n\n\nAdds one to the given \
             number.\n\nReturns the incremented value.",
        );
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.module_path.as_deref(), Some("hover_probe_fixture"));
        assert_eq!(model.signature, "fn add_one(x: i32) -> i32");
        assert_eq!(
            model.doc.as_deref(),
            Some("Adds one to the given number.\n\nReturns the incremented value.")
        );
    }

    #[test]
    fn a_real_local_variable_hover_has_no_module_path() {
        let hover = markup_hover("let result: i32\n\n\nsize = 4, align = 0x4, no Drop");
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.module_path, None);
        assert_eq!(model.signature, "let result: i32");
        assert_eq!(model.doc.as_deref(), Some("size = 4, align = 0x4, no Drop"));
    }

    #[test]
    fn a_real_parameter_hover_is_signature_only() {
        let hover = markup_hover("x: i32");
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.module_path, None);
        assert_eq!(model.signature, "x: i32");
        assert_eq!(model.doc, None);
    }

    #[test]
    fn a_real_literal_hover_is_not_misread_as_a_module_path() {
        // "i32" alone looks exactly like a real single-segment module path, but the paragraph
        // after it ("value of literal: ...") is not a real item signature, so this must fall
        // back to treating "i32" itself as the signature - not silently swallow the real literal
        // explanation as if it were a discarded module path.
        let hover = markup_hover("i32\n\n\nvalue of literal: 41 (0x29|0b101001)");
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.module_path, None);
        assert_eq!(model.signature, "i32");
        assert_eq!(
            model.doc.as_deref(),
            Some("value of literal: 41 (0x29|0b101001)")
        );
    }

    #[test]
    fn a_pub_item_signature_is_still_recognized_after_stripping_visibility() {
        assert!(looks_like_item_signature("pub fn foo() -> i32"));
        assert!(looks_like_item_signature("pub(crate) struct Foo"));
        assert!(looks_like_item_signature("fn foo()"));
        assert!(!looks_like_item_signature("value of literal: 41"));
        assert!(!looks_like_item_signature("let result: i32"));
    }

    // A real hover response for a struct field, captured from a real, running `rust-analyzer`
    // against a scratch fixture the same way this module's four function/local/parameter/literal
    // fixtures above were - see `looks_like_field_declaration`'s own docs.
    #[test]
    fn a_real_struct_field_hover_splits_into_module_path_and_field_signature() {
        let hover = markup_hover("h3probe::Point\n\npub x: f64");
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.module_path.as_deref(), Some("h3probe::Point"));
        assert_eq!(model.signature, "pub x: f64");
        assert_eq!(model.doc, None);
    }

    // A real hover response for an enum variant, captured the same way - see
    // `looks_like_enum_variant`'s own docs.
    #[test]
    fn a_real_enum_variant_hover_splits_into_module_path_signature_and_doc() {
        let hover = markup_hover("h3probe::Color\n\nRed\n\n\nThe red variant doc.");
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.module_path.as_deref(), Some("h3probe::Color"));
        assert_eq!(model.signature, "Red");
        assert_eq!(model.doc.as_deref(), Some("The red variant doc."));
    }

    #[test]
    fn field_and_variant_signatures_are_recognized_by_looks_like_item_signature() {
        assert!(looks_like_item_signature("pub x: f64"));
        assert!(looks_like_item_signature("x: f64"));
        assert!(looks_like_item_signature("Red"));
        assert!(looks_like_item_signature("Rgb(u8, u8, u8)"));
        assert!(looks_like_item_signature("Rgb { r: u8, g: u8, b: u8 }"));
        // A bare, lowercase-leading word with no trailing `:`/`(`/`{` at all must never be
        // misread as an enum variant - only a real, capitalized, `(`/`{`/end-of-string-shaped
        // paragraph should match.
        assert!(!looks_like_item_signature("red"));
        // The real literal-value doc-prose paragraph must still never collide with the new field
        // check - its leading identifier (`value`) isn't directly followed by a colon.
        assert!(!looks_like_item_signature(
            "value of literal: 41 (0x29|0b101001)"
        ));
    }

    #[test]
    fn an_empty_hover_response_yields_no_render_model() {
        let hover = markup_hover("   \n\n  ");
        assert_eq!(build_hover_render_model(&hover), None);
    }

    #[test]
    fn a_scalar_marked_string_response_is_still_handled_for_real() {
        let hover = lsp_types::Hover {
            contents: lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(
                "fn foo()".to_string(),
            )),
            range: None,
        };
        let model = build_hover_render_model(&hover).expect("a real, non-empty response");
        assert_eq!(model.signature, "fn foo()");
    }

    fn location(uri: &str, line: u32) -> lsp_types::Location {
        lsp_types::Location {
            uri: uri.parse().expect("a real, well-formed test URI"),
            range: lsp_types::Range {
                start: lsp_types::Position { line, character: 3 },
                end: lsp_types::Position {
                    line,
                    character: 10,
                },
            },
        }
    }

    #[test]
    fn first_definition_location_reads_a_real_scalar_response() {
        let response = lsp_types::GotoDefinitionResponse::Scalar(location("file:///a.rs", 4));
        let (uri, range) = first_definition_location(&response).expect("a real location");
        assert_eq!(uri.as_str(), "file:///a.rs");
        assert_eq!(range.start.line, 4);
    }

    #[test]
    fn first_definition_location_reads_the_real_first_array_entry() {
        let response = lsp_types::GotoDefinitionResponse::Array(vec![
            location("file:///first.rs", 1),
            location("file:///second.rs", 2),
        ]);
        let (uri, range) = first_definition_location(&response).expect("a real location");
        assert_eq!(uri.as_str(), "file:///first.rs");
        assert_eq!(range.start.line, 1);
    }

    #[test]
    fn first_definition_location_is_none_for_a_real_empty_array() {
        let response = lsp_types::GotoDefinitionResponse::Array(vec![]);
        assert_eq!(first_definition_location(&response), None);
    }

    #[test]
    fn first_definition_location_reads_a_real_link_response_using_target_selection_range() {
        let link = lsp_types::LocationLink {
            origin_selection_range: None,
            target_uri: "file:///target.rs".parse().expect("real test URI"),
            target_range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 5,
                    character: 0,
                },
            },
            target_selection_range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 2,
                    character: 3,
                },
                end: lsp_types::Position {
                    line: 2,
                    character: 9,
                },
            },
        };
        let response = lsp_types::GotoDefinitionResponse::Link(vec![link]);
        let (uri, range) = first_definition_location(&response).expect("a real location");
        assert_eq!(uri.as_str(), "file:///target.rs");
        // The real *selection* range (the target's own name span), not the whole real target
        // range (which could start well before the item, e.g. at its own doc comment).
        assert_eq!(range.start.line, 2);
        assert_eq!(range.start.character, 3);
    }
}
