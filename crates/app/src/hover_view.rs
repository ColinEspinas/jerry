//! Pure logic for Surface C's File view *Hover* state (`design_handoff_jerry_ade/README.md`'s
//! "Language server UI" subsection) - turning a real `lsp_types::Hover` response (as returned by
//! a real `rust-analyzer`, via a real `textDocument/hover` request sent through
//! `lsp_core::LspClient::request` - see `crate::root::AdeApp::request_hover`) into a real render
//! model `crate::root` can draw a signature/doc/module-path card from, plus the small position-
//! encoding helper the click-to-hover trigger needs to build that request in the first place.
//! Deliberately `gpui`-window-free (no `gpui` type is used at all here, unlike
//! `crate::code_view`/`crate::diagnostics_view`, which still touch `gpui::Rgba`/
//! `gpui::SharedString` for colour/text data - this module's own output is plain `String`s, left
//! for `crate::root` to wrap in a `SharedString` at render time), mirroring this crate's
//! established split between pure logic modules and `crate::root`'s actual `Div` construction.
//!
//! ## Scope decision: hover and go-to-definition are real; completions are not built at all
//!
//! `design_handoff_jerry_ade/README.md`'s `lsp_popup` state also covers `Completions` - not
//! this phase's job, and not a later phase's either without a real prerequisite this app
//! doesn't have. The design's Completions popup assumes a real, editable caret position mid-edit
//! (`⏎ accept` inserts the selected candidate's text at that caret) - `crate::code_view`'s File
//! view is, by an explicit, documented H1 scope decision, **read-only**: no caret is rendered, no
//! text is ever inserted anywhere, and `AdeApp::code_cursor` tracks only "which line was last
//! clicked", not an editable insertion point. Building a Completions popup that shows real
//! `textDocument/completion` candidates but has no way to actually insert one would be exactly
//! the kind of "component bound to nothing" this project's own conventions forbid: a `⏎ accept`
//! footer hint implies a real action that would silently do nothing on every real keypress. This
//! phase's real, deliberate choice is (a) from the two the step brief laid out - scope
//! Completions out entirely, build nothing that looks like it accepts a completion, and leave a
//! real text-editing surface (which a completions UI would need to sit honestly on top of) for a
//! future phase - rather than (b), a relabelled "inspector" popup: even a popup honestly framed
//! as non-interactive would still visually look like the design's own interactive completions
//! list (the same 290px candidate column, the same selected-row highlight, the same kind chips),
//! which risks reading as "this does something" regardless of a disclaimer's wording - the safer,
//! more defensible default the step brief itself names. Hover and go-to-definition have no such
//! problem: both are real, complete actions in a read-only viewer (`textDocument/hover` shows
//! real information; `textDocument/definition` navigates real existing view state,
//! `AdeApp::open_change`/`AdeApp::code_cursor` - see `crate::root::AdeApp::trigger_goto_definition`)
//! - neither one implies an edit that never happens.
//!
//! ## Real, observed rust-analyzer hover shape - not guessed
//!
//! `design_handoff_jerry_ade/README.md`'s Hover state asks for "signature, doc prose, `core::
//! convert` + `F12 definition` footer" - three real pieces `rust-analyzer`'s own hover response
//! does *not* return as three separate structured fields: `lsp_types::Hover::contents` is a
//! single markup blob, and the LSP spec says nothing about its internal structure. The real,
//! observed convention this module's [`build_hover_render_model`] parses was reverse-engineered
//! from actual responses - not the spec - by spawning a real `rust-analyzer` against small
//! scratch fixtures and inspecting real replies directly (the same technique
//! `lsp_core::client`'s own end-to-end diagnostics test established as this project's way of
//! avoiding fabricated LSP behaviour, and which this phase's own `lsp_core::client` hover/
//! definition end-to-end tests reuse). Four real examples captured this way, reused verbatim as
//! this module's own test fixtures below (a documented free function hovered at its call site; a
//! local variable; a function parameter; an integer literal):
//!
//! ```text
//! "hover_probe_fixture\n\nfn add_one(x: i32) -> i32\n\n\nAdds one to the given number.\n\nReturns the incremented value."
//! "let result: i32\n\n\nsize = 4, align = 0x4, no Drop"
//! "x: i32"
//! "i32\n\n\nvalue of literal: 41 (0x29|0b101001)"
//! ```
//!
//! The real, observed convention: the response is a sequence of blank-line-separated
//! "paragraphs". An item with a real module path (a real crate/module-qualified symbol - a
//! function, struct, etc.) puts that path on its own first paragraph, immediately followed by a
//! paragraph that is itself a real item signature (starts with `fn `/`struct `/... - see
//! [`looks_like_item_signature`]); anything else (a local, a parameter, a literal - none of which
//! *have* a module path) starts directly with its own signature/type paragraph instead. Every
//! remaining paragraph, if any, is real doc/explanatory prose. [`build_hover_render_model`]
//! encodes exactly this, structurally - a parsed real convention (verified against the four real
//! shapes above, kept as this module's own tests), not a fabricated field. The heuristic is
//! honestly imperfect (a bare single-segment paragraph that happens to itself be an item's own
//! whole hover, with no signature paragraph following it, could in principle be misread as a
//! module path with no signature) - see [`looks_like_item_signature`]'s own docs for why the
//! signature-paragraph check exists specifically to keep that failure mode rare in practice.
//!
//! ## Why plain text, not Markdown
//!
//! `lsp_core::client::LspClient::initialize`'s `ClientCapabilities` never sets
//! `text_document.hover.content_format`, so rust-analyzer has nothing to negotiate against and
//! falls back to its own default - observed here to always be [`lsp_types::MarkupKind::PlainText`]
//! (confirmed directly in every real captured response above; never `Markdown`, and never the
//! older [`lsp_types::HoverContents::Scalar`]/`Array` `MarkedString` shapes either, though
//! [`markup_text`] still handles those defensively rather than assuming only `Markup` can ever
//! arrive). A real consequence: any Markdown syntax a doc comment happens to use arrives
//! un-rendered in [`HoverRenderModel::doc`] - a documented, honest scope limit (this app has no
//! Markdown rendering pipeline at all), not a bug.
//!
//! ## Position precision: per-token, not per-character
//!
//! [`byte_offset_to_utf16_offset`] converts a real byte offset into the real UTF-16 `character`
//! offset the LSP spec's position encoding requires, but the byte offset it's given
//! (`crate::root::render_file_view_line`'s own click handler) is always the *start* of whichever
//! already-highlighted token/run the user clicked, not a genuinely sub-token pixel-accurate
//! position - this app has no character-level mouse hit-testing against a monospace text run
//! (the same documented scope limit `AdeApp::code_cursor`'s own docs already state for the status
//! bar's omitted `col N`). In practice this is indistinguishable from real per-character
//! precision for hover purposes: `rust-analyzer` resolves a hover/definition query to whichever
//! whole symbol/token contains the given position, and a token's own start position always falls
//! inside that same token - so this is a real, honest simplification with no observable downside
//! for what it's used for, not a fabricated fallback.

