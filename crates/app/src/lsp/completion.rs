//! Pure logic for Surface C's File view real Completions popup (Revision R8.5b) - deciding
//! *when* a real edit is completion-worthy, and turning a real `lsp_types::CompletionItem` into
//! the real byte range/text an [`crate::code_surface::edit_buffer::EditBuffer`] mutation should splice in if
//! accepted. Deliberately `gpui`-free (no `gpui` type appears anywhere in this module), mirroring
//! `crate::lsp::hover`'s own split between pure logic here and `crate::root`'s live request
//! dispatch/popover painting - see that module's own top doc comment for the same convention.
//!
//! `crate::lsp::hover`'s own top docs used to say a Completions popup was out of scope because
//! the File view was read-only with "no caret and no text insertion" - Revision R8.5a made the
//! File view a real text editor, closing exactly that gap, which is what this module (and
//! `crate::lsp::completion_popup`, its GPUI-facing counterpart) now build on.

use std::ops::Range;

use lsp_core::lsp_types;

/// The real `char` immediately before byte offset `cursor` in `content` - `None` at the very
/// start of the buffer. `crate::lsp::client::AdeApp::prepare_lsp_sync`'s own real trigger point:
/// the one real character an edit that just landed puts immediately before the caret.
pub fn char_before(content: &str, cursor: usize) -> Option<char> {
    content
        .get(..cursor.min(content.len()))?
        .chars()
        .next_back()
}

/// Whether `ch` continues a real identifier the way this app's three real supported languages
/// (Rust/TypeScript-family/Python) all agree on: ASCII alphanumeric or `_`. Deliberately ASCII-
/// only, not `char::is_alphanumeric` - a real completion-worthy identifier prefix in any of these
/// languages is ASCII (`café` isn't a legal Rust/TS/Python identifier), and staying ASCII-only
/// avoids a real, unnecessary Unicode-word-boundary judgment call this app's three supported
/// grammars never actually need.
pub fn is_identifier_continuation(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Decides whether a real edit that just landed with `char_before_cursor` as the real character
/// now immediately before the caret (`None` at the very start of the buffer) is completion-worthy,
/// and if so, the real `lsp_types::CompletionContext` a `textDocument/completion` request should
/// carry. `trigger_characters` is the real, server-advertised
/// `completionProvider.triggerCharacters` list (`lsp_core::LspClient::
/// completion_trigger_characters`) - checked *before* the identifier-continuation fallback, since
/// a server-advertised trigger character (e.g. TypeScript's `.`) is real, authoritative evidence
/// the server wants a request there, even for a character `is_identifier_continuation` would
/// otherwise reject.
///
/// `None` for anything else (whitespace, a closing bracket, a semicolon, ...) - the real, honest
/// "don't fire a request here" case a caller also reads as "dismiss an already-open popup", since
/// the context that justified it is gone.
pub fn completion_trigger(
    char_before_cursor: Option<char>,
    trigger_characters: &[String],
) -> Option<lsp_types::CompletionContext> {
    let ch = char_before_cursor?;
    if trigger_characters
        .iter()
        .any(|trigger| trigger == ch.to_string().as_str())
    {
        return Some(lsp_types::CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(ch.to_string()),
        });
    }
    if is_identifier_continuation(ch) {
        return Some(lsp_types::CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
            trigger_character: None,
        });
    }
    None
}

/// The real, authoritative `(lsp_types::Range, new_text)` a completion item's own `text_edit`
/// specifies, if it has one - preferred over `insert_text`/`label` whenever present (see this
/// module's top docs). `InsertAndReplace`'s `.insert` half is used, not `.replace`: this app has
/// no separate "replace the whole word, including anything after the caret" accept gesture (only
/// one real Tab/Enter accept path - see `crate::lsp::completion_popup`'s own docs), and `.insert` is
/// the half that matches plain "insert at the caret without touching trailing text" semantics,
/// the least surprising real default for a single accept action.
pub fn completion_text_edit(
    item: &lsp_types::CompletionItem,
) -> Option<(lsp_types::Range, String)> {
    match item.text_edit.as_ref()? {
        lsp_types::CompletionTextEdit::Edit(edit) => Some((edit.range, edit.new_text.clone())),
        lsp_types::CompletionTextEdit::InsertAndReplace(edit) => {
            Some((edit.insert, edit.new_text.clone()))
        }
    }
}

/// The real text to insert for a completion item that carries no `text_edit` at all - `insert_text`
/// if the server supplied one, else the item's own `label` (the spec's documented fallback: "when
/// falsy the label is used as the insert text").
pub fn completion_plain_insert_text(item: &lsp_types::CompletionItem) -> String {
    item.insert_text
        .clone()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| item.label.clone())
}

