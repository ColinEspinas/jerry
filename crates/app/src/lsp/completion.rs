//! Pure logic for Surface C's File view real Completions popup (Revision R8.5b) - deciding
//! *when* a real edit is completion-worthy, and turning a real `lsp_types::CompletionItem` into
//! the real byte range/text an [`crate::code_surface::edit_buffer::EditBuffer`] mutation should splice in if
//! accepted. Deliberately `gpui`-free (no `gpui` type appears anywhere in this module), mirroring
//! `crate::lsp::hover`'s own split between pure logic here and `crate::root`'s live request
//! dispatch/popover painting - see that module's own top doc comment for the same convention.

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
pub fn completion_item_display(item: &lsp_types::CompletionItem) -> (String, Option<String>) {
    (item.label.clone(), split_completion_detail(item).signature)
}

/// The real, per-slot split of whatever a server packed into `CompletionItem`'s three descriptive
/// fields.
struct CompletionDetail {
    /// The type/signature slot - `None` when the server sent nothing that is genuinely one.
    signature: Option<String>,
    /// The module-path footer - `None` when the server sent no real path.
    module_path: Option<String>,
}

fn split_completion_detail(item: &lsp_types::CompletionItem) -> CompletionDetail {
    let label_details = item.label_details.as_ref();
    let description = label_details
        .and_then(|label_details| label_details.description.as_deref())
        .map(str::trim)
        .filter(|description| !description.is_empty() && *description != item.label);
    // `pyright-langserver` puts no module path in `detail` at all - just the literal marker
    // `"Auto-import"` - and names the real module in `description`. See
    // [`detail_is_auto_import_marker`].
    let described_path = description
        .filter(|description| {
            looks_like_module_path(description) || detail_is_auto_import_marker(item)
        })
        .map(str::to_string);
    let use_note_path = label_details
        .and_then(|label_details| label_details.detail.as_deref())
        .and_then(use_note_module_path);
    let annotated_path = use_note_path.or(described_path);

    let Some(raw) = item
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
    else {
        return CompletionDetail {
            signature: None,
            module_path: annotated_path,
        };
    };

    let (import_path, rest) = split_auto_import_note(raw);
    let cleaned = clean_completion_detail(rest, &item.label);
    let cleaned = cleaned.trim();

    // A detail that is just the label again carries no information the row doesn't already show -
    // and neither does `pyright`'s bare `"Auto-import"` marker, which names no type of any kind.
    let redundant =
        cleaned.is_empty() || cleaned == item.label || detail_is_auto_import_marker(item);
    let detail_is_path = !redundant && detail_is_module_specifier(cleaned, item.kind);

    CompletionDetail {
        signature: (!redundant && !detail_is_path).then(|| cleaned.to_string()),
        module_path: import_path
            .or_else(|| detail_is_path.then(|| cleaned.to_string()))
            .or(annotated_path),
    }
}

/// Whether this item's `detail` is `pyright-langserver`'s own literal `"Auto-import"` marker,
/// which is neither a type nor a path and must not be printed as either.
fn detail_is_auto_import_marker(item: &lsp_types::CompletionItem) -> bool {
    item.detail.as_deref().map(str::trim) == Some("Auto-import")
}

/// The path out of `rust-analyzer`'s own real `"(use std::io::Result)"` import note, which it puts
/// in `CompletionItemLabelDetails::detail` - `Some("std::io::Result")` here, and `None` for
/// anything else that field carries.
fn use_note_module_path(note: &str) -> Option<String> {
    let after_use = note.split_once("(use ")?.1;
    let path = after_use[..after_use.find(')')?].trim();
    looks_like_module_path(path).then(|| path.to_string())
}

/// Splits `typescript-language-server`'s own real two-line auto-import detail into the import path
/// and the signature that follows it (`"Auto import from './helper'\nconstructor RemoteHelper():
/// RemoteHelper"` -> `(Some("./helper"), "constructor RemoteHelper(): RemoteHelper")`), captured
/// verbatim from a real resolved completion against a live server.
fn split_auto_import_note(detail: &str) -> (Option<String>, &str) {
    let (first_line, rest) = match detail.split_once('\n') {
        Some((first_line, rest)) => (first_line.trim(), rest.trim()),
        None => (detail.trim(), ""),
    };
    let Some(path) = first_line
        .strip_prefix("Auto import from ")
        .map(|path| path.trim().trim_matches(['\'', '"']).trim())
        .filter(|path| !path.is_empty())
    else {
        return (None, detail);
    };
    (Some(path.to_string()), rest)
}

/// Whether `text` genuinely reads as a module path/import specifier rather than a type or
/// signature - a deliberately narrow test, since its whole job is to keep a path *out* of the type
/// slot without ever exiling a real type into the footer.
fn looks_like_module_path(text: &str) -> bool {
    !text.is_empty()
        && !text.contains(char::is_whitespace)
        && !text.contains(['(', ')', '<', '>', '{', '}', '!', ','])
        && (text.contains("::") || text.contains('/'))
}