use std::ops::Range;

use lsp_core::lsp_types;

/// Converts a real UTF-8 byte offset within `line_text` into the UTF-16 code-unit offset the LSP
/// spec's default `character` position encoding requires (`lsp_core::client`'s
/// `ClientCapabilities` never negotiates a different one - see
/// `crate::diagnostics_view::index_diagnostics_by_line`'s own docs for the identical real
/// reasoning, applied here in the opposite direction: turning a real click position *into* a
/// request, rather than turning a real response *into* a render position). Clamps to
/// `line_text`'s own real length for a `byte_offset` past the line's end, and never panics on a
/// `byte_offset` that doesn't land on a real `char` boundary (walks `char_indices` rather than
/// slicing) - defensive, since every real caller only ever passes a token/run boundary (always a
/// real `char` boundary in practice), but never assumed.
pub fn byte_offset_to_utf16_offset(line_text: &str, byte_offset: usize) -> u32 {
    let clamped = byte_offset.min(line_text.len());
    line_text
        .char_indices()
        .take_while(|(index, _)| *index < clamped)
        .map(|(_, ch)| ch.len_utf16() as u32)
        .sum()
}

/// Builds the real `lsp_types::Position` for a real click at `byte_offset` (within `line_text`,
/// itself line index `line_index_zero_based` - a real, 0-based LSP line number, one less than
/// `crate::root::AdeApp::code_cursor`'s own 1-based convention) - the position
/// `crate::root::AdeApp::request_hover`/`trigger_goto_definition` send in a real
/// `textDocument/hover`/`textDocument/definition` request.
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