/// The real start of the identifier prefix immediately before `cursor` within `line_text` (a byte
/// offset local to `line_text`, e.g. `line_text[start..cursor]` is the real, currently-typed word) -
/// the fallback replace-range `crate::lsp::completion_popup` uses for a completion item with no
/// `text_edit` (see [`completion_text_edit`]'s docs), matching how real editors avoid duplicating
/// an already-typed prefix (`"pri" + accept "println!"` should yield `"println!"`, not
/// `"priprintln!"`) even when the server didn't say so explicitly. Scans backward over
/// [`is_identifier_continuation`] bytes only - safe on plain ASCII identifier bytes without a
/// grapheme-aware scan, since a non-identifier byte (including any real UTF-8 continuation byte,
/// which is never in `0x00..=0x7f`) always stops the scan on its own.
pub fn identifier_prefix_start(line_text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(line_text.len());
    let bytes = line_text.as_bytes();
    let mut start = cursor;
    while start > 0 {
        let byte = bytes[start - 1];
        if byte.is_ascii() && is_identifier_continuation(byte as char) {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// A completion item's real display label plus an optional secondary detail string - what
/// `crate::lsp::completion_popup`'s popover row actually renders on the row's own right side,
/// factored out here so it's testable without a live `gpui` window.
///
/// Prefers `CompletionItemLabelDetails::detail` over the legacy top-level `CompletionItem::detail`
/// whenever a server sent one: the LSP spec's own doc comment for that field is explicit -
/// "rendered less prominently directly after the label ... function signatures or type
/// annotations" - which is exactly this row's own job, and it's spec-designed to stay a clean
/// type/signature fragment with no qualifier mixed in. The legacy top-level `detail` field has no
/// such guarantee: real servers (typescript-language-server in particular) commonly pack a
/// qualifier *and* a type into it together for pre-3.16 clients (`"(property) Foo.bar: string"`),
/// which read as a genuinely confusing, mixed string when shown as a bare type hint - a real,
/// live-reported bug. Falls back to the legacy field for a server that never sent
/// `label_details` at all (rust-analyzer's own `detail` strings are already clean for most items).
pub fn completion_item_display(item: &lsp_types::CompletionItem) -> (String, Option<String>) {
    let detail = item
        .label_details
        .as_ref()
        .and_then(|label_details| label_details.detail.as_ref())
        .or(item.detail.as_ref())
        .map(|detail| detail.trim().to_string())
        .filter(|detail| !detail.is_empty());
    (item.label.clone(), detail)
}

/// The Completions detail pane's own real signature line (its top band, syntax-highlighted in
/// mono - `design_handoff_jerry_ade/revision 3/Jerry.dc.html`'s `fn push_str(&mut self, string:
/// &str)` example) - `label` immediately followed by `label_details.detail` (no space between
/// them, matching that field's own spec: "rendered ... directly after the label, without any
/// spacing") whenever a server sent one, composing the same clean, module-free "typing of the
/// suggestion" [`completion_item_display`]'s own docs describe, just spelled out in full rather
/// than left implicit next to a separately-rendered label. Falls back to the legacy top-level
/// `detail` string (already a real, complete, standalone signature for most rust-analyzer items),
/// then to the bare `label`, for a server that never sent `label_details` at all.
pub fn completion_signature_text(item: &lsp_types::CompletionItem) -> String {
    if let Some(detail) = item
        .label_details
        .as_ref()
        .and_then(|label_details| label_details.detail.as_deref())
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
    {
        return format!("{}{detail}", item.label);
    }
    item.detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .unwrap_or(item.label.as_str())
        .to_string()
}

/// A completion item's real doc prose, for the detail pane's own body text
/// (`design_handoff_jerry_ade/revision 3/README.md`: "Right 300: signature in mono, doc in 11px
/// Plex Sans #7d848b, module path footer" - the Completions popup's own doc/module-path pane,
/// mirroring `crate::lsp::hover::HoverRenderModel::doc` exactly). `None` for an item with no real
/// documentation - an honest "nothing to show", never a fabricated empty string.
///
/// Reuses [`crate::lsp::hover::degrade_markdown_to_plain_text`] for a genuinely `Markdown`-kinded
/// `MarkupContent` - the same real fenced-code/heading/bold-stripping pass that module's own docs
/// describe, not a second, independently-maintained copy of it. `Documentation::String` (the
/// LSP's older, deprecated shape) is passed through unmodified, same as that module's own
/// `MarkedString::String` handling.
pub fn completion_documentation_text(item: &lsp_types::CompletionItem) -> Option<String> {
    let text = match item.documentation.as_ref()? {
        lsp_types::Documentation::String(text) => text.clone(),
        lsp_types::Documentation::MarkupContent(markup) => match markup.kind {
            lsp_types::MarkupKind::Markdown => {
                crate::lsp::hover::degrade_markdown_to_plain_text(&markup.value)
            }
            lsp_types::MarkupKind::PlainText => markup.value.clone(),
        },
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A completion item's real fully-qualified path/module, for the detail pane's own footer
/// (`README.md`: "module path footer", mirroring `crate::lsp::hover::HoverRenderModel::module_path`).
/// `CompletionItemLabelDetails::description` is the LSP spec's own documented slot for exactly
/// this ("fully qualified names or file path"), which real servers (rust-analyzer's
/// `use`-path-qualified completions, for one) populate. `None` when the server didn't send one -
/// never a guessed or re-derived path.
pub fn completion_module_path(item: &lsp_types::CompletionItem) -> Option<String> {
    item.label_details
        .as_ref()?
        .description
        .as_ref()
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty())
}

/// The Completions popup's real kind-badge category (design:
/// `design_handoff_jerry_ade/revision/Jerry.dc.html`'s own `f`/`v`/`t` kind badges, lines
/// ~1792-1793 and ~467-473) - a coarse grouping of the much finer-grained real
/// `lsp_types::CompletionItemKind` a server actually reports, kept here (not in
/// `crate::lsp::completion_popup`) so the mapping is testable without a live `gpui` window, matching
/// this module's own gpui-free convention (see this module's own top docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKindBadge {
    Function,
    Variable,
    Type,
}

impl CompletionKindBadge {
    /// The one-letter glyph the popup's real kind badge paints - `f`/`v`/`t`, matching the design
    /// mockup's own real `c.kind` values byte-for-byte.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Function => "f",
            Self::Variable => "v",
            Self::Type => "t",
        }
    }
}

/// Maps a real `lsp_types::CompletionItemKind` (from a real `CompletionItem::kind`, `None` for a
/// server that didn't report one) onto the popup's three-way real kind badge - `None` for any
/// real kind the design mockup's own three-category badge has no real slot for (`Keyword`,
/// `Snippet`, `File`, ...), which [`crate::lsp::completion_popup`]'s render simply skips (no badge at
/// all), never a guessed/default category.
pub fn completion_kind_badge(
    kind: Option<lsp_types::CompletionItemKind>,
) -> Option<CompletionKindBadge> {
    use lsp_types::CompletionItemKind as Kind;
    match kind? {
        Kind::FUNCTION | Kind::METHOD | Kind::CONSTRUCTOR => Some(CompletionKindBadge::Function),
        Kind::VARIABLE | Kind::FIELD | Kind::PROPERTY | Kind::CONSTANT | Kind::ENUM_MEMBER => {
            Some(CompletionKindBadge::Variable)
        }
        Kind::CLASS
        | Kind::STRUCT
        | Kind::INTERFACE
        | Kind::ENUM
        | Kind::TYPE_PARAMETER
        | Kind::MODULE => Some(CompletionKindBadge::Type),
        _ => None,
    }
}

/// The real text the user's typed prefix must be matched against for `item`: the server's own
/// `filterText` whenever it supplied a non-empty one, else the item's `label` - exactly the
/// fallback the LSP spec mandates for `CompletionItem.filterText` ("When `falsy` the label is used
/// as the filter text"). This matters for real, live servers: rust-analyzer returns items whose
/// *label* carries decoration a typed prefix will never match (`new(…)`, a leading `⚡` for a
/// snippet-ish item) alongside a plain `filterText` that it will.
pub fn completion_filter_text(item: &lsp_types::CompletionItem) -> &str {
    item.filter_text
        .as_deref()
        .filter(|text| !text.is_empty())
        .unwrap_or(item.label.as_str())
}

/// How *directly* a candidate matched the typed prefix. Ordered best-first by `derive(Ord)`'s own
/// declaration order, and compared before every other component of a [`CompletionMatch`], so a
/// real prefix hit always beats a mid-string one, which always beats a scattered one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletionMatchTier {
    /// The query is a real leading prefix of the candidate (`ver` in `version`).
    Prefix,
    /// The query occurs contiguously, but not at the start (`ver` in `has_version`).
    Substring,
    /// The query's characters occur in order but with real gaps between them (`vrs` in
    /// `version`).
    Subsequence,
}