/// The same question for `CompletionItem::detail` specifically, where a real separator is *not*
/// required - the live-reported "the types are loading only after in a weird way replacing the
/// module names".
fn detail_is_module_specifier(text: &str, kind: Option<lsp_types::CompletionItemKind>) -> bool {
    if looks_like_module_path(text) {
        return true;
    }
    !text.is_empty()
        && !text.contains(char::is_whitespace)
        && !text.contains(['(', ')', '<', '>', '{', '}', '!', ','])
        && kind_has_no_one_word_type(kind)
}

/// Whether an item of this kind could ever have a genuine *one-word* type/signature in `detail` -
/// `false` here means a separator-less `detail` on such an item can only be a module specifier.
fn kind_has_no_one_word_type(kind: Option<lsp_types::CompletionItemKind>) -> bool {
    matches!(
        kind,
        Some(
            lsp_types::CompletionItemKind::FUNCTION
                | lsp_types::CompletionItemKind::METHOD
                | lsp_types::CompletionItemKind::CONSTRUCTOR
                | lsp_types::CompletionItemKind::CLASS
                | lsp_types::CompletionItemKind::INTERFACE
                | lsp_types::CompletionItemKind::ENUM
                | lsp_types::CompletionItemKind::STRUCT
                | lsp_types::CompletionItemKind::MODULE
        )
    )
}

/// The Completions detail pane's own real signature line (its top band, syntax-highlighted in
/// mono - `design_handoff_jerry_ade/revision 3/Jerry.dc.html`'s `fn push_str(&mut self, string:
/// &str)` example) - the same [`CompletionDetail::signature`] [`completion_item_display`] reads
/// (see [`split_completion_detail`]'s own docs for why `label_details.detail` is deliberately not
/// consulted here), falling back to the bare `label` for an item with no real
/// detail at all (a bare `label`/`kind`-only item that hasn't been resolved yet, or a server that
/// never sends one for this item's own kind).
pub fn completion_signature_text(item: &lsp_types::CompletionItem) -> String {
    split_completion_detail(item)
        .signature
        .unwrap_or_else(|| item.label.clone())
}

/// Strips `typescript-language-server`'s own real, live-observed `"(kind) Qualifier.member(...)"`
/// convention (e.g. `"(method) QueryBuilder.pushStr(s: string): void"` -> `"pushStr(s: string):
/// void"`, `"(property) Foo.bar: string"` -> `"bar: string"`) from a completion item's legacy
/// `detail` string, when present - the real, server-baked-in source of "a weird mix of modules
/// and typing" a user reported seeing, with no separate structured field (see
/// [`completion_item_display`]'s own docs) this app could read the clean half from instead. Two
/// independent, both-conservative steps:
pub fn clean_completion_detail(detail: &str, label: &str) -> String {
    let without_kind_prefix = strip_leading_kind_parenthetical(detail);
    if let Some(label_start) = without_kind_prefix.find(label) {
        if label_start > 0 && label_start <= without_kind_prefix.len().min(80) {
            let before = &without_kind_prefix[..label_start];
            if before.ends_with('.') && !before[..before.len() - 1].contains(char::is_whitespace) {
                return without_kind_prefix[label_start..].to_string();
            }
        }
    }
    without_kind_prefix.to_string()
}

/// The first real step [`clean_completion_detail`] runs - see that function's own docs for the
/// real shape this looks for and why a genuine parameter-list/tuple-type parenthetical is never
/// mistaken for one.
fn strip_leading_kind_parenthetical(detail: &str) -> &str {
    let Some(inner) = detail.strip_prefix('(') else {
        return detail;
    };
    let Some(close) = inner.find(')') else {
        return detail;
    };
    let kind_word = &inner[..close];
    if kind_word.is_empty()
        || kind_word.contains(':')
        || kind_word.contains(',')
        || kind_word.contains('(')
    {
        return detail;
    }
    let rest = inner[close + 1..].trim_start();
    if rest.is_empty() {
        detail
    } else {
        rest
    }
}

/// A completion item's real doc prose, for the detail pane's own body text
/// (`design_handoff_jerry_ade/revision 3/README.md`: "Right 300: signature in mono, doc in 11px
/// Plex Sans #7d848b, module path footer" - the Completions popup's own doc/module-path pane,
/// mirroring `crate::lsp::hover::HoverRenderModel::doc` exactly). `None` for an item with no real
/// documentation - an honest "nothing to show", never a fabricated empty string.
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
pub fn completion_module_path(item: &lsp_types::CompletionItem) -> Option<String> {
    split_completion_detail(item).module_path
}

/// The module a completion *row* says this item would be imported from - the same real path
/// [`completion_module_path`] gives the detail pane's footer, surfaced on the row itself.
pub fn completion_import_source(item: &lsp_types::CompletionItem) -> Option<String> {
    completion_module_path(item)
}