/// A real, already-parsed hover response, ready for `crate::root::render_hover_card` to draw -
/// see this module's own top-level docs for exactly how [`build_hover_render_model`] derives
/// these three fields from `rust-analyzer`'s real, unstructured markup blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverRenderModel {
    /// The real crate/module path prefix (`design_handoff_jerry_ade/README.md`: "`core::
    /// convert`"), when the hovered symbol is a real, path-qualified item and rust-analyzer's own
    /// response included one - `None` for a local/parameter/literal/anything else that has no
    /// real module path of its own (see this module's docs for the real, observed cases).
    pub module_path: Option<String>,
    /// The real signature/type line - always present for any real, non-empty hover response.
    pub signature: String,
    /// Real remaining doc/explanatory prose, if the response had any left after `module_path`/
    /// `signature` were taken - `None` for a symbol with no doc comment (a parameter, a literal
    /// with no further note, ...), never a fabricated empty string standing in for "no doc".
    pub doc: Option<String>,
}

/// Real item-signature keywords `rust-analyzer`'s own hover convention was observed to always
/// start a real signature paragraph with (see this module's top-level docs). Deliberately not
/// `let ` (a real local variable's own hover, e.g. `"let result: i32"`, starts with it too, but a
/// local has no module path preceding it in practice - including `let` here would make
/// [`looks_like_item_signature`] misfire on exactly the two-paragraph local-variable shape this
/// module's own tests capture).
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

/// Whether `paragraph` looks like a real Rust item signature (see
/// [`ITEM_SIGNATURE_KEYWORDS`]'s own docs) - real, deliberately narrow evidence
/// [`build_hover_render_model`] uses to decide whether the paragraph *before* this one is a real
/// module path (an item's hover) or should itself be treated as the signature (a local/parameter/
/// literal's hover, which has no module path at all - see this module's top-level docs for the
/// real, observed shapes both cases produce). A real leading visibility qualifier (`pub`,
/// `pub(crate)`, `pub(super)`, `pub(in ...)`) is stripped first, so `"pub fn foo()"` is recognized
/// identically to `"fn foo()"` - real rust-analyzer signatures include real visibility when the
/// item has one.
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

/// The real, leading identifier-shaped prefix of `text` - the run of ASCII alphanumeric/`_`
/// characters starting at byte `0`, as long as `text` actually starts with an identifier (an
/// ASCII letter or `_`, never a digit). `0` for anything that doesn't start with a real
/// identifier at all (an empty string, or a paragraph starting with punctuation/whitespace) -
/// [`looks_like_field_declaration`]/[`looks_like_enum_variant`] both treat that as "not a match"
/// rather than misreading an empty identifier as a real one.
fn leading_identifier_len(text: &str) -> usize {
    match text.chars().next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => text
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(text.len()),
        _ => 0,
    }
}

/// Whether `text` (already stripped of any leading `pub`/`pub(...)` visibility - see
/// [`looks_like_item_signature`]'s own stripping step, reused here) looks like a real Rust struct
/// field declaration - real, captured `rust-analyzer` shape for a struct field's own hover, e.g.
/// `"h3probe::Point\n\npub x: f64"`: after visibility is stripped, `"x: f64"` is a bare identifier
/// immediately followed by `:`. Deliberately requires the colon to follow the identifier with no
/// other real token in between (only optional whitespace), so this does not also misfire on the
/// real literal-value doc-prose paragraph (`"value of literal: 41 (0x29|0b101001)"`) - that
/// paragraph's own leading identifier (`value`) is followed by `" of literal: 41 ..."`, not
/// directly by a colon, so [`leading_identifier_len`]'s own real match stops before it.
fn looks_like_field_declaration(text: &str) -> bool {
    let identifier_len = leading_identifier_len(text);
    if identifier_len == 0 {
        return false;
    }
    text[identifier_len..].trim_start().starts_with(':')
}

/// Whether `text` (already stripped of any leading `pub`/`pub(...)` visibility, same as
/// [`looks_like_field_declaration`]) looks like a real Rust enum variant's own hover - real,
/// captured `rust-analyzer` shape: a bare, capitalized identifier, either standing entirely alone
/// (a unit variant, e.g. `"Red"`) or immediately followed by a real tuple/struct variant's own
/// `(`/`{` (e.g. `"Rgb(u8, u8, u8)"`/`"Rgb { r: u8, g: u8, b: u8 }"` - not directly captured from a
/// real response, but the same real Rust variant-declaration grammar the captured unit-variant
/// case is one instance of). A leading uppercase letter is real, load-bearing evidence here (every
/// other real paragraph shape this module parses - a signature keyword, a field, a local, a
/// literal - starts lowercase in practice), keeping this from misfiring on an ordinary capitalized
/// type name that happens to appear as some other paragraph's own leading word.
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