/// One candidate's real match quality, ordered best-first: `derive(Ord)` compares the fields in
/// declaration order, which *is* the ranking policy, so the ordering rule lives in one place
/// rather than in a hand-written `cmp`.
///
/// - `tier` first (see [`CompletionMatchTier`]).
/// - `start` - an earlier match is a better one (`ver` in `xver` beats `ver` in `xxxxver`).
/// - `gaps` - how many times the match had to skip characters at a position that *isn't* a real
///   word start; skipping to a `snake_case`/`camelCase` boundary is free, since that is exactly
///   the query shape (`rts` for `read_to_string`) a real fuzzy matcher is supposed to reward.
/// - `span` - how much of the candidate the match had to stretch across; a tighter match wins.
/// - `case_mismatches` - matching is case-insensitive, but an exactly-cased match wins the tie
///   (`Ver` prefers `Version` over `version`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompletionMatch {
    pub tier: CompletionMatchTier,
    pub start: usize,
    pub gaps: usize,
    pub span: usize,
    pub case_mismatches: usize,
}

/// Whether `index` starts a real word inside `chars` - the start of the string, anything right
/// after a non-alphanumeric separator (`_`, `.`, `-`, `::`), or a `camelCase` lower-to-upper
/// transition.
fn is_word_start(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = chars[index - 1];
    if !previous.is_alphanumeric() {
        return true;
    }
    chars[index].is_uppercase() && !previous.is_uppercase()
}