/// The one secondary string a completion **row** shows beside its label - the origin/module the
/// item comes from when the server named one, and the signature it sent inline otherwise.
pub fn completion_row_hint(item: &lsp_types::CompletionItem) -> Option<String> {
    completion_import_source(item).or_else(|| split_completion_detail(item).signature)
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
    /// The query *is* the candidate, ignoring case (`app` against `app`, or against `App`) - the
    /// user has already typed the whole identifier, and nothing that merely starts with it can be
    /// a better answer.
    Exact,
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
            tier: if start == 0 && len == haystack.len() {
                CompletionMatchTier::Exact
            } else if start == 0 {
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
    #[allow(clippy::expect_used)]
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
pub fn rank_completion_items(items: &[lsp_types::CompletionItem], query: &str) -> Vec<usize> {
    let server_ranks = items.iter().any(|item| item.sort_text.is_some());
    let sort_text = |item: &'_ lsp_types::CompletionItem| -> String {
        if server_ranks {
            item.sort_text.clone().unwrap_or_else(|| item.label.clone())
        } else {
            String::new()
        }
    };
    let mut scored: Vec<(CompletionMatch, String, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            completion_match(completion_filter_text(item), query)
                .map(|score| (score, sort_text(item), index))
        })
        .collect();
    scored.sort();
    let mut interchangeable = std::collections::HashSet::with_capacity(scored.len());
    let ranked: Vec<usize> = scored
        .into_iter()
        .map(|(_, _, index)| index)
        .filter(|index| interchangeable.insert(interchangeable_completion_key(&items[*index])))
        .collect();

    // Each group keeps its best-ranked row's *position*, but not necessarily that row's item - see
    // `same_choice_key` and `prefers_this_spelling`.
    let mut group_at: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
    let mut kept: Vec<usize> = Vec::with_capacity(ranked.len());
    for index in ranked {
        let key = same_choice_key(&items[index]);
        match group_at.get(&key) {
            Some(&slot) => {
                if prefers_this_spelling(&items[index], &items[kept[slot]]) {
                    kept[slot] = index;
                }
            }
            None => {
                group_at.insert(key, kept.len());
                kept.push(index);
            }
        }
    }
    kept
}

/// Of two candidates that would write the same import, whether `candidate` is the one to show -
/// which for Node's two spellings of a builtin means the `node:`-prefixed one.
fn prefers_this_spelling(
    candidate: &lsp_types::CompletionItem,
    incumbent: &lsp_types::CompletionItem,
) -> bool {
    let is_prefixed = |item: &lsp_types::CompletionItem| {
        completion_import_source(item).is_some_and(|source| source.starts_with("node:"))
    };
    is_prefixed(candidate) && !is_prefixed(incumbent)
}

/// What a completion row actually offers a user: this identifier, of this kind, spliced in as this
/// text, out of this module. Two rows with equal keys put the same word in the file *and* import it
/// from the same place - so there is genuinely nothing to choose between them, and the second is
/// noise on an already-crowded list.
fn same_choice_key(item: &lsp_types::CompletionItem) -> (String, String, String, Option<String>) {
    let inserted = match item.text_edit.as_ref() {
        Some(lsp_types::CompletionTextEdit::Edit(edit)) => edit.new_text.clone(),
        Some(lsp_types::CompletionTextEdit::InsertAndReplace(edit)) => edit.new_text.clone(),
        None => item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone()),
    };
    // `CompletionItemKind` is neither `Hash` nor castable to an integer, and its `Debug` is the
    // real variant name - a stable, unambiguous identity for a key that never leaves this pass.
    (
        item.label.clone(),
        format!("{:?}", item.kind),
        inserted,
        completion_import_source(item)
            .as_deref()
            .map(canonical_import_source),
    )
}

/// One module, spelled one way: the `node:` prefix dropped, and nothing else touched.
fn canonical_import_source(source: &str) -> String {
    source.strip_prefix("node:").unwrap_or(source).to_string()
}