/// Splits `text` into real, non-empty "paragraphs" - runs of text separated by one or more real
/// blank lines (`rust-analyzer`'s own hover convention uses both a single blank line, between a
/// module path and its signature, and a double blank line, between a signature and its doc prose,
/// see this module's top-level docs for the real captured examples of both). `str::split`'s own
/// non-overlapping-match semantics mean a real `"\n\n\n"` run still yields exactly one boundary
/// here (verified directly in this module's own tests against the real captured triple-newline
/// example), not three, and not a spurious empty paragraph in between.
fn split_paragraphs(text: &str) -> Vec<&str> {
    text.split("\n\n")
        .map(|segment| segment.trim_matches('\n').trim())
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Real text content from any of `lsp_types::HoverContents`' three real shapes - `Markup` is the
/// only one ever actually observed from this client's own real `rust-analyzer` (see this module's
/// top-level docs for why), but `Scalar`/`Array` (`lsp_types::MarkedString`, the LSP's older,
/// deprecated-but-still-real hover shape) are still handled for real rather than assumed
/// unreachable - a `LanguageString`'s own `value` (its code text, with the `language` tag itself
/// dropped - there is no real place to show it, and the code text alone is still real, useful
/// content) stands in for a bare string the same way a `Markup`'s `value` does.
fn markup_text(contents: &lsp_types::HoverContents) -> String {
    match contents {
        lsp_types::HoverContents::Markup(markup) => markup.value.clone(),
        lsp_types::HoverContents::Scalar(marked) => marked_string_text(marked),
        lsp_types::HoverContents::Array(items) => items
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn marked_string_text(marked: &lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(text) => text.clone(),
        lsp_types::MarkedString::LanguageString(language_string) => language_string.value.clone(),
    }
}

/// Parses a real `lsp_types::Hover` response into a real [`HoverRenderModel`] - see this module's
/// top-level docs for exactly how the real, observed paragraph convention is decoded. `None` only
/// for a genuinely empty/whitespace-only response (no real paragraph at all) - an honest "nothing
/// to show", never a fabricated empty-string signature.
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

/// Picks the real, first usable `(Uri, Range)` out of a real `lsp_types::GotoDefinitionResponse`,
/// a real, untagged three-way union per the LSP spec (a single `Location`, a `Vec<Location>`, or
/// a `Vec<LocationLink>` - see [`lsp_types::GotoDefinitionResponse`]'s own docs), verified against
/// a real `rust-analyzer` response in `lsp_core::client`'s own end-to-end definition test (observed
/// there to reply with the `Array` shape for a real call-site query). Only the *first* real
/// location is used: `design_handoff_jerry_ade/README.md`'s own `F12 definition` footer navigates
/// to one place, not a disambiguation list (out of scope here, same as the rest of this phase's
/// real, documented simplifications). `Range` (not just a line number) is returned so the caller
/// can navigate to the real target's own line without a second lookup;
/// `crate::root::AdeApp::trigger_goto_definition` only actually uses `range.start.line`, but
/// keeping the real, whole `Range` here (rather than pre-extracting one field) keeps this
/// function's own real output self-describing.
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

/// Which real byte range of `line.text` a real hover-triggering click at `byte_range` (itself
/// always exactly one already-rendered run's own real byte span - see
/// `crate::root::render_file_view_line`'s own click handler) should be underlined with
/// [`crate::theme::syntax::HOVER_UNDERLINE`] - real, run-level granularity (see this module's
/// top-level docs on why that's not a meaningfully different precision from real per-character
/// hit-testing for this purpose), not a re-derivation from `rust-analyzer`'s own returned
/// `Hover::range` (which would require the reverse UTF-16-to-byte conversion
/// `crate::diagnostics_view::index_diagnostics_by_line` already performs for diagnostics - real,
/// but redundant here, since the clicked run's own byte range is already known exactly, having
/// been computed to build the request in the first place).
pub type HoverByteRange = Range<usize>;

#[cfg(test)]
mod tests {
    use super::*;

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