/// The real match, if any, of `query` against `candidate` - `None` when `query`'s characters do
/// not all occur in `candidate`, in order.
///
/// ## Why a real subsequence match, and why this isn't a second copy of the palette's matcher
///
/// The command palette (`crate::palette::state`) deliberately matches on *contiguous* substrings
/// only, and says so in its own module docs: a palette row highlights one contiguous span, and
/// scattered highlight characters would read as noise there. That decision is about the palette's
/// own rendering, not about matching in general, so it can't simply be adopted here - every real
/// LSP client (VSCode, coc.nvim, ...) filters completions by a real *subsequence* match against
/// `filterText`, which is what GitHub issue #189 explicitly asks this popup to behave like.
///
/// So the palette's own real, tested [`crate::palette::state::substring_match`] is *reused as-is*
/// for the two contiguous tiers rather than reimplemented here (it already does the leftmost,
/// alignment-safe ASCII-folded search this needs), and only the genuinely new part - the scattered
/// subsequence tier and the tier/gap/span ranking above it - is added. Nothing is duplicated.
///
/// The subsequence walk is a deterministic greedy leftmost one, not a full dynamic-programming
/// optimum like VSCode's own `fuzzyScore`: for identifier-length strings the difference only ever
/// affects the *ranking* of an already-matching item (never whether it matches at all), and the
/// two tiers that carry real ranking weight - prefix and contiguous substring - are resolved
/// exactly, before the greedy walk is ever reached.
pub fn completion_match(candidate: &str, query: &str) -> Option<CompletionMatch> {
    if query.is_empty() {
        // Nothing typed past the trigger point yet: every candidate the server returned is
        // equally, honestly relevant, and `rank_completion_items`' own index tiebreak then
        // preserves the server's own ordering exactly.
        return Some(CompletionMatch {
            tier: CompletionMatchTier::Prefix,
            start: 0,
            gaps: 0,
            span: 0,
            case_mismatches: 0,
        });
    }

    let haystack: Vec<char> = candidate.chars().collect();
    let needle: Vec<char> = query.chars().collect();
    if needle.len() > haystack.len() {
        return None;
    }

    if let Some((start, len)) = crate::palette::state::substring_match(candidate, query) {
        let case_mismatches = (0..len)
            .filter(|offset| haystack[start + offset] != needle[*offset])
            .count();
        return Some(CompletionMatch {
            tier: if start == 0 {
                CompletionMatchTier::Prefix
            } else {
                CompletionMatchTier::Substring
            },
            start,
            gaps: 0,
            span: len,
            case_mismatches,
        });
    }

    let folded_haystack: Vec<char> = haystack.iter().map(|c| c.to_ascii_lowercase()).collect();
    let folded_needle: Vec<char> = needle.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut positions = Vec::with_capacity(folded_needle.len());
    let mut search_from = 0usize;
    for wanted in &folded_needle {
        let found = folded_haystack[search_from..]
            .iter()
            .position(|ch| ch == wanted)?
            + search_from;
        positions.push(found);
        search_from = found + 1;
    }

    let start = positions[0];
    let end = *positions
        .last()
        .expect("a non-empty query matched at least one char");
    let gaps = positions
        .windows(2)
        .filter(|pair| pair[1] != pair[0] + 1 && !is_word_start(&haystack, pair[1]))
        .count();
    let case_mismatches = positions
        .iter()
        .enumerate()
        .filter(|(index, position)| haystack[**position] != needle[*index])
        .count();
    Some(CompletionMatch {
        tier: CompletionMatchTier::Subsequence,
        start,
        gaps,
        span: end - start + 1,
        case_mismatches,
    })
}