/// Everything about a completion item that a user can either *see* on its row or *get* by
/// accepting it. Two items with equal keys offer no choice at all: they paint the same row and
/// splice the same text over the same range, so a second row for the second one is pure noise.
fn interchangeable_completion_key(item: &lsp_types::CompletionItem) -> String {
    #[derive(serde::Serialize)]
    struct Key<'a> {
        label: &'a str,
        kind: Option<lsp_types::CompletionItemKind>,
        detail: Option<&'a String>,
        label_details: Option<&'a lsp_types::CompletionItemLabelDetails>,
        filter_text: Option<&'a String>,
        insert_text: Option<&'a String>,
        insert_text_format: Option<lsp_types::InsertTextFormat>,
        insert_text_mode: Option<lsp_types::InsertTextMode>,
        text_edit: Option<&'a lsp_types::CompletionTextEdit>,
        additional_text_edits: Option<&'a Vec<lsp_types::TextEdit>>,
        command: Option<&'a lsp_types::Command>,
    }
    let key = Key {
        label: &item.label,
        kind: item.kind,
        detail: item.detail.as_ref(),
        label_details: item.label_details.as_ref(),
        filter_text: item.filter_text.as_ref(),
        insert_text: item.insert_text.as_ref(),
        insert_text_format: item.insert_text_format,
        insert_text_mode: item.insert_text_mode,
        text_edit: item.text_edit.as_ref(),
        additional_text_edits: item.additional_text_edits.as_ref(),
        command: item.command.as_ref(),
    };
    // A `CompletionItem`'s own fields all round-trip through `serde_json` by construction (that is
    // how they arrived off the wire), so this cannot realistically fail. If it somehow did, an
    // unforgeable per-item key is the honest fallback: it keeps the item rather than silently
    // collapsing it into an unrelated one.
    serde_json::to_string(&key).unwrap_or_else(|_| format!("\u{0}unserializable-{:p}", item))
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
    fn two_rows_a_user_cannot_tell_apart_or_choose_between_collapse_into_one() {
        let deref_twin = |doc: &str| lsp_types::CompletionItem {
            label: "len".to_string(),
            kind: Some(lsp_types::CompletionItemKind::METHOD),
            detail: Some("const fn(&self) -> usize".to_string()),
            filter_text: Some("len".to_string()),
            sort_text: Some("7fffffff".to_string()),
            documentation: Some(lsp_types::Documentation::String(doc.to_string())),
            ..Default::default()
        };
        let items = [
            deref_twin("Returns the length of this `String`, in bytes."),
            deref_twin("Returns the length of `self`."),
            item("lines_any"),
        ];
        assert_eq!(
            rank_completion_items(&items, "len"),
            vec![0, 2],
            "the second `len` is interchangeable with the first in everything the user can see or \
             act on, so it must not occupy a row of its own"
        );
    }

    #[test]
    fn several_import_candidates_for_one_name_each_keep_their_row() {
        let import_candidate = |path: &str| lsp_types::CompletionItem {
            label: "Result".to_string(),
            kind: Some(lsp_types::CompletionItemKind::STRUCT),
            filter_text: Some("Result".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(format!("(use {path})")),
                description: None,
            }),
            ..Default::default()
        };
        let items = [
            import_candidate("std::fmt::Result"),
            import_candidate("std::io::Result"),
            import_candidate("std::thread::Result"),
        ];
        assert_eq!(
            rank_completion_items(&items, "Result"),
            vec![0, 1, 2],
            "three different `use` statements are three real choices - collapsing them would hide \
             two and add whichever import ranked first"
        );
    }

    #[test]
    fn an_identical_looking_row_with_a_different_edit_keeps_its_own_row() {
        let with_insert = |insert: &str| lsp_types::CompletionItem {
            label: "new".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            insert_text: Some(insert.to_string()),
            ..Default::default()
        };
        let items = [with_insert("new()"), with_insert("new($1)")];
        assert_eq!(rank_completion_items(&items, "new"), vec![0, 1]);
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

    #[test]
    fn completion_item_display_ignores_a_real_rust_analyzer_trait_source_label_details_detail() {
        let item = lsp_types::CompletionItem {
            label: "into".to_string(),
            detail: Some("fn(self) -> T".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("(as Into)".to_string()),
                description: Some("fn(self) -> T".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1.as_deref(),
            Some("fn(self) -> T"),
            "the row's own right-side hint must read the real legacy detail string, never the \
             trait-source annotation rust-analyzer puts in label_details.detail"
        );
    }

    #[test]
    fn completion_item_display_falls_back_to_the_bare_label_with_no_real_detail_at_all() {
        let item = lsp_types::CompletionItem {
            label: "push_str".to_string(),
            ..Default::default()
        };
        assert_eq!(completion_item_display(&item).1, None);
    }

    #[test]
    fn completion_signature_text_ignores_label_details_and_cleans_the_legacy_detail() {
        let item = lsp_types::CompletionItem {
            label: "pushStr".to_string(),
            detail: Some("(method) QueryBuilder.pushStr(s: string): void".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("this must never appear in the signature line".to_string()),
                description: Some("also never".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_signature_text(&item),
            "pushStr(s: string): void",
            "the pane's own signature line must clean the real legacy detail string, never read \
             label_details at all"
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
            "rust-analyzer's own convention - a real, complete, standalone signature string \
             with no kind/qualifier prefix to clean - must pass through unchanged"
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
    fn a_real_typescript_auto_import_detail_splits_into_a_real_signature_and_a_real_path() {
        let item = lsp_types::CompletionItem {
            label: "RemoteHelper".to_string(),
            kind: Some(lsp_types::CompletionItemKind::CLASS),
            detail: Some(
                "Auto import from './helper'\nconstructor RemoteHelper(): RemoteHelper".to_string(),
            ),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1.as_deref(),
            Some("constructor RemoteHelper(): RemoteHelper"),
            "the row's type hint must show the real signature, not the import note above it"
        );
        assert_eq!(
            completion_signature_text(&item),
            "constructor RemoteHelper(): RemoteHelper"
        );
        assert_eq!(
            completion_module_path(&item).as_deref(),
            Some("./helper"),
            "the import path is a real module path and belongs in the footer"
        );
    }

    #[test]
    fn a_real_unresolved_typescript_auto_import_shows_no_type_and_a_real_path() {
        let item = lsp_types::CompletionItem {
            label: "RemoteHelper".to_string(),
            kind: Some(lsp_types::CompletionItemKind::CLASS),
            detail: Some("./helper".to_string()),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1,
            None,
            "a bare module specifier must never be rendered as this item's type"
        );
        assert_eq!(completion_module_path(&item).as_deref(), Some("./helper"));
    }

    #[test]
    fn a_real_bare_package_specifier_is_a_module_path_not_a_type() {
        let item = lsp_types::CompletionItem {
            label: "createProgram".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some("typescript".to_string()),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1,
            None,
            "a bare package specifier is a module, not this function's type - showing it in the \
             type slot is exactly the module name the resolve response was then seen to replace"
        );
        assert_eq!(
            completion_module_path(&item).as_deref(),
            Some("typescript"),
            "it belongs in the footer that exists for module paths"
        );
    }

    #[test]
    fn resolving_a_bare_package_import_only_adds_a_type_and_keeps_the_same_path() {
        let resolved = lsp_types::CompletionItem {
            label: "createProgram".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some(
                "Auto import from 'typescript'\nfunction ts.createProgram(rootNames: readonly \
                 string[], options: ts.CompilerOptions): ts.Program"
                    .to_string(),
            ),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&resolved).1.as_deref(),
            Some(
                "function ts.createProgram(rootNames: readonly string[], options: \
                 ts.CompilerOptions): ts.Program"
            )
        );
        assert_eq!(
            completion_module_path(&resolved).as_deref(),
            Some("typescript"),
            "the footer must read the same module before and after the resolve, so the pane's \
             module path never visibly changes under the user"
        );
    }

    #[test]
    fn a_real_one_word_field_type_is_still_a_type() {
        for (label, kind, detail) in [
            ("count", lsp_types::CompletionItemKind::FIELD, "usize"),
            ("name", lsp_types::CompletionItemKind::FIELD, "String"),
            ("on", lsp_types::CompletionItemKind::FIELD, "bool"),
            ("w", lsp_types::CompletionItemKind::VARIABLE, "Widget"),
        ] {
            let item = lsp_types::CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail: Some(detail.to_string()),
                ..Default::default()
            };
            assert_eq!(
                completion_item_display(&item).1.as_deref(),
                Some(detail),
                "{label}: a real one-word type must stay in the type slot"
            );
            assert_eq!(
                completion_module_path(&item),
                None,
                "{label}: and must never be exiled into the module-path footer"
            );
        }
    }

    #[test]
    fn a_real_rust_analyzer_type_completion_does_not_echo_its_own_label_as_a_type() {
        let item = lsp_types::CompletionItem {
            label: "Widget".to_string(),
            kind: Some(lsp_types::CompletionItemKind::STRUCT),
            detail: Some("Widget".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: None,
                description: Some("Widget".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(completion_item_display(&item).1, None);
        assert_eq!(
            completion_module_path(&item),
            None,
            "a bare type name is not a module path, however the server labelled the field"
        );
    }

    #[test]
    fn a_real_rust_analyzer_signature_is_never_mistaken_for_a_module_path() {
        let item = lsp_types::CompletionItem {
            label: "into".to_string(),
            kind: Some(lsp_types::CompletionItemKind::METHOD),
            detail: Some("fn(self) -> T".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("(as Into)".to_string()),
                description: Some("fn(self) -> T".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_item_display(&item).1.as_deref(),
            Some("fn(self) -> T")
        );
        assert_eq!(completion_module_path(&item), None);
    }

    #[test]
    fn a_real_qualified_path_description_still_reaches_the_footer() {
        let item = lsp_types::CompletionItem {
            label: "HashMap".to_string(),
            kind: Some(lsp_types::CompletionItemKind::STRUCT),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: None,
                description: Some("std::collections::HashMap".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_module_path(&item).as_deref(),
            Some("std::collections::HashMap")
        );
    }

    #[test]
    fn a_real_pyright_auto_import_marker_is_neither_a_type_nor_a_module_path() {
        for (label, kind) in [
            ("_path", lsp_types::CompletionItemKind::MODULE),
            ("path", lsp_types::CompletionItemKind::VARIABLE),
            ("PathLike", lsp_types::CompletionItemKind::CLASS),
            ("P_NOWAIT", lsp_types::CompletionItemKind::CONSTANT),
        ] {
            let item = lsp_types::CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail: Some("Auto-import".to_string()),
                label_details: Some(lsp_types::CompletionItemLabelDetails {
                    detail: None,
                    description: Some("os".to_string()),
                }),
                ..Default::default()
            };
            assert_eq!(
                completion_item_display(&item).1,
                None,
                "{label}: `Auto-import` is a marker, not this item's type"
            );
            assert_eq!(
                completion_import_source(&item).as_deref(),
                Some("os"),
                "{label}: the module it would import from is the real one the server named, not \
                 the marker"
            );
        }
    }

    #[test]
    fn the_real_reported_app_completion_list_comes_out_readable() {
        let item =
            |label: &str, kind: lsp_types::CompletionItemKind, sort: &str, detail: Option<&str>| {
                lsp_types::CompletionItem {
                    label: label.to_string(),
                    kind: Some(kind),
                    sort_text: Some(sort.to_string()),
                    detail: detail.map(str::to_string),
                    ..Default::default()
                }
            };
        use lsp_types::CompletionItemKind as K;
        let auto = "\u{ffff}16";
        let items = [
            item("appendFile", K::FUNCTION, auto, Some("fs")),
            item("appendFile", K::FUNCTION, auto, Some("node:fs")),
            item("appendFile", K::FUNCTION, auto, Some("fs/promises")),
            item("appendFile", K::FUNCTION, auto, Some("node:fs/promises")),
            item("appendFileSync", K::FUNCTION, auto, Some("fs")),
            item("appendFileSync", K::FUNCTION, auto, Some("node:fs")),
            item("asyncWrapProviders", K::MODULE, auto, Some("async_hooks")),
            item(
                "asyncWrapProviders",
                K::MODULE,
                auto,
                Some("node:async_hooks"),
            ),
            item("AudioParamMap", K::VARIABLE, "15", None),
            item("app", K::VARIABLE, "11", None),
            item("App", K::VARIABLE, "11", None),
            item("createApp", K::VARIABLE, "11", None),
            item("SearchApplication", K::CLASS, "15", None),
        ];
        let ranked: Vec<&str> = rank_completion_items(&items, "app")
            .into_iter()
            .map(|index| items[index].label.as_str())
            .collect();
        assert_eq!(
            ranked,
            vec![
                "app",
                "App",
                "appendFile", // from `fs`, folding in the `node:fs` spelling of it
                "appendFile", // from `fs/promises` - a different module, a real choice
                "appendFileSync",
                "createApp",
                "SearchApplication",
                "asyncWrapProviders",
                "AudioParamMap",
            ],
            "the two symbols the user fully typed must lead; nine rows of auto-import must come \
             down to the five real choices behind them - never fewer, so `fs/promises` keeps its \
             own row against `fs` - and nothing the server ranked lowest may sit above what it \
             ranked highest"
        );
    }

    #[test]
    fn a_fully_typed_candidate_outranks_one_that_merely_starts_with_it() {
        let bare = |label: &str| lsp_types::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        };
        let items = [bare("appendFile"), bare("App"), bare("app")];
        let ranked: Vec<&str> = rank_completion_items(&items, "app")
            .into_iter()
            .map(|index| items[index].label.as_str())
            .collect();
        assert_eq!(ranked, vec!["app", "App", "appendFile"]);
    }

    #[test]
    fn a_response_with_no_sort_text_at_all_keeps_the_servers_own_order() {
        let bare = |label: &str| lsp_types::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        };
        let items = [
            bare("zebra_value"),
            bare("alpha_value"),
            bare("middle_value"),
        ];
        let ranked: Vec<&str> = rank_completion_items(&items, "value")
            .into_iter()
            .map(|index| items[index].label.as_str())
            .collect();
        assert_eq!(
            ranked,
            vec!["zebra_value", "alpha_value", "middle_value"],
            "no item claimed a rank, so the server's own order is the only real signal there is"
        );
    }

    #[test]
    fn a_real_server_sort_text_outranks_the_responses_own_arbitrary_order() {
        let ranked_item = |label: &str, sort: &str| lsp_types::CompletionItem {
            label: label.to_string(),
            sort_text: Some(sort.to_string()),
            ..Default::default()
        };
        let items = [
            ranked_item("valueFromNodeTypes", "\u{ffff}16"),
            ranked_item("valueInScope", "11"),
        ];
        let ranked: Vec<&str> = rank_completion_items(&items, "value")
            .into_iter()
            .map(|index| items[index].label.as_str())
            .collect();
        assert_eq!(ranked, vec!["valueInScope", "valueFromNodeTypes"]);
    }

    #[test]
    fn two_same_named_exports_of_unrelated_packages_keep_their_own_rows() {
        let from = |module: &str| lsp_types::CompletionItem {
            label: "Ref".to_string(),
            kind: Some(lsp_types::CompletionItemKind::INTERFACE),
            detail: Some(module.to_string()),
            sort_text: Some("\u{ffff}16".to_string()),
            ..Default::default()
        };
        let items = [from("vue"), from("react"), from("preact")];
        assert_eq!(
            rank_completion_items(&items, "Ref"),
            vec![0, 1, 2],
            "three packages exporting a `Ref` are three genuinely different symbols - merging \
             them would hide two and import a third the user never chose"
        );

        // ...while the entry points of one package still collapse, which is the case that was
        // reported first. Verbatim from a live `typescript-language-server`.
        let node = |module: &str| lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some(module.to_string()),
            sort_text: Some("\u{ffff}16".to_string()),
            ..Default::default()
        };
        let node_items = [
            node("node:fs"),
            node("fs"),
            node("fs/promises"),
            node("node:fs/promises"),
        ];
        assert_eq!(
            rank_completion_items(&node_items, "app"),
            vec![0, 3],
            "four candidates, two real choices: `fs` and `node:fs` write the same import, and so \
             do `fs/promises` and `node:fs/promises` - but the callback API and the promise API \
             are two different things to pick between. Each surviving row is its group's `node:` \
             spelling, whichever order that arrived in"
        );
    }

    #[test]
    fn the_node_prefixed_spelling_is_the_row_that_survives_whatever_order_it_arrives_in() {
        let candidate = |module: &str| lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some(module.to_string()),
            sort_text: Some("\u{ffff}16".to_string()),
            ..Default::default()
        };

        let items = [candidate("node:fs"), candidate("fs")];
        let ranked = rank_completion_items(&items, "app");
        assert_eq!(ranked, vec![0]);
        assert_eq!(
            completion_row_hint(&items[ranked[0]]).as_deref(),
            Some("node:fs")
        );

        let items = [candidate("fs"), candidate("node:fs")];
        let ranked = rank_completion_items(&items, "app");
        assert_eq!(
            ranked,
            vec![1],
            "the surviving row must be the `node:` one even when the bare spelling arrives first"
        );
        assert_eq!(
            completion_row_hint(&items[ranked[0]]).as_deref(),
            Some("node:fs"),
            "`node:` is unshadowable and is the only spelling Node's newer builtins have"
        );
    }

    #[test]
    fn preferring_a_spelling_does_not_move_the_row_up_or_down_the_list() {
        let candidate = |label: &str, module: Option<&str>, sort: &str| lsp_types::CompletionItem {
            label: label.to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: module.map(str::to_string),
            sort_text: Some(sort.to_string()),
            ..Default::default()
        };
        let items = [
            candidate("appendFile", Some("fs"), "\u{ffff}16"),
            candidate("appendZebra", None, "\u{ffff}17"),
            candidate("appendFile", Some("node:fs"), "\u{ffff}16"),
        ];
        let ranked = rank_completion_items(&items, "append");
        assert_eq!(
            ranked,
            vec![2, 1],
            "the `appendFile` group keeps the first slot its own best rank earned - it must not \
             fall behind `appendZebra` just because the spelling that won arrived later"
        );
    }

    #[test]
    fn only_nodes_two_spellings_of_one_module_are_the_same_import() {
        for (source, canonical) in [
            ("node:fs", "fs"),
            ("node:fs/promises", "fs/promises"),
            ("node:async_hooks", "async_hooks"),
            ("fs", "fs"),
            ("fs/promises", "fs/promises"),
            ("std::io::Result", "std::io::Result"),
            ("std::fmt::Result", "std::fmt::Result"),
            ("os", "os"),
            ("os.path", "os.path"),
            ("typing", "typing"),
            ("typing_extensions", "typing_extensions"),
            ("vue", "vue"),
            ("@vue/reactivity", "@vue/reactivity"),
        ] {
            assert_eq!(canonical_import_source(source), canonical, "{source}");
        }
    }

    #[test]
    fn two_rows_that_insert_different_text_are_never_collapsed() {
        let with_insert = |insert: &str| lsp_types::CompletionItem {
            label: "new".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            insert_text: Some(insert.to_string()),
            ..Default::default()
        };
        let items = [with_insert("new()"), with_insert("new($1)")];
        assert_eq!(rank_completion_items(&items, "new"), vec![0, 1]);
    }

    #[test]
    fn a_real_auto_import_row_says_which_module_it_comes_from_and_is_not_repeated() {
        let candidate = |module: &str| lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some(module.to_string()),
            sort_text: Some("\u{ffff}16".to_string()),
            ..Default::default()
        };
        let items = [
            candidate("node:fs"),
            candidate("fs"),
            candidate("fs/promises"),
        ];
        assert_eq!(
            rank_completion_items(&items, "app"),
            vec![0, 2],
            "`node:fs` and `fs` would write the same import and are one row; `fs/promises` is a \
             different module and keeps its own"
        );
        assert_eq!(
            completion_row_hint(&items[0]).as_deref(),
            Some("node:fs"),
            "and each row names its own origin, up front, with no resolve needed"
        );
        assert_eq!(
            completion_row_hint(&items[2]).as_deref(),
            Some("fs/promises")
        );
    }

    #[test]
    fn a_real_rust_analyzer_row_shows_the_signature_it_was_sent_inline() {
        for (label, kind, detail) in [
            (
                "len",
                lsp_types::CompletionItemKind::METHOD,
                "const fn(&self) -> usize",
            ),
            ("count", lsp_types::CompletionItemKind::FIELD, "usize"),
        ] {
            let item = lsp_types::CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail: Some(detail.to_string()),
                ..Default::default()
            };
            assert_eq!(
                completion_row_hint(&item).as_deref(),
                Some(detail),
                "{label}: the server sent this up front, so the row shows it up front"
            );
        }
    }

    #[test]
    fn a_resolve_response_can_never_put_anything_new_on_a_row() {
        let inline = lsp_types::CompletionItem {
            label: "app".to_string(),
            kind: Some(lsp_types::CompletionItemKind::VARIABLE),
            sort_text: Some("11".to_string()),
            ..Default::default()
        };
        assert_eq!(
            completion_row_hint(&inline),
            None,
            "the server said nothing about this item up front, so its row says nothing"
        );
        // What the row would have become had the resolve been merged back into the server's list,
        // which is exactly what `AdeApp::completions_resolved_items` exists to prevent.
        let resolved = lsp_types::CompletionItem {
            detail: Some("const app: App<Element>".to_string()),
            ..inline.clone()
        };
        assert_eq!(
            completion_signature_text(&resolved),
            "const app: App<Element>",
            "the detail pane is where that type belongs, and it does reach it"
        );
    }

    #[test]
    fn a_resolved_auto_import_candidate_keeps_the_very_same_import_source_on_its_row() {
        let unresolved = lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some("fs".to_string()),
            ..Default::default()
        };
        let resolved = lsp_types::CompletionItem {
            detail: Some(
                "Auto import from 'fs'\nnamespace appendFile\nfunction appendFile(path: \
                 fs.PathOrFileDescriptor, data: string | Uint8Array, options: fs.WriteFileOptions, \
                 callback: fs.NoParamCallback): void (+1 overload)"
                    .to_string(),
            ),
            ..unresolved.clone()
        };
        assert_eq!(
            completion_import_source(&unresolved),
            completion_import_source(&resolved),
            "resolving must only ever *add* a type to the row, never move or rewrite the import \
             source already printed on it"
        );
        assert_eq!(completion_import_source(&resolved).as_deref(), Some("fs"));
        assert!(
            completion_item_display(&resolved)
                .1
                .is_some_and(|signature| signature.starts_with("namespace appendFile")),
            "and the type slot gains the signature the resolve finally supplied"
        );
    }

    #[test]
    fn three_real_rust_analyzer_import_candidates_each_name_the_use_they_would_add() {
        let candidate = |path: &str| lsp_types::CompletionItem {
            label: "Result".to_string(),
            kind: Some(lsp_types::CompletionItemKind::STRUCT),
            filter_text: Some("Result".to_string()),
            sort_text: Some("80000000".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some(format!("(use {path})")),
                description: None,
            }),
            ..Default::default()
        };
        for path in ["std::fmt::Result", "std::io::Result", "std::thread::Result"] {
            let item = candidate(path);
            assert_eq!(
                completion_import_source(&item).as_deref(),
                Some(path),
                "{path}: the `use` this candidate would add is the only thing distinguishing it \
                 from its siblings, so the row has to show it"
            );
            assert_eq!(
                completion_module_path(&item).as_deref(),
                Some(path),
                "{path}: and the detail pane's own module footer, empty until now, shows the same"
            );
            assert_eq!(
                completion_item_display(&item).1,
                None,
                "{path}: a `use` path is not this item's type"
            );
        }
    }

    #[test]
    fn a_real_combined_alias_and_use_note_still_yields_the_import_source() {
        let item = lsp_types::CompletionItem {
            label: "temp_dir".to_string(),
            kind: Some(lsp_types::CompletionItemKind::FUNCTION),
            detail: Some("fn() -> PathBuf".to_string()),
            filter_text: Some("temp_dirGetTempPathGetTempPath2".to_string()),
            label_details: Some(lsp_types::CompletionItemLabelDetails {
                detail: Some("(alias GetTempPath, GetTempPath2) (use std::env::temp_dir)".into()),
                description: Some("fn() -> PathBuf".to_string()),
            }),
            ..Default::default()
        };
        assert_eq!(
            completion_import_source(&item).as_deref(),
            Some("std::env::temp_dir"),
            "an item carrying both kinds of note is still an import candidate, and is exactly the \
             kind that most needs saying so"
        );
        assert_eq!(
            completion_item_display(&item).1.as_deref(),
            Some("fn() -> PathBuf"),
            "and its real signature still reaches the type slot"
        );
    }

    #[test]
    fn a_real_alias_or_trait_note_is_not_an_import_source() {
        for note in [
            "(alias ==, !=)",
            "(alias <, >, <=, >=)",
            "(alias ?, ?Sized)",
            "(alias list, vector)",
            "(as Into)",
        ] {
            let item = lsp_types::CompletionItem {
                label: "eq".to_string(),
                kind: Some(lsp_types::CompletionItemKind::METHOD),
                label_details: Some(lsp_types::CompletionItemLabelDetails {
                    detail: Some(note.to_string()),
                    description: None,
                }),
                ..Default::default()
            };
            assert_eq!(
                completion_import_source(&item),
                None,
                "{note}: only a genuine `(use path)` note names a module this item comes from"
            );
        }
    }

    #[test]
    fn clean_completion_detail_strips_a_real_typescript_method_kind_and_qualifier_prefix() {
        assert_eq!(
            clean_completion_detail("(method) QueryBuilder.pushStr(s: string): void", "pushStr"),
            "pushStr(s: string): void"
        );
    }

    #[test]
    fn clean_completion_detail_strips_a_real_typescript_property_kind_and_qualifier_prefix() {
        assert_eq!(
            clean_completion_detail("(property) Foo.bar: string", "bar"),
            "bar: string"
        );
    }

    #[test]
    fn clean_completion_detail_leaves_a_real_rust_analyzer_signature_untouched() {
        assert_eq!(
            clean_completion_detail("fn(&mut self, &str)", "push_str"),
            "fn(&mut self, &str)"
        );
        assert_eq!(
            clean_completion_detail("fn(self) -> T", "into"),
            "fn(self) -> T"
        );
    }

    #[test]
    fn clean_completion_detail_never_strips_a_real_parenthesized_type() {
        assert_eq!(
            clean_completion_detail("(x: number) => void", "onClick"),
            "(x: number) => void"
        );
        assert_eq!(
            clean_completion_detail("(number, string)", "pair"),
            "(number, string)"
        );
    }

    #[test]
    fn clean_completion_detail_only_strips_a_real_bare_dotted_qualifier() {
        // The label reappearing deep inside a return type, with no real `Qualifier.` immediately
        // before it, must not be mistaken for one.
        assert_eq!(
            clean_completion_detail("fn() -> Option<Table>", "Table"),
            "fn() -> Option<Table>"
        );
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
