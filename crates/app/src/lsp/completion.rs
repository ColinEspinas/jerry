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

/// A completion item's real display label plus an optional secondary detail string
/// (`CompletionItem::detail`, e.g. a real inferred type/signature) - what `crate::root::
/// completions`'s popover row actually renders, factored out here so it's testable without a
/// live `gpui` window.
pub fn completion_item_display(item: &lsp_types::CompletionItem) -> (String, Option<String>) {
    let detail = item
        .detail
        .as_ref()
        .map(|detail| detail.trim().to_string())
        .filter(|detail| !detail.is_empty());
    (item.label.clone(), detail)
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
}