/// The real, client-side narrowed and re-ranked view of `items` for the prefix the user has typed
/// since the completion was triggered: indices into `items`, best match first, with every item
/// that doesn't match `query` at all left out entirely.
///
/// This is the whole point of GitHub issue #189: real language servers (rust-analyzer included)
/// answer a `textDocument/completion` request with a broad, position-relevant candidate set and
/// expect the *client* to narrow it locally as more characters arrive, rather than re-narrowing it
/// themselves on every keystroke. Returning indices (not cloned items) is what lets
/// `crate::lsp::completion_popup` keep the server's full response intact underneath, so pressing
/// Backspace genuinely widens the list back out instead of having to re-ask the server for what it
/// already sent.
///
/// Ties (including *every* pair under an empty `query`) fall back to the item's own original index,
/// so the server's own ordering is preserved wherever this ranking has nothing real to say about
/// it - a deliberate choice not to start honoring `CompletionItem::sortText` here, which this app
/// has never applied and which is a separate concern from narrowing by typed text.
pub fn rank_completion_items(items: &[lsp_types::CompletionItem], query: &str) -> Vec<usize> {
    let mut scored: Vec<(CompletionMatch, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            completion_match(completion_filter_text(item), query).map(|score| (score, index))
        })
        .collect();
    scored.sort();
    scored.into_iter().map(|(_, index)| index).collect()
}

/// A completion item's byte range, purely for a caller (`crate::lsp::completion_popup`) that already
/// has a resolved fallback `Range<usize>` (the identifier prefix, per [`identifier_prefix_start`])
/// and just needs a shared type - kept here only as a doc anchor; the real conversion from
/// `lsp_types::Range`'s line/UTF-16-character coordinates into a real buffer byte offset needs
/// `EditBuffer::offset_for_position` (a real buffer to resolve against), so it can't live in this
/// `gpui`/buffer-free module - see `crate::lsp::completion_popup::resolve_completion_edit`.
pub type ByteRange = Range<usize>;

#[cfg(test)]
mod tests {
    use super::*;

    fn triggers(chars: &[&str]) -> Vec<String> {
        chars.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn an_identifier_continuation_character_triggers_an_invoked_completion() {
        let context = completion_trigger(Some('x'), &triggers(&["."])).expect("should trigger");
        assert_eq!(
            context.trigger_kind,
            lsp_types::CompletionTriggerKind::INVOKED
        );
        assert_eq!(context.trigger_character, None);
    }

    #[test]
    fn a_digit_or_underscore_also_triggers_an_invoked_completion() {
        assert!(completion_trigger(Some('9'), &[]).is_some());
        assert!(completion_trigger(Some('_'), &[]).is_some());
    }

    #[test]
    fn a_server_advertised_trigger_character_triggers_a_real_trigger_character_completion() {
        let context = completion_trigger(Some('.'), &triggers(&[".", "::"])).expect("trigger");
        assert_eq!(
            context.trigger_kind,
            lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER
        );
        assert_eq!(context.trigger_character.as_deref(), Some("."));
    }

    #[test]
    fn whitespace_and_punctuation_do_not_trigger_when_not_a_real_advertised_trigger_character() {
        assert_eq!(completion_trigger(Some(' '), &triggers(&["."])), None);
        assert_eq!(completion_trigger(Some(';'), &triggers(&["."])), None);
        assert_eq!(completion_trigger(Some(')'), &[]), None);
    }

    #[test]
    fn the_start_of_the_buffer_never_triggers() {
        assert_eq!(completion_trigger(None, &triggers(&["."])), None);
    }

    #[test]
    fn completion_text_edit_prefers_a_real_plain_edit() {
        let item = lsp_types::CompletionItem {
            label: "println!".to_string(),
            text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "println!".to_string(),
            })),
            ..Default::default()
        };
        let (range, text) = completion_text_edit(&item).expect("a real edit");
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.character, 3);
        assert_eq!(text, "println!");
    }

    #[test]
    fn completion_text_edit_reads_the_insert_half_of_a_real_insert_and_replace_edit() {
        let item = lsp_types::CompletionItem {
            label: "println!".to_string(),
            text_edit: Some(lsp_types::CompletionTextEdit::InsertAndReplace(
                lsp_types::InsertReplaceEdit {
                    new_text: "println!".to_string(),
                    insert: lsp_types::Range {
                        start: lsp_types::Position {
                            line: 0,
                            character: 0,
                        },
                        end: lsp_types::Position {
                            line: 0,
                            character: 3,
                        },
                    },
                    replace: lsp_types::Range {
                        start: lsp_types::Position {
                            line: 0,
                            character: 0,
                        },
                        end: lsp_types::Position {
                            line: 0,
                            character: 10,
                        },
                    },
                },
            )),
            ..Default::default()
        };
        let (range, _) = completion_text_edit(&item).expect("a real edit");
        assert_eq!(
            range.end.character, 3,
            "the insert half (3), not the wider replace half (10), must be used"
        );
    }

    #[test]
    fn completion_text_edit_is_none_without_a_real_text_edit() {
        let item = lsp_types::CompletionItem {
            label: "foo".to_string(),
            ..Default::default()
        };
        assert_eq!(completion_text_edit(&item), None);
    }

    #[test]
    fn completion_plain_insert_text_prefers_a_real_insert_text_over_the_label() {
        let item = lsp_types::CompletionItem {
            label: "foo (function)".to_string(),
            insert_text: Some("foo".to_string()),
            ..Default::default()
        };
        assert_eq!(completion_plain_insert_text(&item), "foo");
    }

    #[test]
    fn completion_plain_insert_text_falls_back_to_the_real_label_when_insert_text_is_absent() {
        let item = lsp_types::CompletionItem {
            label: "foo".to_string(),
            insert_text: None,
            ..Default::default()
        };
        assert_eq!(completion_plain_insert_text(&item), "foo");
    }

    #[test]
    fn identifier_prefix_start_walks_back_to_the_real_start_of_the_typed_word() {
        assert_eq!(identifier_prefix_start("let x = pri", 11), 8);
        assert_eq!(identifier_prefix_start("    foo_bar2", 12), 4);
    }

    #[test]
    fn identifier_prefix_start_stops_at_a_real_non_identifier_byte() {
        assert_eq!(identifier_prefix_start("foo.bar", 7), 4);
        assert_eq!(identifier_prefix_start("a.b", 3), 2);
    }

    #[test]
    fn identifier_prefix_start_is_the_cursor_itself_with_no_real_prefix() {
        assert_eq!(identifier_prefix_start("foo.", 4), 4);
        assert_eq!(identifier_prefix_start("", 0), 0);
    }

    #[test]
    fn completion_kind_badge_groups_function_like_kinds() {
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::FUNCTION)),
            Some(CompletionKindBadge::Function)
        );
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::METHOD)),
            Some(CompletionKindBadge::Function)
        );
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::CONSTRUCTOR)),
            Some(CompletionKindBadge::Function)
        );
    }

    #[test]
    fn completion_kind_badge_groups_variable_like_kinds() {
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::VARIABLE)),
            Some(CompletionKindBadge::Variable)
        );
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::FIELD)),
            Some(CompletionKindBadge::Variable)
        );
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::CONSTANT)),
            Some(CompletionKindBadge::Variable)
        );
    }

    #[test]
    fn completion_kind_badge_groups_type_like_kinds() {
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::STRUCT)),
            Some(CompletionKindBadge::Type)
        );
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::CLASS)),
            Some(CompletionKindBadge::Type)
        );
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::ENUM)),
            Some(CompletionKindBadge::Type)
        );
    }

    #[test]
    fn completion_kind_badge_is_none_for_a_kind_with_no_real_badge_slot_or_no_kind_at_all() {
        assert_eq!(
            completion_kind_badge(Some(lsp_types::CompletionItemKind::KEYWORD)),
            None
        );
        assert_eq!(completion_kind_badge(None), None);
    }

    #[test]
    fn completion_kind_badge_letters_match_the_design_mockups_own_glyphs() {
        assert_eq!(CompletionKindBadge::Function.letter(), "f");
        assert_eq!(CompletionKindBadge::Variable.letter(), "v");
        assert_eq!(CompletionKindBadge::Type.letter(), "t");
    }

    fn item(label: &str) -> lsp_types::CompletionItem {
        lsp_types::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        }
    }

    fn ranked_labels(items: &[lsp_types::CompletionItem], query: &str) -> Vec<String> {
        rank_completion_items(items, query)
            .into_iter()
            .map(|index| items[index].label.clone())
            .collect()
    }

    #[test]
    fn completion_filter_text_prefers_a_real_server_supplied_filter_text() {
        let mut with_filter = item("new(…)");
        with_filter.filter_text = Some("new".to_string());
        assert_eq!(completion_filter_text(&with_filter), "new");
    }

    #[test]
    fn completion_filter_text_falls_back_to_the_label_when_absent_or_empty() {
        assert_eq!(completion_filter_text(&item("version")), "version");
        let mut blank = item("version");
        blank.filter_text = Some(String::new());
        assert_eq!(
            completion_filter_text(&blank),
            "version",
            "the spec's own \"when falsy the label is used\" fallback must cover an empty string, \
             not just a missing field"
        );
    }

    #[test]
    fn an_empty_query_matches_every_item_and_preserves_the_servers_own_order() {
        let items = [item("zeta"), item("alpha"), item("version")];
        assert_eq!(ranked_labels(&items, ""), ["zeta", "alpha", "version"]);
    }

    #[test]
    fn a_real_typed_prefix_narrows_the_list_and_excludes_an_irrelevant_item() {
        let items = [
            item("version"),
            item("verify"),
            item("unwrap"),
            item("clone"),
        ];
        assert_eq!(
            ranked_labels(&items, "ver"),
            ["version", "verify"],
            "`unwrap`/`clone` carry no real `ver` match at all and must be filtered out entirely"
        );
    }

    #[test]
    fn matching_is_a_real_subsequence_match_not_only_a_contiguous_one() {
        // "vrs" is genuinely non-contiguous inside "version" (v_0, r_2, s_3) - a plain
        // substring/prefix matcher would reject it outright.
        let items = [item("version"), item("verify")];
        assert_eq!(
            ranked_labels(&items, "vrs"),
            ["version"],
            "a real fuzzy client must keep `version` for `vrs` and still drop `verify`, which \
             has no `s` at all"
        );
    }

    #[test]
    fn a_real_word_boundary_subsequence_match_is_kept() {
        let items = [item("read_to_string"), item("replace")];
        assert_eq!(
            ranked_labels(&items, "rts"),
            ["read_to_string"],
            "snake_case initials are a real, everyday fuzzy query"
        );
    }

    #[test]
    fn a_real_prefix_match_outranks_a_substring_match_which_outranks_a_scattered_one() {
        let items = [
            // Deliberately server-ordered worst-first, so only real re-ranking can fix it.
            item("vector_of_readers"), // scattered subsequence
            item("has_version"),       // contiguous, but not at the start
            item("version"),           // real prefix
        ];
        assert_eq!(
            ranked_labels(&items, "ver"),
            ["version", "has_version", "vector_of_readers"]
        );
    }

    #[test]
    fn ranking_prefers_the_earlier_and_tighter_of_two_real_matches_of_the_same_tier() {
        let items = [item("xxxxver"), item("xver")];
        assert_eq!(ranked_labels(&items, "ver"), ["xver", "xxxxver"]);
    }

    #[test]
    fn matching_is_case_insensitive_but_an_exact_case_match_ranks_first() {
        let items = [item("Version"), item("version")];
        assert_eq!(ranked_labels(&items, "ver"), ["version", "Version"]);
        assert_eq!(ranked_labels(&items, "Ver"), ["Version", "version"]);
    }

    #[test]
    fn ranking_matches_against_the_real_filter_text_not_the_label_when_the_server_supplied_one() {
        let mut item_with_filter = item("⚡ insert version macro");
        item_with_filter.filter_text = Some("version".to_string());
        let items = [item_with_filter, item("unrelated")];
        assert_eq!(
            ranked_labels(&items, "ver"),
            ["⚡ insert version macro"],
            "the server's own `filterText` must be what the typed prefix is matched against"
        );
    }

    #[test]
    fn a_query_matching_nothing_ranks_nothing() {
        let items = [item("version"), item("verify")];
        assert!(rank_completion_items(&items, "zzz").is_empty());
    }

    #[test]
    fn a_query_longer_than_a_candidate_never_matches_it() {
        assert_eq!(completion_match("ab", "abc"), None);
    }

    #[test]
    fn completion_item_display_trims_and_drops_a_real_blank_detail() {
        let item = lsp_types::CompletionItem {
            label: "foo".to_string(),
            detail: Some("  fn() -> i32  ".to_string()),
            ..Default::default()
        };
        let (label, detail) = completion_item_display(&item);
        assert_eq!(label, "foo");
        assert_eq!(detail.as_deref(), Some("fn() -> i32"));

        let no_detail_item = lsp_types::CompletionItem {
            label: "bar".to_string(),
            detail: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(completion_item_display(&no_detail_item).1, None);
    }

    /// The real, live-reported bug this session: a server (typescript-language-server, in
    /// practice) sending a legacy top-level `detail` that mixes a qualifier and a type together
    /// (`"(property) Foo.bar: string"`) must not win over a real `label_details.detail` the same
    /// item also sent - the row's own right-side hint should read as the clean, spec-designed
    /// type fragment (`"string"`), not the mixed legacy string.
    #[test]
    fn completion_item_display_prefers_a_real_label_details_detail_over_the_legacy_mixed_detail_string()
     {
        let item = lsp_types::CompletionItem {
            label: "bar".to_string(),
            detail: Some("(property) Foo.bar: string".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(": string".to_string()),
                description: Some("Foo".to_string()),
            }),
            ..Default::default()
        };
        let (label, detail) = completion_item_display(&item);
        assert_eq!(label, "bar");
        assert_eq!(
            detail.as_deref(),
            Some(": string"),
            "the row's own right-side hint must read from the real label_details.detail field, \
             never the legacy detail string it was designed to replace"
        );
    }

    #[test]
    fn completion_item_display_falls_back_to_the_legacy_detail_without_real_label_details() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn(&mut self, &str)".to_string()),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1.as_deref(),
            Some("fn(&mut self, &str)"),
            "a server that never sent label_details at all must still get a real row hint from \
             the legacy detail field"
        );
    }

    #[test]
    fn completion_item_display_falls_back_to_legacy_detail_when_label_details_detail_is_blank() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn(&mut self, &str)".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: None,
                description: Some("alloc::string::String".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1.as_deref(),
            Some("fn(&mut self, &str)"),
            "a real label_details with no detail field of its own must not suppress the legacy \
             detail string - only a real label_details.detail should ever win"
        );
    }

    #[test]
    fn completion_signature_text_composes_the_label_with_a_real_label_details_detail() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("this legacy string must lose to label_details".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("(&mut self, string: &str)".to_string()),
                description: Some("alloc::string::String".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_signature_text(&item),
            "push_str(&mut self, string: &str)",
            "the pane's own signature line must compose label + label_details.detail (no space, \
             per that field's own spec) whenever a server sent one, never the legacy detail string"
        );
    }

    #[test]
    fn completion_signature_text_falls_back_to_the_legacy_detail_string() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn push_str(&mut self, string: &str)".to_string()),
            ..Default::default()
        };
        assert_eq!(
            completion_signature_text(&item),
            "fn push_str(&mut self, string: &str)",
            "rust-analyzer's own convention - a real, complete, standalone signature string in \
             the legacy detail field - must still be used verbatim for a server that never sent \
             label_details"
        );
    }

    #[test]
    fn completion_signature_text_falls_back_to_the_bare_label_with_nothing_else_real() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            ..Default::default()
        };
        assert_eq!(completion_signature_text(&item), "push_str");
    }

    #[test]
    fn completion_documentation_text_reads_a_real_plain_string_shape() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            documentation: Some(lsp_types::Documentation::String(
                "Appends a given string slice.".to_string(),
            )),
            ..Default::default()
        };
        assert_eq!(
            completion_documentation_text(&item).as_deref(),
            Some("Appends a given string slice.")
        );
    }

    #[test]
    fn completion_documentation_text_degrades_a_real_markdown_markup_content() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            documentation: Some(lsp_types::Documentation::MarkupContent(
                lsp_types::MarkupContent {
                    kind: lsp_types::MarkupKind::Markdown,
                    value: "Appends a **given** string slice.".to_string(),
                },
            )),
            ..Default::default()
        };
        assert_eq!(
            completion_documentation_text(&item).as_deref(),
            Some("Appends a given string slice.")
        );
    }

    #[test]
    fn completion_documentation_text_passes_a_real_plain_text_markup_content_through_unmodified() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            documentation: Some(lsp_types::Documentation::MarkupContent(
                lsp_types::MarkupContent {
                    kind: lsp_types::MarkupKind::PlainText,
                    value: "`not markdown` stays as-is".to_string(),
                },
            )),
            ..Default::default()
        };
        assert_eq!(
            completion_documentation_text(&item).as_deref(),
            Some("`not markdown` stays as-is")
        );
    }

    #[test]
    fn completion_documentation_text_is_none_for_a_real_absent_or_blank_documentation() {
        let no_doc = lsp_types::CompletionItem {
            label: "foo".to_string(),
            ..Default::default()
        };
        assert_eq!(completion_documentation_text(&no_doc), None);

        let blank_doc = lsp_types::CompletionItem {
            label: "foo".to_string(),
            documentation: Some(lsp_types::Documentation::String("   ".to_string())),
            ..Default::default()
        };
        assert_eq!(completion_documentation_text(&blank_doc), None);
    }

    #[test]
    fn completion_module_path_reads_a_real_label_details_description() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("(&mut self, &str)".to_string()),
                description: Some("alloc::string::String".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_module_path(&item).as_deref(),
            Some("alloc::string::String")
        );
    }

    #[test]
    fn completion_module_path_is_none_without_real_label_details_or_a_real_description() {
        let no_label_details = lsp_types::CompletionItem {
            label: "foo".to_string(),
            ..Default::default()
        };
        assert_eq!(completion_module_path(&no_label_details), None);

        let no_description = lsp_types::CompletionItem {
            label: "foo".to_string(),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("()".to_string()),
                description: None,
            }),
            ..Default::default()
        };
        assert_eq!(completion_module_path(&no_description), None);
    }
}
