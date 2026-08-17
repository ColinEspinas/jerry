//! Pure logic for Surface C's File view (`design_handoff_jerry_ade/README.md`'s "File view"
//! subsection): reads a file off disk, detects its line-ending style, picks a language label
//! from its extension, and - for a real subset of extensions - produces syntax-colored spans by
//! parsing with `tree-sitter` and walking the resulting AST. Deliberately `gpui`-window-free
//! (only [`gpui::Rgba`] is used, for plain colour data), mirroring this crate's split between
//! pure logic modules and `crate::root`'s `Div` construction.

use std::sync::OnceLock;

use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::{Rgba, SharedString};

use crate::sidebar::changes;
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

/// The status bar's language label, derived from `path`'s extension - reads
/// `crate::language::display_name_for_extension`, the one canonical registry every
/// extension-keyed lookup in this crate now shares (case-insensitive; `"Plain Text"` for
/// anything not in it).
pub fn language_name_for_extension(extension: Option<&str>) -> &'static str {
    crate::language::display_name_for_extension(extension)
}

/// The real highlighter function `extension` should be parsed with, read straight off
/// `crate::language`'s canonical registry - the single real source of truth
/// `crate::language::ExtensionEntry::highlighter`'s own docs describe (Revision R8's
/// consolidation of what used to be a second, independent extension -> highlighter `match`
/// statement `load_file` maintained on its own, invisible to that registry). `None` for an
/// extension absent from the registry, or present with no real grammar wired
/// ([`crate::language::ExtensionEntry::highlighter`] itself `None` - only SQL and Vue, today).
pub fn highlighter_for_extension(
    extension: Option<&str>,
) -> Option<crate::language::HighlighterFn> {
    crate::language::entry_for_extension(extension)?.highlighter
}

/// A syntax span's classification - GitHub issue #31's extended scope-coverage checklist (22 real
/// `tree-sitter-highlight` buckets, up from the original six-bucket
/// `design_handoff_jerry_ade/README.md` File view table), plus the [`Text`](HighlightKind::Text)
/// fallback every byte a query doesn't classify at all still receives. See [`HIGHLIGHT_NAMES`] for
/// the real, verified capture names each variant is reached through, and
/// `theme::syntax`'s own module docs for the palette design (which variants are real,
/// independently-authored colours versus ones whose default is simply their parent scope's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    Function,
    FunctionMethod,
    /// A function/method *definition site* - see `RUST_DEFINITION_SUPPLEMENT`.
    FunctionDefinition,
    Type,
    TypeBuiltin,
    Constant,
    ConstantBuiltin,
    String,
    StringEscape,
    Number,
    Comment,
    CommentDoc,
    /// A JSDoc-style `@param`/`@returns`/`@example` block tag, or a `{@link ...}` inline tag,
    /// found inside an already-[`CommentDoc`](Self::CommentDoc) span - GitHub issue #200. Not a
    /// real capture from any bundled grammar's own query (none of this app's grammars parses
    /// *inside* a comment body), so it is never registered in [`HIGHLIGHT_NAMES`] - it only ever
    /// comes from [`doc_tag_ranges`] splitting an existing `Comment`/`CommentDoc` span after the
    /// real tree-sitter pass, the same "post-process on top of the grammar's own output" idiom
    /// [`colorize_bracket_pairs`] already established for the bracket-pair ring.
    CommentDocTag,
    Variable,
    VariableParameter,
    VariableBuiltin,
    Property,
    Operator,
    /// A `(`/`)`/`[`/`]`/`{`/`}` the grammar's own `punctuation.bracket` capture matched - **and
    /// which [`colorize_bracket_pairs`] could not pair up with a real partner**, plus every
    /// `<`/`>` that capture matches (a generic argument list's, an HTML tag's), which that pass
    /// deliberately never tracks. A bracket that *is* half of a real matched pair is reclassified
    /// into one of [`HighlightKind::BRACKET_DEPTH_RING`]'s six buckets instead. See
    /// [`colorize_bracket_pairs`] for both decisions.
    PunctuationBracket,
    PunctuationDelimiter,
    /// GitHub issue #168: a real matched bracket pair at nesting depth 0, 6, 12, ... - see
    /// [`HighlightKind::BRACKET_DEPTH_RING`] and `theme::syntax::BRACKET_1`. Unlike every other
    /// variant here this is never produced by a `tree-sitter-highlight` capture (it is not in
    /// [`HIGHLIGHT_NAMES`] and never can be - no grammar knows a bracket's nesting depth); it is
    /// assigned by [`colorize_bracket_pairs`] as a post-process over an already-classified span
    /// list. It is a real [`HighlightKind`] all the same, so it is independently themeable and
    /// reaches every renderer that already goes through [`color_for_kind`] - the File view, the
    /// minimap, the Diff and Merge views, the Markdown preview's fenced code blocks - with no
    /// per-renderer wiring at all.
    Bracket1,
    /// Bracket-pair depth ring, colour 2 of 6 (nesting depth 1, 7, ...) - see [`Bracket1`](Self::Bracket1).
    Bracket2,
    /// Bracket-pair depth ring, colour 3 of 6 (nesting depth 2, 8, ...) - see [`Bracket1`](Self::Bracket1).
    Bracket3,
    /// Bracket-pair depth ring, colour 4 of 6 (nesting depth 3, 9, ...) - see [`Bracket1`](Self::Bracket1).
    Bracket4,
    /// Bracket-pair depth ring, colour 5 of 6 (nesting depth 4, 10, ...) - see [`Bracket1`](Self::Bracket1).
    Bracket5,
    /// Bracket-pair depth ring, colour 6 of 6 (nesting depth 5, 11, ...) - see [`Bracket1`](Self::Bracket1).
    Bracket6,
    Tag,
    Attribute,
    Embedded,
    Text,
    /// GitHub issue #104: Markdown's own `text.title` capture (a heading's text) - see
    /// `theme::syntax::HEADING`'s own docs for why this is a real, dedicated variant rather than
    /// a reused code bucket.
    Heading,
    /// GitHub issue #104: Markdown's `text.uri`/`text.reference` captures (a link's destination
    /// and its visible label/text) - see `theme::syntax::LINK`'s own docs.
    Link,
    /// GitHub issue #104: Markdown's `text.strong` capture (`**bold**`) - see
    /// `theme::syntax::STRONG`'s own docs on this app's real font-weight rendering limitation.
    Strong,
    /// GitHub issue #104: Markdown's `text.emphasis` capture (`*italic*`) - see
    /// `theme::syntax::EMPHASIS`'s own docs.
    Emphasis,
    /// GitHub issue #183: the grammar's own `@punctuation.special` capture - Markdown's ATX `#`
    /// heading marker and list bullets, JS/TS's `${`/`}` template-interpolation delimiters,
    /// YAML's `---`/`&`/`*`/`...` sigils. Used to fall through to [`Operator`](Self::Operator),
    /// which reads a heading marker or a document separator as an arithmetic/comparison operator,
    /// a real, distinct grammar-level concept, now its own bucket. `theme::syntax::
    /// PUNCTUATION_SPECIAL` keeps `Operator`'s own colour (the restraint palette has no reason to
    /// tell them apart *visually* yet), but the classification itself is real and independently
    /// themeable now.
    PunctuationSpecial,
    /// GitHub issue #183: the grammar's own `@label` capture - Rust lifetimes (`'a`), C `goto`
    /// targets, YAML anchor/alias names. Used to fall through to [`Variable`](Self::Variable),
    /// which is a different real concept again. Note this one real bucket still covers all three:
    /// `tree-sitter-highlight` resolves purely on the capture-name string, which is identical
    /// (`"label"`) across all three grammars, so nothing downstream of the parse can tell a Rust
    /// lifetime from a YAML anchor by capture name alone; splitting those three apart from *each
    /// other* would need a real, new per-language supplement query (the same pattern
    /// [`RUST_DEFINITION_SUPPLEMENT`]/[`GO_HIGHLIGHTS_SUPPLEMENT`] already use) re-capturing each
    /// language's own node under a more specific dotted name, deliberately out of scope here.
    /// `theme::syntax::LABEL` keeps `Variable`'s own colour for the same restraint-palette reason
    /// [`PunctuationSpecial`](Self::PunctuationSpecial) does.
    Label,
    /// GitHub issue #183: the grammar's own `@string.special` capture - a JS/TS regex literal, a
    /// TOML datetime, a CSS colour value (`#fff`, `rgb(...)`). Not registered in
    /// [`HIGHLIGHT_NAMES`] before this issue, so it fell through `tree-sitter-highlight`'s own
    /// subset match to the coarser, already-registered plain `"string"` - a regex literal reading
    /// as an ordinary string. `theme::syntax::STRING_SPECIAL` keeps `String`'s own colour.
    StringSpecial,
    /// GitHub issue #183: the grammar's own `@function.builtin` capture - Python's `len`/`print`,
    /// Go's `append`/`make`/`panic`, JavaScript's `require`. Not registered before this issue, so
    /// it fell through to the coarser plain `"function"`. `theme::syntax::FUNCTION_BUILTIN` keeps
    /// `Function`'s own colour.
    FunctionBuiltin,
    /// GitHub issue #183: the grammar's own `@function.macro` capture - Rust's `println!`-style
    /// macro invocations (both the macro name and its own `!`). Not registered before this issue
    /// (this app's own docs used to name it as a deliberate, known gap - see this issue's own
    /// discussion), so it fell through to the coarser plain `"function"`, reading as an ordinary
    /// call. `theme::syntax::FUNCTION_MACRO` keeps `Function`'s own colour.
    FunctionMacro,
    /// GitHub issue #183: the grammar's own `@tag.error` capture - HTML's mismatched/erroneous
    /// closing tag. Not registered before this issue, so it fell through to the coarser plain
    /// `"tag"`, reading as an ordinary (correct) tag. `theme::syntax::TAG_ERROR` keeps `Tag`'s own
    /// colour.
    TagError,
    /// GitHub issue #183: the grammar's own `@constructor` capture - Rust/Python/JavaScript's
    /// shared `^[A-Z]`-starts-with-a-capital heuristic for "this identifier names a type or one
    /// of its variants" (`tree-sitter-rust`'s own comment: "enum constructors ... either that, or
    /// struct names"). Was folded into [`Type`](Self::Type) - the same real capture rule, but a
    /// distinct grammar-level concept (an enum variant construction site vs. a type name used
    /// elsewhere) that a theme wanting to tell them apart now has a real, independent bucket for.
    /// `theme::syntax::CONSTRUCTOR` keeps `Type`'s own colour, its exact pre-issue behavior.
    Constructor,
}

impl HighlightKind {
    /// GitHub issue #141: the real, stable snake_case name a hand-authored or VSCode-converted
    /// custom theme file's own `[syntax]` table key names this bucket by - the counterpart to
    /// [`Self::from_name`]. Deliberately distinct from `Debug`'s own `CamelCase` output (a
    /// derived `Debug` impl is not a real, stable public contract - a future derive-macro change
    /// or field addition could silently reshape it) and from the tree-sitter capture-name
    /// vocabulary [`HIGHLIGHT_NAMES`] uses (that list has dotted, grammar-facing names like
    /// `"function.method"`; a TOML table key is a plain identifier, no dots).
    pub fn name(self) -> &'static str {
        match self {
            HighlightKind::Keyword => "keyword",
            HighlightKind::Function => "function",
            HighlightKind::FunctionMethod => "function_method",
            HighlightKind::FunctionDefinition => "function_definition",
            HighlightKind::Type => "type",
            HighlightKind::TypeBuiltin => "type_builtin",
            HighlightKind::Constant => "constant",
            HighlightKind::ConstantBuiltin => "constant_builtin",
            HighlightKind::String => "string",
            HighlightKind::StringEscape => "string_escape",
            HighlightKind::Number => "number",
            HighlightKind::Comment => "comment",
            HighlightKind::CommentDoc => "comment_doc",
            HighlightKind::CommentDocTag => "comment_doc_tag",
            HighlightKind::Variable => "variable",
            HighlightKind::VariableParameter => "variable_parameter",
            HighlightKind::VariableBuiltin => "variable_builtin",
            HighlightKind::Property => "property",
            HighlightKind::Operator => "operator",
            HighlightKind::PunctuationBracket => "punctuation_bracket",
            HighlightKind::PunctuationDelimiter => "punctuation_delimiter",
            HighlightKind::Bracket1 => "bracket_1",
            HighlightKind::Bracket2 => "bracket_2",
            HighlightKind::Bracket3 => "bracket_3",
            HighlightKind::Bracket4 => "bracket_4",
            HighlightKind::Bracket5 => "bracket_5",
            HighlightKind::Bracket6 => "bracket_6",
            HighlightKind::Tag => "tag",
            HighlightKind::Attribute => "attribute",
            HighlightKind::Embedded => "embedded",
            HighlightKind::Text => "text",
            HighlightKind::Heading => "heading",
            HighlightKind::Link => "link",
            HighlightKind::Strong => "strong",
            HighlightKind::Emphasis => "emphasis",
            HighlightKind::PunctuationSpecial => "punctuation_special",
            HighlightKind::Label => "label",
            HighlightKind::StringSpecial => "string_special",
            HighlightKind::FunctionBuiltin => "function_builtin",
            HighlightKind::FunctionMacro => "function_macro",
            HighlightKind::TagError => "tag_error",
            HighlightKind::Constructor => "constructor",
        }
    }

    /// The inverse of [`Self::name`] - `None` for anything that isn't a real, exact name (a
    /// custom theme file's own validation reports this as a real, specific
    /// `ThemeFileError::UnknownSyntaxKey` rather than silently ignoring a typo).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Every real variant, in the same order [`Self::name`]'s own `match` lists them - the real
    /// source [`Self::from_name`] searches and
    /// `crate::settings::custom_theme::tests::every_highlight_kind_name_round_trips_through_from_name`
    /// checks exhaustively against.
    pub const ALL: [HighlightKind; 42] = [
        HighlightKind::Keyword,
        HighlightKind::Function,
        HighlightKind::FunctionMethod,
        HighlightKind::FunctionDefinition,
        HighlightKind::Type,
        HighlightKind::TypeBuiltin,
        HighlightKind::Constant,
        HighlightKind::ConstantBuiltin,
        HighlightKind::String,
        HighlightKind::StringEscape,
        HighlightKind::Number,
        HighlightKind::Comment,
        HighlightKind::CommentDoc,
        HighlightKind::CommentDocTag,
        HighlightKind::Variable,
        HighlightKind::VariableParameter,
        HighlightKind::VariableBuiltin,
        HighlightKind::Property,
        HighlightKind::Operator,
        HighlightKind::PunctuationBracket,
        HighlightKind::PunctuationDelimiter,
        HighlightKind::Bracket1,
        HighlightKind::Bracket2,
        HighlightKind::Bracket3,
        HighlightKind::Bracket4,
        HighlightKind::Bracket5,
        HighlightKind::Bracket6,
        HighlightKind::Tag,
        HighlightKind::Attribute,
        HighlightKind::Embedded,
        HighlightKind::Text,
        HighlightKind::Heading,
        HighlightKind::Link,
        HighlightKind::Strong,
        HighlightKind::Emphasis,
        HighlightKind::PunctuationSpecial,
        HighlightKind::Label,
        HighlightKind::StringSpecial,
        HighlightKind::FunctionBuiltin,
        HighlightKind::FunctionMacro,
        HighlightKind::TagError,
        HighlightKind::Constructor,
    ];

    /// GitHub issue #168's rotating bracket-pair depth ring, in ring order: a real matched pair at
    /// nesting depth `d` paints `BRACKET_DEPTH_RING[d % BRACKET_DEPTH_RING.len()]`, both halves
    /// alike. Six is the ring length VSCode and most editors shipping this feature settled on -
    /// short enough that two depths a reader is actually comparing (`d` and `d + 1`, which nest
    /// directly inside one another) are always maximally far apart in the palette, long enough
    /// that a wrap-around collision needs six levels of nesting to happen at all. See
    /// `theme::syntax`'s own "bracket-pair depth ring" docs for how the six colours were measured.
    pub const BRACKET_DEPTH_RING: [HighlightKind; 6] = [
        HighlightKind::Bracket1,
        HighlightKind::Bracket2,
        HighlightKind::Bracket3,
        HighlightKind::Bracket4,
        HighlightKind::Bracket5,
        HighlightKind::Bracket6,
    ];

    /// The ring colour a real matched pair at nesting `depth` paints - the one place the `% 6`
    /// wrap-around lives, so no caller open-codes it.
    pub fn for_bracket_depth(depth: usize) -> Self {
        Self::BRACKET_DEPTH_RING[depth % Self::BRACKET_DEPTH_RING.len()]
    }
}

/// Maps a [`HighlightKind`] to its real `theme::syntax::*` colour - see that module's own docs
/// for the fallback-chain design behind each mapping.
pub fn color_for_kind(kind: HighlightKind) -> Rgba {
    // Each arm is an ordinary `theme::syntax::*` token, so a theme file that names e.g.
    // `[syntax] keyword = "#ff79c6"` (very much including an imported VSCode theme, whose own
    // `tokenColors` array is converted straight into those keys - see
    // `crate::settings::vscode_theme`) changes what this returns through the exact same
    // `ColorToken::resolve` path every other colour in the app goes through. Before the theme
    // system's rewrite this function needed its own separate per-scope override map checked ahead
    // of the tokens, because several of these buckets were literal Rust-level aliases of one
    // another and could not be told apart; every one of them is now independently keyed, so that
    // second mechanism is gone.
    match kind {
        HighlightKind::Keyword => theme::syntax::KEYWORD.into(),
        HighlightKind::Function => theme::syntax::FUNCTION.into(),
        HighlightKind::FunctionMethod => theme::syntax::FUNCTION_METHOD.into(),
        HighlightKind::FunctionDefinition => theme::syntax::FUNCTION_DEFINITION.into(),
        HighlightKind::Type => theme::syntax::TYPE.into(),
        HighlightKind::TypeBuiltin => theme::syntax::TYPE_BUILTIN.into(),
        HighlightKind::Constant => theme::syntax::CONSTANT.into(),
        HighlightKind::ConstantBuiltin => theme::syntax::CONSTANT_BUILTIN.into(),
        HighlightKind::String => theme::syntax::STRING.into(),
        HighlightKind::StringEscape => theme::syntax::STRING_ESCAPE.into(),
        HighlightKind::Number => theme::syntax::NUMBER.into(),
        HighlightKind::Comment => theme::syntax::COMMENT.into(),
        HighlightKind::CommentDoc => theme::syntax::COMMENT_DOC.into(),
        HighlightKind::CommentDocTag => theme::syntax::COMMENT_DOC_TAG.into(),
        HighlightKind::Variable => theme::syntax::VARIABLE.into(),
        HighlightKind::VariableParameter => theme::syntax::VARIABLE_PARAMETER.into(),
        HighlightKind::VariableBuiltin => theme::syntax::VARIABLE_BUILTIN.into(),
        HighlightKind::Property => theme::syntax::PROPERTY.into(),
        HighlightKind::Operator => theme::syntax::OPERATOR.into(),
        HighlightKind::PunctuationBracket => theme::syntax::PUNCTUATION_BRACKET.into(),
        HighlightKind::PunctuationDelimiter => theme::syntax::PUNCTUATION_DELIMITER.into(),
        HighlightKind::Bracket1 => theme::syntax::BRACKET_1.into(),
        HighlightKind::Bracket2 => theme::syntax::BRACKET_2.into(),
        HighlightKind::Bracket3 => theme::syntax::BRACKET_3.into(),
        HighlightKind::Bracket4 => theme::syntax::BRACKET_4.into(),
        HighlightKind::Bracket5 => theme::syntax::BRACKET_5.into(),
        HighlightKind::Bracket6 => theme::syntax::BRACKET_6.into(),
        HighlightKind::Tag => theme::syntax::TAG.into(),
        HighlightKind::Attribute => theme::syntax::ATTRIBUTE.into(),
        HighlightKind::Embedded => theme::syntax::EMBEDDED.into(),
        HighlightKind::Text => theme::syntax::TEXT.into(),
        HighlightKind::Heading => theme::syntax::HEADING.into(),
        HighlightKind::Link => theme::syntax::LINK.into(),
        HighlightKind::Strong => theme::syntax::STRONG.into(),
        HighlightKind::Emphasis => theme::syntax::EMPHASIS.into(),
        HighlightKind::PunctuationSpecial => theme::syntax::PUNCTUATION_SPECIAL.into(),
        HighlightKind::Label => theme::syntax::LABEL.into(),
        HighlightKind::StringSpecial => theme::syntax::STRING_SPECIAL.into(),
        HighlightKind::FunctionBuiltin => theme::syntax::FUNCTION_BUILTIN.into(),
        HighlightKind::FunctionMacro => theme::syntax::FUNCTION_MACRO.into(),
        HighlightKind::TagError => theme::syntax::TAG_ERROR.into(),
        HighlightKind::Constructor => theme::syntax::CONSTRUCTOR.into(),
    }
}

/// One classified leaf token from a `tree-sitter` parse - byte offsets into the whole-file
/// source [`highlight_rust`] parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
    /// Which real *injected region* of the file this span's bytes came from - see
    /// [`injection_scopes`]. [`OUTER_SCOPE`] for the host language's own top-level text (which is
    /// every span in a file whose grammar injects nothing at all, i.e. all but Markdown and HTML);
    /// `1..` identifies one specific injected range, in source order.
    pub scope: u32,
}

/// [`HighlightSpan::scope`] for the host language's own text, outside every injected region.
pub const OUTER_SCOPE: u32 = 0;
/// Which real grammar a piece of source is parsed and queried with. `TypeScript` and `Tsx` are
/// two genuinely different grammars in `tree-sitter-typescript` (not one grammar with a flag), and
/// they need two different composed query strings - see [`highlight_query_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::code_surface) enum Grammar {
    Rust,
    TypeScript,
    Tsx,
    Python,
    Toml,
    Go,
    Json,
    Yaml,
    C,
    /// GitHub issue #104: Markdown's own *block* grammar (headings, lists, fenced code blocks,
    /// ...) - the real top-level entry point [`highlight_markdown`] parses `.md` source with.
    /// Never assigned to a file extension on its own; [`Grammar::MarkdownInline`] only exists as
    /// this one's real injection target (see [`build_highlight_config`]'s own docs on why
    /// Markdown needs two grammars, unlike every other entry here).
    Markdown,
    /// GitHub issue #104: Markdown's *inline* grammar (emphasis, links, code spans, ...) - real
    /// prose content lives inside `(inline)` nodes the block grammar itself never parses further,
    /// per `tree-sitter-md`'s own `injections.scm` (`(inline) @injection.content (#set!
    /// injection.language "markdown_inline")`, read directly from the crate). Reached only
    /// through [`Grammar::Markdown`]'s own injection callback - never a file's own top-level
    /// grammar.
    MarkdownInline,
    /// GitHub issue #154: `tree-sitter-html`, the real top-level grammar for `.html`/`.htm`. Also
    /// reached as an *injection* target - from Markdown's own `(html_block)`/`(html_tag)` patterns
    /// (see [`MARKDOWN_INJECTION_QUERY`] / [`MARKDOWN_INLINE_INJECTION_QUERY`]) - which is the
    /// "including in the markdown files" half of that issue.
    Html,
    /// GitHub issue #154: `tree-sitter-css`, the real top-level grammar for `.css`. Also reached as
    /// an injection target from [`Grammar::Html`]'s own bundled `injections.scm`, which routes a
    /// `<style>` element's `(raw_text)` here.
    Css,
}

impl Grammar {
    /// Every real grammar, in [`Grammar::index`] order - the single list both
    /// [`HIGHLIGHT_CONFIGS`]' slot count and this module's own coverage tests are derived from, so
    /// adding a grammar cannot leave either behind.
    const ALL: [Grammar; 13] = [
        Grammar::Rust,
        Grammar::TypeScript,
        Grammar::Tsx,
        Grammar::Python,
        Grammar::Toml,
        Grammar::Go,
        Grammar::Json,
        Grammar::Yaml,
        Grammar::C,
        Grammar::Markdown,
        Grammar::MarkdownInline,
        Grammar::Html,
        Grammar::Css,
    ];

    const COUNT: usize = Self::ALL.len();

    /// This grammar's slot in [`HIGHLIGHT_CONFIGS`]. Kept in step with [`Grammar::ALL`] by
    /// `grammar_indices_match_their_position_in_all`, which is what makes indexing that array
    /// without a bounds concern honest rather than assumed.
    const fn index(self) -> usize {
        match self {
            Grammar::Rust => 0,
            Grammar::TypeScript => 1,
            Grammar::Tsx => 2,
            Grammar::Python => 3,
            Grammar::Toml => 4,
            Grammar::Go => 5,
            Grammar::Json => 6,
            Grammar::Yaml => 7,
            Grammar::C => 8,
            Grammar::Markdown => 9,
            Grammar::MarkdownInline => 10,
            Grammar::Html => 11,
            Grammar::Css => 12,
        }
    }

    pub(in crate::code_surface) fn language(self) -> tree_sitter::Language {
        match self {
            Grammar::Rust => tree_sitter_rust::LANGUAGE.into(),
            Grammar::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Grammar::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Grammar::Python => tree_sitter_python::LANGUAGE.into(),
            Grammar::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Grammar::Go => tree_sitter_go::LANGUAGE.into(),
            Grammar::Json => tree_sitter_json::LANGUAGE.into(),
            Grammar::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Grammar::C => tree_sitter_c::LANGUAGE.into(),
            Grammar::Markdown => tree_sitter_md::LANGUAGE.into(),
            Grammar::MarkdownInline => tree_sitter_md::INLINE_LANGUAGE.into(),
            Grammar::Html => tree_sitter_html::LANGUAGE.into(),
            Grammar::Css => tree_sitter_css::LANGUAGE.into(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Grammar::Rust => "rust",
            Grammar::TypeScript => "typescript",
            Grammar::Tsx => "tsx",
            Grammar::Python => "python",
            Grammar::Toml => "toml",
            Grammar::Go => "go",
            Grammar::Json => "json",
            Grammar::Yaml => "yaml",
            Grammar::C => "c",
            Grammar::Markdown => "markdown",
            Grammar::MarkdownInline => "markdown_inline",
            Grammar::Html => "html",
            Grammar::Css => "css",
        }
    }

    /// The grammar an `@injection.language` capture or a `#set! injection.language` predicate
    /// naming `name` should resolve to - the single real resolver behind
    /// [`injection_config`], which every [`Highlighter::highlight`] call in this module
    /// passes as its injection callback.
    fn for_injection_name(name: &str) -> Option<Grammar> {
        if let Some(grammar) = Grammar::ALL
            .into_iter()
            .find(|grammar| grammar.name() == name)
        {
            return Some(grammar);
        }
        Grammar::for_extension(crate::language::extension_for_fence_language(name)?)
    }

    /// The real grammar behind a [`crate::language::EXTENSIONS`] extension key. Kept honest
    /// against that registry by `every_registry_extension_with_a_highlighter_has_an_injectable_
    /// grammar`, so an extension that gains a highlighter but not an entry here (and would then
    /// silently stop working as a markdown fence language) fails a test rather than degrading
    /// quietly.
    pub(in crate::code_surface) fn for_extension(extension: &str) -> Option<Grammar> {
        Some(match extension {
            "rs" => Grammar::Rust,
            // `.js` reuses the plain TypeScript grammar, `.jsx` the TSX one - see
            // [`highlight_typescript`]'s own docs, and `crate::language::EXTENSIONS`' `js`/`jsx`
            // entries, which wire exactly this pairing.
            "ts" | "js" => Grammar::TypeScript,
            "tsx" | "jsx" => Grammar::Tsx,
            "py" => Grammar::Python,
            "toml" => Grammar::Toml,
            "go" => Grammar::Go,
            "json" => Grammar::Json,
            "yaml" | "yml" => Grammar::Yaml,
            "c" | "h" => Grammar::C,
            "md" => Grammar::Markdown,
            "html" | "htm" => Grammar::Html,
            "css" => Grammar::Css,
            _ => return None,
        })
    }
}

/// The ordered "recognized highlight names" list every [`HighlightConfiguration`] below is
/// [`HighlightConfiguration::configure`]d with. `tree-sitter-highlight` resolves each of a query's
/// own capture names against this list and hands back a [`tree_sitter_highlight::Highlight`]
/// holding **an index into this exact slice**, so [`HIGHLIGHT_KINDS`] below is a positional
/// parallel array: entry `i` here is classified as entry `i` there. The two are length-checked
/// against each other at compile time (see [`HIGHLIGHT_KINDS`]) so they can never silently drift.
const HIGHLIGHT_NAMES: &[&str] = &[
    "keyword",
    "function",
    "function.method",
    "function.definition",
    "type",
    "type.builtin",
    "constructor",
    "tag",
    "constant",
    "constant.builtin",
    "string",
    "escape",
    "string.escape",
    "number",
    "comment",
    "comment.documentation",
    "variable",
    "variable.parameter",
    "variable.builtin",
    "property",
    "operator",
    "punctuation.bracket",
    "punctuation.delimiter",
    "attribute",
    "embedded",
    "boolean",
    "delimiter",
    "label",
    "punctuation.special",
    "string.special.key",
    // GitHub issue #104 (Markdown): see `HighlightKind::Heading`/`Link`/`Strong`/`Emphasis`'s own
    // docs for why these have no reasonable existing-bucket analog.
    "text.title",
    "text.literal",
    "text.uri",
    "text.reference",
    "text.strong",
    "text.emphasis",
    // GitHub issue #183 - see this constant's own docs above for what each was falling through
    // to before, and why each is a real, distinct grammar-level concept rather than an internal
    // implementation detail.
    "string.special",
    "function.builtin",
    "function.macro",
    "tag.error",
    // `"none"` is deliberately **not** registered, and that absence is load-bearing - see
    // [`MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT`], which is what makes real per-fence language
    // injection produce correct spans. GitHub issue #104 originally registered it (mapped to
    // `Text`) to cancel the enclosing `(fenced_code_block) @text.literal`; issue #154 achieves
    // the same visible result by cancelling `@text.literal` at its source instead, which - unlike
    // a registered `"none"` - leaves no parent highlight open across an injected range.
];

/// [`HIGHLIGHT_NAMES`]' positional parallel array: which real [`HighlightKind`] each recognized
/// highlight name renders as. See [`HIGHLIGHT_NAMES`] for the indexing contract.
const HIGHLIGHT_KINDS: [HighlightKind; HIGHLIGHT_NAMES.len()] = [
    HighlightKind::Keyword,
    HighlightKind::Function,
    HighlightKind::FunctionMethod,
    HighlightKind::FunctionDefinition,
    HighlightKind::Type,
    HighlightKind::TypeBuiltin,
    HighlightKind::Constructor,
    HighlightKind::Tag,
    HighlightKind::Constant,
    HighlightKind::ConstantBuiltin,
    HighlightKind::String,
    HighlightKind::StringEscape,
    HighlightKind::StringEscape,
    HighlightKind::Number,
    HighlightKind::Comment,
    HighlightKind::CommentDoc,
    HighlightKind::Variable,
    HighlightKind::VariableParameter,
    HighlightKind::VariableBuiltin,
    HighlightKind::Property,
    HighlightKind::Operator,
    HighlightKind::PunctuationBracket,
    HighlightKind::PunctuationDelimiter,
    HighlightKind::Attribute,
    HighlightKind::Embedded,
    HighlightKind::ConstantBuiltin,
    HighlightKind::PunctuationDelimiter,
    HighlightKind::Label,
    HighlightKind::PunctuationSpecial,
    HighlightKind::Property,
    HighlightKind::Heading,
    HighlightKind::String,
    HighlightKind::Link,
    HighlightKind::Link,
    HighlightKind::Strong,
    HighlightKind::Emphasis,
    HighlightKind::StringSpecial,
    HighlightKind::FunctionBuiltin,
    HighlightKind::FunctionMacro,
    HighlightKind::TagError,
];

/// Real supplement appended after `tree-sitter-python`'s own bundled `queries/highlights.scm`,
/// covering two genuine gaps that would otherwise be visible *regressions* against the hand-rolled
/// implementation this module replaced. Appending (rather than prepending) is what makes these
/// win: for two patterns capturing the same node, `tree-sitter-highlight` keeps iterating and
/// takes the **last** one ("keep iterating over any later highlighting patterns that also match
/// this node and set the match to it", `tree-sitter-highlight-0.26.9/src/highlight.rs:1043-1066`,
/// read directly - not assumed).
const PYTHON_HIGHLIGHTS_SUPPLEMENT: &str = r#"
[
  "and"
  "or"
  "not"
  "in"
  "is"
  "not in"
  "is not"
] @keyword

(type) @type

(type (generic_type (identifier) @type))
(type (attribute object: (identifier) @type))

(class_definition name: (identifier) @type)

((identifier) @variable.builtin
 (#match? @variable.builtin "^(self|cls)$"))

(call function: (identifier) @function)
(call function: (attribute attribute: (identifier) @function.method))
"#;

/// Real supplement appended after `tree-sitter-go`'s own bundled query (see
/// [`highlight_query_for`]) - GitHub issue #32's own "last-pattern-wins" gotcha, same root cause
/// as [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]'s rule 5. `-go`'s own query declares its real
/// `(function_declaration name: (identifier) @function)`/call-expression rules *before* its later,
/// blanket `(identifier) @variable` (its own "Identifiers" section) - so without this, every real
/// function name and call in a `.go` file loses to the blanket rule and renders as `Variable`
/// (harmless in that it's a direct `TEXT` alias, but a real, avoidable loss of the same
/// `Function` colouring Rust/TypeScript/Python/C all get for the identical construct). Restating
/// the three real function rules last, verbatim from `-go`'s own query, puts that back.
const GO_HIGHLIGHTS_SUPPLEMENT: &str = r#"
(call_expression
  function: (identifier) @function)

(call_expression
  function: (identifier) @function.builtin
  (#match? @function.builtin "^(append|cap|close|complex|copy|delete|imag|len|make|new|panic|print|println|real|recover)$"))

(call_expression
  function: (selector_expression
    field: (field_identifier) @function.method))

(function_declaration
  name: (identifier) @function)
"#;

/// Real supplement appended after `tree-sitter-json`'s own bundled query (see
/// [`highlight_query_for`]) - the same "last-pattern-wins" gotcha as [`GO_HIGHLIGHTS_SUPPLEMENT`],
/// found the same way (a real, run test, not assumed from reading the query alone). `-json`'s own
/// query declares its real `(pair key: (_) @string.special.key)` *before* its later, blanket
/// `(string) @string` - and a JSON object key genuinely is a `(string)` node too, so without this
/// every real key loses to the blanket rule and renders as a plain string value (`String`)
/// instead of the more accurate `Property` this module's own docs above describe. Restating the
/// key rule last, verbatim from `-json`'s own query, puts that back.
const JSON_HIGHLIGHTS_SUPPLEMENT: &str = r#"
(pair
  key: (_) @string.special.key)
"#;

/// GitHub issue #168's real prerequisite for four of this module's grammars: a
/// `punctuation.bracket` capture they simply do not ship one of.
const PYTHON_BRACKET_SUPPLEMENT: &str = r#"
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
"#;
/// See [`PYTHON_BRACKET_SUPPLEMENT`].
const GO_BRACKET_SUPPLEMENT: &str = r#"
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
"#;
/// See [`PYTHON_BRACKET_SUPPLEMENT`] - JSON has no parenthesis token, only the two bracket shapes
/// its own grammar defines.
const JSON_BRACKET_SUPPLEMENT: &str = r#"
["[" "]" "{" "}"] @punctuation.bracket
"#;
/// See [`PYTHON_BRACKET_SUPPLEMENT`].
const C_BRACKET_SUPPLEMENT: &str = r#"
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
"#;

// ---------------------------------------------------------------------------------------------
// Definition-site supplements
// ---------------------------------------------------------------------------------------------
//
/// Real supplements that split a *function definition* out from a *function call*.
const RUST_VARIABLE_PREFIX: &str = r#"
(identifier) @variable
"#;

/// Real supplement **appended** after `tree-sitter-rust`'s own query, repairing the one genuine
/// regression [`RUST_VARIABLE_PREFIX`] introduced.
const RUST_ATTRIBUTE_SUPPLEMENT: &str = r#"
(attribute (identifier) @attribute)
(attribute (scoped_identifier) @attribute)
(attribute (token_tree (identifier) @attribute))
(attribute (token_tree (token_tree (identifier) @attribute)))
(attribute (token_tree (token_tree (token_tree (identifier) @attribute))))
"#;

/// Real supplement appended after `tree-sitter-rust`'s own bundled query, repairing a genuine
/// **upstream typo** that makes `theme::syntax::CONSTANT` unreachable in Rust.
const RUST_CONSTANT_SUPPLEMENT: &str = r#"
((identifier) @constant (#match? @constant "^[A-Z][A-Z\d_]+$"))
"#;

const RUST_DEFINITION_SUPPLEMENT: &str = r#"
(function_item name: (identifier) @function.definition)
(function_signature_item name: (identifier) @function.definition)
"#;

/// See [`RUST_DEFINITION_SUPPLEMENT`]. `function_definition`'s `name` field is an `identifier`
/// only (no metavariable case, unlike Rust). A `lambda` has no name node at all, so it is
/// correctly untouched.
/// Real supplement giving Python the `@variable.parameter` capture its bundled query has **no
/// pattern for at all** - `tree-sitter-python-0.25.0/queries/highlights.scm` contains zero
/// `@variable.parameter`, so every parameter fell through to its line-3 blanket
/// `(identifier) @variable` and `theme::syntax::VARIABLE_PARAMETER` was unreachable in Python.
const PYTHON_PARAMETER_SUPPLEMENT: &str = r#"
(parameters (identifier) @variable.parameter)
(parameters (typed_parameter (identifier) @variable.parameter))
(parameters (default_parameter name: (identifier) @variable.parameter))
(parameters (typed_default_parameter name: (identifier) @variable.parameter))
(parameters (list_splat_pattern (identifier) @variable.parameter))
(parameters (dictionary_splat_pattern (identifier) @variable.parameter))
(lambda_parameters (identifier) @variable.parameter)

((parameters (identifier) @variable.builtin)
 (#match? @variable.builtin "^(self|cls)$"))
"#;

const PYTHON_DEFINITION_SUPPLEMENT: &str = r#"
(function_definition name: (identifier) @function.definition)
"#;

/// See [`RUST_DEFINITION_SUPPLEMENT`]. Composed onto the JavaScript query, so it covers TypeScript
/// and TSX too (both inherit it - see [`highlight_query_for`]).
const JAVASCRIPT_DEFINITION_SUPPLEMENT: &str = r#"
(function_declaration name: (identifier) @function.definition)
(generator_function_declaration name: (identifier) @function.definition)
(method_definition
  name: [(property_identifier) (private_property_identifier)] @function.definition)
(variable_declarator
  name: (identifier) @function.definition
  value: [(arrow_function) (function_expression)])
"#;

/// See [`RUST_DEFINITION_SUPPLEMENT`] - the TypeScript-only declaration forms, which have no
/// JavaScript counterpart: an ambient `declare function`, an interface member signature, and an
/// `abstract` class method.
/// Real supplement fixing the closest twin of the [`RUST_VARIABLE_PREFIX`] bug that exists in
/// TypeScript/JavaScript, plus the parameter shapes TypeScript's own query misses.
const TYPESCRIPT_IDENTIFIER_SUPPLEMENT: &str = r#"
((shorthand_property_identifier) @variable
 (#not-match? @variable "^[A-Z_][A-Z\d_]+$"))
((shorthand_property_identifier_pattern) @variable
 (#not-match? @variable "^[A-Z_][A-Z\d_]+$"))

(arrow_function parameter: (identifier) @variable.parameter)
(required_parameter pattern: (rest_pattern (identifier) @variable.parameter))
(required_parameter
  pattern: (object_pattern (shorthand_property_identifier_pattern) @variable.parameter))
"#;

const TYPESCRIPT_DEFINITION_SUPPLEMENT: &str = r#"
(function_signature name: (identifier) @function.definition)
(method_signature name: (property_identifier) @function.definition)
(abstract_method_signature name: (property_identifier) @function.definition)
"#;

/// See [`RUST_DEFINITION_SUPPLEMENT`]. A method's own name node is a `field_identifier`, not an
/// `identifier` - a real difference from `function_declaration`, checked against
/// `tree-sitter-go`'s `node-types.json` rather than assumed symmetrical. `method_elem` is the
/// interface-method node kind in this grammar version (it was `method_spec` in older ones).
/// Real supplement covering three roles `tree-sitter-go`'s own bundled query never emits, each of
/// which a sibling grammar here does emit - the same gap class as [`RUST_VARIABLE_PREFIX`].
const GO_CLASSIFICATION_SUPPLEMENT: &str = r#"
(parameter_declaration name: (identifier) @variable.parameter)
(variadic_parameter_declaration name: (identifier) @variable.parameter)

(const_spec name: (identifier) @constant)

(literal_value (keyed_element key: (literal_element (identifier) @property)))
"#;

const GO_DEFINITION_SUPPLEMENT: &str = r#"
(function_declaration name: (identifier) @function.definition)
(method_declaration name: (field_identifier) @function.definition)
(method_elem name: (field_identifier) @function.definition)
"#;

/// See [`RUST_DEFINITION_SUPPLEMENT`]. C is the one language here that needs the *outer*
/// `function_definition` node in the pattern rather than the declarator alone, and the reason is
/// real rather than stylistic: `(function_declarator declarator: (identifier))` - the shape the
/// bundled query itself uses for `@function` - fires identically on a **prototype**
/// (`int f(int);`) and on a definition (`int f(int) {...}`), so appending it under this name would
/// relabel every prototype in a header as a definition.
const C_DEFINITION_SUPPLEMENT: &str = r#"
(function_definition
  declarator: (function_declarator declarator: (identifier) @function.definition))
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (identifier) @function.definition)))
(function_definition
  declarator: (pointer_declarator
    declarator: (pointer_declarator
      declarator: (function_declarator declarator: (identifier) @function.definition))))
"#;

/// Real supplement appended after the composed JavaScript + TypeScript query (see
/// [`highlight_query_for`]), repairing two regressions that a live old-vs-new diff over real
/// TypeScript caught. Both come from the same root cause: `tree-sitter-typescript`'s own
/// `((identifier) @type (#match? @type "^[A-Z]"))` heuristic and `tree-sitter-javascript`'s
/// keyword list are each individually reasonable, but the concatenation order that makes
/// TypeScript's type rules win over JavaScript's blanket `(identifier) @variable` also lets them
/// win over two JavaScript rules that were actually right.
const TYPESCRIPT_HIGHLIGHTS_SUPPLEMENT: &str = r#"
(function_declaration name: (identifier) @function)
(function_signature name: (identifier) @function)
(call_expression function: (identifier) @function)
(predefined_type "void" @type.builtin)
"#;

/// Real supplement appended after `tree-sitter-md`'s own bundled block `highlights.scm` (GitHub
/// issue #154). Two lines, and the first one is the single thing that makes real per-fence
/// language injection produce *correct* spans rather than shifted, half-missing ones.
const MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT: &str = r#"
(fenced_code_block) @none
(info_string) @text.literal
"#;

/// The real, composed highlights query source for `grammar`, built from the grammar crates' own
/// published `queries/*.scm` files (exposed by each crate as a `&'static str` constant, so nothing
/// here reads from disk or vendors a copy of a query file).
fn highlight_query_for(grammar: Grammar) -> String {
    match grammar {
        Grammar::Rust => format!(
            "{RUST_VARIABLE_PREFIX}\n{}\n{RUST_CONSTANT_SUPPLEMENT}\n\
             {RUST_ATTRIBUTE_SUPPLEMENT}\n{RUST_DEFINITION_SUPPLEMENT}",
            tree_sitter_rust::HIGHLIGHTS_QUERY
        ),
        Grammar::Python => {
            format!(
                "{}\n{PYTHON_HIGHLIGHTS_SUPPLEMENT}\n{PYTHON_BRACKET_SUPPLEMENT}\n\
                 {PYTHON_PARAMETER_SUPPLEMENT}\n{PYTHON_DEFINITION_SUPPLEMENT}",
                tree_sitter_python::HIGHLIGHTS_QUERY
            )
        }
        Grammar::TypeScript => format!(
            "{}\n{}\n{TYPESCRIPT_HIGHLIGHTS_SUPPLEMENT}\n{TYPESCRIPT_IDENTIFIER_SUPPLEMENT}\n\
             {JAVASCRIPT_DEFINITION_SUPPLEMENT}\n{TYPESCRIPT_DEFINITION_SUPPLEMENT}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY
        ),
        Grammar::Tsx => format!(
            "{}\n{}\n{}\n{TYPESCRIPT_HIGHLIGHTS_SUPPLEMENT}\n\
             {TYPESCRIPT_IDENTIFIER_SUPPLEMENT}\n{JAVASCRIPT_DEFINITION_SUPPLEMENT}\n\
             {TYPESCRIPT_DEFINITION_SUPPLEMENT}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY
        ),
        Grammar::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY.to_string(),
        Grammar::Go => format!(
            "{}\n{GO_HIGHLIGHTS_SUPPLEMENT}\n{GO_BRACKET_SUPPLEMENT}\n\
             {GO_CLASSIFICATION_SUPPLEMENT}\n{GO_DEFINITION_SUPPLEMENT}",
            tree_sitter_go::HIGHLIGHTS_QUERY
        ),
        Grammar::Json => format!(
            "{}\n{JSON_HIGHLIGHTS_SUPPLEMENT}\n{JSON_BRACKET_SUPPLEMENT}",
            tree_sitter_json::HIGHLIGHTS_QUERY
        ),
        Grammar::Yaml => tree_sitter_yaml::HIGHLIGHTS_QUERY.to_string(),
        Grammar::C => format!(
            "{}\n{C_BRACKET_SUPPLEMENT}\n{C_DEFINITION_SUPPLEMENT}",
            tree_sitter_c::HIGHLIGHT_QUERY
        ),
        Grammar::Markdown => format!(
            "{}\n{MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT}",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK
        ),
        Grammar::MarkdownInline => tree_sitter_md::HIGHLIGHT_QUERY_INLINE.to_string(),
        // Both plural (`HIGHLIGHTS_QUERY`), unlike `tree-sitter-c`'s and
        // `tree-sitter-javascript`'s singular `HIGHLIGHT_QUERY` above - checked against each
        // crate's own `bindings/rust/lib.rs`, not assumed from a neighbour.
        Grammar::Html => tree_sitter_html::HIGHLIGHTS_QUERY.to_string(),
        Grammar::Css => tree_sitter_css::HIGHLIGHTS_QUERY.to_string(),
    }
}

/// A real, hand-written injection query - **not** `tree_sitter_md::INJECTION_QUERY_BLOCK`
/// (the crate's own bundled `queries/injections.scm`) verbatim, because that file's `(inline)
/// @injection.content (#set! injection.language "markdown_inline")` pattern has two real bugs
/// when driven through `tree-sitter-highlight`'s own generic injection engine (both found by
/// direct empirical testing - a standalone `Highlighter`/`HighlightConfiguration` reproduction
/// outside this app entirely, not assumed from reading the crate's source):
const MARKDOWN_INJECTION_QUERY: &str = r#"
((inline) @injection.content
 (#set! injection.language "markdown_inline")
 (#set! injection.include-children))

((fenced_code_block
   (info_string
     (language) @injection.language)
   (code_fence_content) @injection.content)
 (#set! injection.include-children))

((html_block) @injection.content
 (#set! injection.language "html")
 (#set! injection.include-children))

((minus_metadata) @injection.content
  (#set! injection.language "yaml"))

((plus_metadata) @injection.content
  (#set! injection.language "toml"))
"#;

/// `tree-sitter-md`'s **inline** grammar's own bundled `injections.scm`, verbatim (GitHub issue
/// #154) - the inline half of "including in the markdown files": a raw `<span class="x">` written
/// inline in a paragraph is an `(html_tag)` node, and now really is parsed by
/// [`Grammar::Html`].
const MARKDOWN_INLINE_INJECTION_QUERY: &str = r#"
((html_tag) @injection.content
 (#set! injection.language "html")
 (#set! injection.include-children))

((latex_block) @injection.content
 (#set! injection.language "latex")
 (#set! injection.include-children))
"#;

/// Builds `grammar`'s real [`HighlightConfiguration`], already
/// [`HighlightConfiguration::configure`]d against [`HIGHLIGHT_NAMES`].
fn build_highlight_config(
    grammar: Grammar,
) -> Result<HighlightConfiguration, tree_sitter::QueryError> {
    let injection_query = injection_query_source(grammar);
    let mut config = HighlightConfiguration::new(
        grammar.language(),
        grammar.name(),
        &highlight_query_for(grammar),
        injection_query,
        "",
    )?;
    config.configure(HIGHLIGHT_NAMES);
    Ok(config)
}

/// Process-wide cache of the real [`HighlightConfiguration`]s - **one independent slot per
/// grammar**, each built at most once, and only if that grammar is ever actually asked for.
static HIGHLIGHT_CONFIGS: [OnceLock<Option<HighlightConfiguration>>; Grammar::COUNT] =
    [const { OnceLock::new() }; Grammar::COUNT];

/// The real injection-query source `grammar` drives language injection with, or `""` for a grammar
/// that injects nothing. The single source of truth for both consumers: the
/// [`HighlightConfiguration`] the highlighting engine itself uses ([`build_highlight_config`]), and
/// [`injection_scopes`]' own separate parse. Keeping them on one constant is what stops the two
/// from ever disagreeing about where an injected region begins.
fn injection_query_source(grammar: Grammar) -> &'static str {
    match grammar {
        Grammar::Markdown => MARKDOWN_INJECTION_QUERY,
        Grammar::MarkdownInline => MARKDOWN_INLINE_INJECTION_QUERY,
        Grammar::Html => tree_sitter_html::INJECTIONS_QUERY,
        _ => "",
    }
}

/// Compiled [`injection_query_source`]s, cached per grammar exactly like [`HIGHLIGHT_CONFIGS`] -
/// building a `tree_sitter::Query` costs real work and must not happen per keystroke. `None` for a
/// grammar with no injection query at all, and also for the (test-covered, not expected) case of
/// one that fails to compile.
static INJECTION_QUERIES: [OnceLock<Option<tree_sitter::Query>>; Grammar::COUNT] =
    [const { OnceLock::new() }; Grammar::COUNT];

fn injection_query(grammar: Grammar) -> Option<&'static tree_sitter::Query> {
    INJECTION_QUERIES[grammar.index()]
        .get_or_init(|| {
            let source = injection_query_source(grammar);
            if source.is_empty() {
                return None;
            }
            match tree_sitter::Query::new(&grammar.language(), source) {
                Ok(query) => Some(query),
                Err(error) => {
                    log::error!(
                        "bracket-pair scoping disabled for {}: its injection query failed to \
                         compile: {error}",
                        grammar.name()
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Every real *injected region* in `source`, as byte ranges, in source order - one entry per
/// ` ```rust ` fence body, per markdown `(inline)` node, per `<script>`/`<style>` body, and so on,
/// taken from the grammar's own [`injection_query_source`]'s `@injection.content` captures.
fn injection_scopes(source: &str, grammar: Grammar) -> Vec<(usize, usize)> {
    use streaming_iterator::StreamingIterator;

    let Some(query) = injection_query(grammar) else {
        return Vec::new();
    };
    let Some(content_index) = query
        .capture_names()
        .iter()
        .position(|name| *name == "injection.content")
    else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(query_match) = matches.next() {
        for capture in query_match.captures {
            if capture.index as usize == content_index {
                let node = capture.node;
                if node.start_byte() < node.end_byte() {
                    ranges.push((node.start_byte(), node.end_byte()));
                }
            }
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// [`HighlightSpan::scope`] for a span starting at `start`, given `scopes` from
/// [`injection_scopes`] - the index of the innermost containing range, plus one, or
/// [`OUTER_SCOPE`] when no range contains it.
fn scope_for_byte(scopes: &[(usize, usize)], start: usize) -> u32 {
    let mut scope = OUTER_SCOPE;
    for (index, (range_start, range_end)) in scopes.iter().enumerate() {
        if *range_start > start {
            break;
        }
        if start < *range_end {
            scope = index as u32 + 1;
        }
    }
    scope
}

/// `grammar`'s real, shared [`HighlightConfiguration`], compiling it on first use, or `None` if
/// its query failed to compile.
fn highlight_config(grammar: Grammar) -> Option<&'static HighlightConfiguration> {
    HIGHLIGHT_CONFIGS[grammar.index()]
        .get_or_init(|| match build_highlight_config(grammar) {
            Ok(config) => Some(config),
            Err(error) => {
                log::error!(
                    "syntax highlighting disabled for {}: its highlights query failed to \
                     compile: {error}",
                    grammar.name()
                );
                None
            }
        })
        .as_ref()
}

/// The single real `injection_callback` every [`Highlighter::highlight`] call in this module
/// passes (GitHub issue #154). `tree-sitter-highlight` hands it the language name an
/// `@injection.language` capture or a `#set! injection.language` predicate produced, and takes the
/// returned configuration as the grammar to reparse that injected range with; returning `None`
/// means no layer is created at all and the outer grammar's own classification stands
/// (`highlight.rs:910-917`, read directly).
fn injection_config<'a>(name: &str) -> Option<&'a HighlightConfiguration> {
    highlight_config(Grammar::for_injection_name(name)?)
}

/// Parses `source` with `tree-sitter-rust` and classifies it through the real, official
/// `tree-sitter-highlight` engine driving `tree-sitter-rust`'s own bundled
/// `queries/highlights.scm` - see [`highlight_with`]. Returns an empty `Vec` (rather than
/// panicking) if the grammar's query failed to compile or the parse produced no tree, neither
/// expected in practice.
pub fn highlight_rust(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Rust)
}

/// Parses `source` with `tree-sitter-typescript` and classifies it the same way [`highlight_rust`]
/// does. `is_tsx` selects the real TSX grammar variant (used for `.tsx`/`.jsx` - TSX's grammar is
/// a superset that also parses plain JSX-free TypeScript/JavaScript correctly, and there is no
/// separate JSX-only grammar in `tree-sitter-typescript`) over the plain TypeScript one (used for
/// `.ts`/`.js` - TypeScript's grammar is itself a real syntactic superset of JavaScript, so `.js`
/// deliberately reuses it rather than adding a third grammar dependency for plain JavaScript).
pub fn highlight_typescript(source: &str, is_tsx: bool) -> Vec<HighlightSpan> {
    highlight_with(
        source,
        if is_tsx {
            Grammar::Tsx
        } else {
            Grammar::TypeScript
        },
    )
}

/// A real `fn(&str) -> Vec<HighlightSpan>` wrapper over [`highlight_typescript`] with `is_tsx:
/// false` bound in - `crate::language::HighlighterFn`'s shape has no room for a second `bool`
/// argument, and this is the plain `.ts`/`.js` half of [`highlight_typescript`]'s two real grammar
/// variants (see [`crate::language::EXTENSIONS`]'s `ts`/`js` entries, which wire this in as their
/// real [`crate::language::ExtensionEntry::highlighter`]).
pub fn highlight_ts(source: &str) -> Vec<HighlightSpan> {
    highlight_typescript(source, false)
}

/// The TSX/JSX half of [`highlight_typescript`] - see [`highlight_ts`]'s own docs.
pub fn highlight_tsx(source: &str) -> Vec<HighlightSpan> {
    highlight_typescript(source, true)
}

/// Parses `source` with `tree-sitter-python` and classifies it the same way [`highlight_rust`]
/// does - plus [`PYTHON_HIGHLIGHTS_SUPPLEMENT`], see there.
pub fn highlight_python(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Python)
}

/// Parses `source` with `tree-sitter-toml-ng` and classifies it the same way [`highlight_rust`]
/// does - GitHub issue #32.
pub fn highlight_toml(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Toml)
}

/// Parses `source` with `tree-sitter-go` and classifies it the same way [`highlight_rust`] does -
/// GitHub issue #32.
pub fn highlight_go(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Go)
}

/// Parses `source` with `tree-sitter-json` and classifies it the same way [`highlight_rust`]
/// does - GitHub issue #32.
pub fn highlight_json(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Json)
}

/// Parses `source` with `tree-sitter-yaml` and classifies it the same way [`highlight_rust`]
/// does - GitHub issue #32. Used for both `.yaml` and `.yml` (the same real grammar; there is no
/// separate `.yml`-only variant, matching how `.js` reuses the plain TypeScript grammar).
pub fn highlight_yaml(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Yaml)
}

/// Parses `source` with `tree-sitter-c` and classifies it the same way [`highlight_rust`] does -
/// GitHub issue #32. Used for both `.c` and `.h` (a header is still real C syntax to this
/// grammar; there is no separate declarations-only variant).
pub fn highlight_c(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::C)
}

/// Parses `source` with `tree-sitter-html` and classifies it the same way [`highlight_rust`] does -
/// GitHub issue #154. Used for both `.html` and `.htm`.
pub fn highlight_html(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Html)
}

/// Parses `source` with `tree-sitter-css` and classifies it the same way [`highlight_rust`] does -
/// GitHub issue #154. `tree-sitter-css` bundles no `injections.scm`, so this is a plain
/// single-grammar path.
pub fn highlight_css(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Css)
}

/// The one real highlighting path every language wrapper above funnels into: run the official
/// [`tree_sitter_highlight::Highlighter`] over `source` with `grammar`'s real
/// [`HighlightConfiguration`], and fold the [`HighlightEvent`] stream it emits down into this
/// app's six [`HighlightKind`] buckets.
fn highlight_with(source: &str, grammar: Grammar) -> Vec<HighlightSpan> {
    let Some(config) = highlight_config(grammar) else {
        return Vec::new();
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(config, source.as_bytes(), None, injection_config)
    else {
        return Vec::new();
    };
    // Empty (and free - no parse at all) for the ten grammars that inject nothing; see
    // `injection_scopes` for why the three that do inject pay a second parse.
    let scopes = injection_scopes(source, grammar);
    fold_highlight_events(events, &scopes)
}

/// Parses `source` with `tree-sitter-md`'s real block grammar and classifies it through the same
/// `tree-sitter-highlight` engine [`highlight_with`] uses - GitHub issue #104. Unlike every other
/// grammar in this module, Markdown's own block grammar never parses prose content itself: real
/// text (emphasis, links, code spans, ...) lives inside `(inline)` nodes the block grammar leaves
/// opaque, and only [`Grammar::MarkdownInline`] - reached through the real injection callback
/// [`highlight_with`] passes, resolving `MARKDOWN_INJECTION_QUERY`'s own `(#set!
/// injection.language "markdown_inline")` rule - actually parses it. Without that callback, every
/// real markdown document would render as a single flat `Text` region: verified directly by
/// testing this function with `|_| None` in place first
/// (`inline_content_is_never_left_as_a_single_flat_text_region`'s own premise).
pub fn highlight_markdown(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Markdown)
}

/// The one real event-folding path every highlighting entry point in this module funnels into:
/// collapses a [`tree_sitter_highlight::Highlighter::highlight`] event stream down into
/// this app's [`HighlightKind`] buckets.
fn fold_highlight_events(
    events: impl Iterator<Item = Result<HighlightEvent, tree_sitter_highlight::Error>>,
    scopes: &[(usize, usize)],
) -> Vec<HighlightSpan> {
    let mut spans: Vec<HighlightSpan> = Vec::new();
    let mut open: Vec<HighlightKind> = Vec::new();
    for event in events {
        let Ok(event) = event else {
            break;
        };
        match event {
            HighlightEvent::HighlightStart(Highlight(index)) => {
                // `index` is an index into `HIGHLIGHT_NAMES`, which `HIGHLIGHT_KINDS` is a
                // compile-time-length-checked parallel array of - so this cannot be out of range.
                open.push(HIGHLIGHT_KINDS[index]);
            }
            HighlightEvent::HighlightEnd => {
                open.pop();
            }
            HighlightEvent::Source { start, end } => {
                if start >= end {
                    continue;
                }
                let kind = open.last().copied().unwrap_or(HighlightKind::Text);
                let scope = scope_for_byte(scopes, start);
                // Coalesce with the previous span when it is both adjacent and the same bucket.
                // Real, not cosmetic: the engine splits `Source` at every highlight boundary, so a
                // keyword list like Rust's `"as" @keyword ... "async" @keyword ...` arrives as one
                // `Source` event per anonymous token even though they're all the same real
                // `Keyword` bucket. Merging here keeps `build_lines`' per-line run lists (and the
                // `SharedString` allocation each run costs at render time) proportional to what is
                // actually visually distinct - note this deliberately does *not* merge a `String`
                // run with an adjacent `StringEscape` one (different buckets since GitHub issue
                // #31), so an escape sequence stays its own real, separately-classified span
                // rather than disappearing into the surrounding string - see
                // `escapes_inside_a_string_are_their_own_real_string_escape_run`.
                match spans.last_mut() {
                    Some(previous)
                        if previous.end == start
                            && previous.kind == kind
                            && previous.scope == scope =>
                    {
                        previous.end = end;
                    }
                    _ => spans.push(HighlightSpan {
                        start,
                        end,
                        kind,
                        scope,
                    }),
                }
            }
        }
    }
    spans
}

/// Byte ranges within `text` that look like a JSDoc-style doc-comment tag - GitHub issue #200. Two
/// real shapes, matching the JSDoc spec's own syntax:
pub(crate) fn doc_tag_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{'
            && bytes.get(index + 1) == Some(&b'@')
            && bytes.get(index + 2).is_some_and(u8::is_ascii_alphabetic)
        {
            if let Some(relative_close) = text[index..].find('}') {
                let end = index + relative_close + 1;
                ranges.push(index..end);
                index = end;
                continue;
            }
        }
        let preceded_by_identifier_byte = index
            .checked_sub(1)
            .and_then(|previous| bytes.get(previous))
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
        if bytes[index] == b'@'
            && !preceded_by_identifier_byte
            && bytes.get(index + 1).is_some_and(u8::is_ascii_alphabetic)
        {
            let start = index;
            let mut end = index + 1;
            while bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
                end += 1;
            }
            ranges.push(start..end);
            index = end;
            continue;
        }
        index += 1;
    }
    ranges
}

/// Reclassifies a `/** ... */`-shaped [`HighlightKind::Comment`] span as
/// [`HighlightKind::CommentDoc`], then splits every `Comment`/`CommentDoc` span's own real
/// [`doc_tag_ranges`] out into separate [`HighlightKind::CommentDocTag`] sub-spans - GitHub issue
/// #200's editor-side half. Runs unconditionally (not gated by [`HighlightOptions`] the way
/// [`colorize_bracket_pairs`] is - there's no real reason a user would want doc tags left flat)
/// after the grammar's own query, since no bundled grammar this app ships parses *inside* a
/// comment body at all - the same "post-process on top of the grammar's own output" shape
/// [`colorize_bracket_pairs`] already established for the bracket-pair ring.
fn split_doc_comment_tags(source: &str, spans: Vec<HighlightSpan>) -> Vec<HighlightSpan> {
    let mut result = Vec::with_capacity(spans.len());
    for span in spans {
        if !matches!(
            span.kind,
            HighlightKind::Comment | HighlightKind::CommentDoc
        ) {
            result.push(span);
            continue;
        }
        let Some(text) = source.get(span.start..span.end) else {
            result.push(span);
            continue;
        };
        let kind = if span.kind == HighlightKind::Comment
            && text.starts_with("/**")
            && !text.starts_with("/**/")
        {
            HighlightKind::CommentDoc
        } else {
            span.kind
        };
        let tag_ranges = doc_tag_ranges(text);
        if tag_ranges.is_empty() {
            result.push(HighlightSpan { kind, ..span });
            continue;
        }
        let mut cursor = 0;
        for tag_range in tag_ranges {
            if tag_range.start > cursor {
                result.push(HighlightSpan {
                    start: span.start + cursor,
                    end: span.start + tag_range.start,
                    kind,
                    scope: span.scope,
                });
            }
            result.push(HighlightSpan {
                start: span.start + tag_range.start,
                end: span.start + tag_range.end,
                kind: HighlightKind::CommentDocTag,
                scope: span.scope,
            });
            cursor = tag_range.end;
        }
        if cursor < text.len() {
            result.push(HighlightSpan {
                start: span.start + cursor,
                end: span.end,
                kind,
                scope: span.scope,
            });
        }
    }
    result
}

/// Which optional, settings-gated post-processes run over a freshly classified span list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightOptions {
    /// GitHub issue #168's bracket-pair depth ring - `crate::settings::store::AppearanceSettings`'
    /// own `bracket_pair_colorization`. When `false`, [`colorize_bracket_pairs`] genuinely does
    /// not run: brackets keep the flat [`HighlightKind::PunctuationBracket`] the grammar gave
    /// them, which is aliased to plain text, so the result is byte-for-byte what this module
    /// produced before the feature existed - not a recoloured imitation of it.
    pub bracket_pair_colorization: bool,
}

impl Default for HighlightOptions {
    fn default() -> Self {
        Self {
            bracket_pair_colorization: true,
        }
    }
}

impl HighlightOptions {
    /// Runs whichever post-processes this options set enables over `spans`.
    pub fn apply(self, source: &str, spans: Vec<HighlightSpan>) -> Vec<HighlightSpan> {
        let spans = split_doc_comment_tags(source, spans);
        if self.bracket_pair_colorization {
            colorize_bracket_pairs(source, spans)
        } else {
            spans
        }
    }

    /// Classifies `source` with `highlighter` and applies this options set - the whole
    /// highlight-a-string pipeline in one call, so no caller has to remember that the two steps
    /// belong together.
    pub fn highlight(
        self,
        source: &str,
        highlighter: Option<crate::language::HighlighterFn>,
    ) -> Vec<HighlightSpan> {
        match highlighter {
            Some(highlighter) => self.apply(source, highlighter(source)),
            None => Vec::new(),
        }
    }
}

/// The three bracket shapes [`colorize_bracket_pairs`] really tracks, opener paired with closer.
/// See that function's docs for why `<`/`>` is deliberately not a fourth entry.
const TRACKED_BRACKET_PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// The closer that matches `opener`, or `None` if `opener` isn't a tracked opening bracket.
fn closer_for(opener: char) -> Option<char> {
    TRACKED_BRACKET_PAIRS
        .iter()
        .find(|(open, _)| *open == opener)
        .map(|(_, close)| *close)
}

/// Whether `ch` is a tracked *closing* bracket.
fn is_tracked_closer(ch: char) -> bool {
    TRACKED_BRACKET_PAIRS.iter().any(|(_, close)| *close == ch)
}

/// GitHub issue #168's bracket-pair colourization: rewrites `spans` so that every `(`/`[`/`{`
/// which really has a matching partner - and that partner - carry the same
/// [`HighlightKind::for_bracket_depth`] ring colour for their pair's nesting depth, instead of the
/// flat [`HighlightKind::PunctuationBracket`] the grammar's own capture gave them.
pub fn colorize_bracket_pairs(source: &str, spans: Vec<HighlightSpan>) -> Vec<HighlightSpan> {
    // Pass 1a: find every tracked bracket character, in source order, and work out which ones
    // are halves of a real pair - a single-stack matcher, exactly as before, except its only real
    // job now is the boolean "did this occurrence ever find a real partner", not depth. `depths`
    // is computed fresh in pass 1b below (see that pass's own docs for why a first-pass depth is
    // wrong for GitHub issue #182). `bracket_bytes`/`bracket_chars`/`bracket_scopes` record every
    // occurrence in source order, so pass 1b can walk the identical sequence a second time and
    // pass 2 can `debug_assert` it too - all three passes iterating the same spans by the same
    // rule is what would catch them ever drifting apart and silently colouring the wrong
    // characters.
    let mut bracket_bytes: Vec<usize> = Vec::new();
    let mut bracket_chars: Vec<char> = Vec::new();
    let mut bracket_scopes: Vec<u32> = Vec::new();
    let mut matched: Vec<bool> = Vec::new();
    // One independent stack per `HighlightSpan::scope` - see this function's own "Injected regions
    // pair independently" docs. Each entry is (expected closer, index into `matched` of the opener).
    let mut stacks: HashMap<u32, Vec<(char, usize)>> = HashMap::new();

    for span in &spans {
        if span.kind != HighlightKind::PunctuationBracket {
            continue;
        }
        let Some(text) = source.get(span.start..span.end) else {
            continue;
        };
        let stack = stacks.entry(span.scope).or_default();
        for (offset, ch) in text.char_indices() {
            // Only a *tracked* opener/closer (`(`/`)`/`[`/`]`/`{`/`}` - see
            // `TRACKED_BRACKET_PAIRS`) gets a real occurrence slot at all; an untracked
            // punctuation character sharing this span's `PunctuationBracket` kind (an angle
            // bracket, most commonly - deliberately never tracked, see this function's own docs)
            // must stay invisible to `bracket_bytes` the same way it always has, or pass 2's own
            // `next_bracket` walk (which only ever advances on a tracked char) desyncs from it.
            if closer_for(ch).is_none() && !is_tracked_closer(ch) {
                continue;
            }
            let index = bracket_bytes.len();
            bracket_bytes.push(span.start + offset);
            bracket_chars.push(ch);
            bracket_scopes.push(span.scope);
            matched.push(false);
            if let Some(closer) = closer_for(ch) {
                stack.push((closer, index));
            } else if let Some(&(expected, opener_index)) = stack.last() {
                if expected == ch {
                    stack.pop();
                    matched[opener_index] = true;
                    matched[index] = true;
                }
            }
        }
    }

    if bracket_bytes.is_empty() {
        return spans;
    }

    // Pass 1b (GitHub issue #182): recomputes `depths` over only the occurrences pass 1a found a
    // real partner for, in effect dropping every unmatched opener from the stack retroactively -
    // the fix the issue itself proposes. Pass 1a's own single stack counts an opener's depth as
    // the stack's size *at push time*, which is wrong whenever an *earlier* opener in the same
    // scope never gets a real closer: that earlier opener never leaves the stack, so it inflates
    // the depth of every real, well-formed pair that follows it for the rest of the scope - one
    // unterminated `(` mid-edit used to shift the whole rest of the file's ring by one. Walking
    // the identical occurrence sequence a second time, skipping anything `matched` says never
    // found a real partner, means the depth stack here only ever holds genuinely open, genuinely
    // real pairs - exactly what the ring is supposed to reflect.
    let mut depths: Vec<Option<usize>> = vec![None; bracket_bytes.len()];
    let mut real_pair_stacks: HashMap<u32, Vec<usize>> = HashMap::new();
    for index in 0..bracket_bytes.len() {
        if !matched[index] {
            continue;
        }
        let stack = real_pair_stacks.entry(bracket_scopes[index]).or_default();
        if closer_for(bracket_chars[index]).is_some() {
            depths[index] = Some(stack.len());
            stack.push(index);
        } else {
            // `matched[index]` already guarantees this closer has a real partner somewhere
            // earlier in this same scope, and skipping every unmatched occurrence keeps this
            // stack's push/pop order identical to the real pairs alone - so the top of the stack
            // here is always that exact partner.
            if let Some(opener_index) = stack.pop() {
                depths[index] = depths[opener_index];
            }
        }
    }

    // Pass 2: rebuild the span list, splitting each `PunctuationBracket` span into its individual
    // characters so a matched bracket can carry its own ring colour, then re-coalescing.
    let mut out: Vec<HighlightSpan> = Vec::with_capacity(spans.len());
    let mut next_bracket = 0usize;
    for span in spans {
        if span.kind != HighlightKind::PunctuationBracket {
            push_coalesced(&mut out, span);
            continue;
        }
        let Some(text) = source.get(span.start..span.end) else {
            push_coalesced(&mut out, span);
            continue;
        };
        for (offset, ch) in text.char_indices() {
            let start = span.start + offset;
            let mut kind = HighlightKind::PunctuationBracket;
            if closer_for(ch).is_some() || is_tracked_closer(ch) {
                debug_assert_eq!(
                    bracket_bytes.get(next_bracket),
                    Some(&start),
                    "pass 2 walked a different bracket sequence than pass 1"
                );
                if let Some(Some(depth)) = depths.get(next_bracket) {
                    kind = HighlightKind::for_bracket_depth(*depth);
                }
                next_bracket += 1;
            }
            push_coalesced(
                &mut out,
                HighlightSpan {
                    start,
                    end: start + ch.len_utf8(),
                    kind,
                    scope: span.scope,
                },
            );
        }
    }
    out
}

/// Appends `span`, merging it into the previous one when they're adjacent and the same bucket -
/// the same invariant [`fold_highlight_events`] maintains, restored after
/// [`colorize_bracket_pairs`] splits a multi-character bracket run apart.
fn push_coalesced(spans: &mut Vec<HighlightSpan>, span: HighlightSpan) {
    match spans.last_mut() {
        Some(previous)
            if previous.end == span.start
                && previous.kind == span.kind
                && previous.scope == span.scope =>
        {
            previous.end = span.end;
        }
        _ => spans.push(span),
    }
}

/// One already-highlighted display line: its text (never including line-ending bytes) plus a
/// gapless run list covering every byte of it (unhighlighted stretches are explicit
/// [`HighlightKind::Text`] runs) - computed once by [`build_lines`] and cached in [`ParsedFile`],
/// never recomputed per render.
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
pub(crate) fn build_lines(source: &str, spans: &[HighlightSpan]) -> Vec<RenderedLine> {
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
        // `crate::code_surface::file_view::render_file_view_line` on every render - see `RenderedLine`'s docs.
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

/// Real line-boundary byte ranges within `source`, excluding line-ending bytes (`\n`, and a
/// preceding `\r` for a CRLF line) - the same ranges [`build_lines`] slices `RenderedLine::text`
/// from. `pub(crate)` (not private) so `crate::code_surface::edit_buffer::EditBuffer` can derive its own
/// byte-offset<->line/column mapping from exactly this function rather than a second,
/// independently-maintained line-splitting implementation that could silently disagree with what
/// [`build_lines`] actually displays (a real CRLF off-by-one bug class this sharing avoids).
pub(crate) fn line_ranges(source: &str) -> Vec<Range<usize>> {
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

/// Highlights an isolated block of lines - a diff hunk's interleaved add/remove/context lines,
/// or one side of a merge conflict hunk - as its own real source unit: joins `lines` with `\n`,
/// runs `extension`'s real highlighter (if any), and re-splits via [`build_lines`]. Shared by
/// `crate::code_surface`'s Diff view and `crate::merge::render`'s Merge view so
/// both highlight the same way [`load_file`] does, rather than each re-deriving the
/// join-highlight-split recipe.
pub(crate) fn highlight_block<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    extension: Option<&str>,
    options: HighlightOptions,
) -> Vec<RenderedLine> {
    let lines: Vec<&str> = lines.into_iter().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let source = lines.join("\n");
    let spans = options.highlight(&source, highlighter_for_extension(extension));
    build_lines(&source, &spans)
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
    Ok(load_file_with_source(path)?.0)
}

/// [`load_file_with_source`] with a real, caller-supplied [`HighlightOptions`] - what
/// `crate::code_surface::tabs::AdeApp::spawn_file_load` actually calls, having read the user's own
/// setting on the foreground thread before spawning the background read.
pub fn load_file_with_options(
    path: &Path,
    options: HighlightOptions,
) -> io::Result<(ParsedFile, String)> {
    load_file_inner(path, options)
}

/// [`load_file`]'s real implementation, also handing back the decoded source text - used by
/// `crate::root::AdeApp::spawn_file_load` to lazily seed a `crate::code_surface::edit_buffer::EditBuffer` from
/// the exact same background read/decode this already does, rather than a second, independent
/// disk read of the same file. `load_file` itself is now a thin wrapper that discards the source,
/// kept as the public entry point every other existing caller (and this module's own tests)
/// already uses unchanged.
pub fn load_file_with_source(path: &Path) -> io::Result<(ParsedFile, String)> {
    load_file_inner(path, HighlightOptions::default())
}

fn load_file_inner(path: &Path, options: HighlightOptions) -> io::Result<(ParsedFile, String)> {
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
    let spans = options.highlight(&source, highlighter_for_extension(extension));
    let lines = build_lines(&source, &spans);

    let parsed = ParsedFile {
        path: path.to_path_buf(),
        mtime,
        len,
        language,
        line_ending,
        truncated,
        is_valid_utf8,
        lines,
    };
    Ok((parsed, source))
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
/// `crate::sidebar::changes::parse_hunk_new_range`. Context lines advance the new-file line counter
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

    /// GitHub issue #141: a live theme palette really changes `color_for_kind`'s output for the
    /// buckets it names, and leaves every other bucket completely alone. `Drop`-guarded: Rust's
    /// default test harness reuses worker threads across different tests, so a `thread_local!`
    /// left non-default here could leak into a completely unrelated test scheduled on the same
    /// worker later - the same real concern `crate::theme::CURRENT_THEME`'s own docs document.
    struct ResetThemeOnDrop;
    impl Drop for ResetThemeOnDrop {
        fn drop(&mut self) {
            theme::set_current_theme(None);
        }
    }

    #[test]
    fn color_for_kind_follows_a_live_theme_palette_for_exactly_the_scopes_it_names() {
        let _guard = ResetThemeOnDrop;
        let default_keyword = color_for_kind(HighlightKind::Keyword);
        let default_string = color_for_kind(HighlightKind::String);

        let overridden = gpui::Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.5,
            a: 1.0,
        };
        let mut palette = theme::Palette::new();
        palette.insert("syntax.keyword", overridden);
        theme::set_current_theme(Some(std::rc::Rc::new(palette)));

        assert_eq!(
            color_for_kind(HighlightKind::Keyword),
            overridden,
            "a bucket the live theme names must return the theme's own real colour, not the \
             compiled default"
        );
        assert_eq!(
            color_for_kind(HighlightKind::String),
            default_string,
            "a bucket the theme names nothing for must be completely unaffected"
        );

        theme::set_current_theme(None);
        assert_eq!(
            color_for_kind(HighlightKind::Keyword),
            default_keyword,
            "clearing the theme must really restore the compiled default, not leave the \
             last-set colour stuck"
        );
    }

    #[test]
    fn detect_line_ending_reads_the_real_bytes_and_defaults_to_lf() {
        let cases: &[(&[u8], LineEnding)] = &[
            (b"fn main() {\n}\n", LineEnding::Lf),
            (b"fn main() {\r\n}\r\n", LineEnding::Crlf),
            // Nothing to read: a file with no newline at all is LF, not "unknown".
            (b"no newline here", LineEnding::Lf),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                detect_line_ending(bytes),
                *expected,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    fn plain_line(text: &str) -> RenderedLine {
        RenderedLine {
            text: text.to_string(),
            runs: Vec::new(),
        }
    }

    /// Every shape `detect_indent_width` has to get right, in one table. The two "one-off" rows
    /// are the audit's own reproductions: a C-style block-comment header's single leading space
    /// and a hanging-indent continuation line must each lose to the file's real, repeated indent
    /// unit rather than being picked just for appearing first.
    #[test]
    fn detect_indent_width_picks_the_files_real_repeated_indent_unit() {
        let cases: &[(&str, &[&str], Option<usize>)] = &[
            (
                "plain space indent",
                &["fn main() {", "    let x = 1;", "}"],
                Some(4),
            ),
            // A tab-indented line has no single "N spaces" answer - keep scanning.
            (
                "tab lines skipped",
                &["fn main() {", "\tlet x = 1;", "  let y = 2;"],
                Some(2),
            ),
            // A blank/whitespace-only line says nothing about the real indent unit.
            (
                "blank lines skipped",
                &["fn main() {", "    ", "      let x = 1;"],
                Some(6),
            ),
            ("no indentation at all", &["fn main() {", "}"], None),
            ("empty file", &[], None),
            (
                "block comment header",
                &[
                    "/**",
                    " * Copyright 2024 Example Corp.",
                    " */",
                    "fn main() {",
                    "    let x = 1;",
                    "    let y = 2;",
                    "    let z = 3;",
                    "}",
                ],
                Some(4),
            ),
            (
                "hanging indent continuation",
                &[
                    "let long_name =",
                    "  some_call(a, b);",
                    "fn main() {",
                    "    let x = 1;",
                    "    let y = 2;",
                    "}",
                ],
                Some(4),
            ),
            // An exact tie in occurrence count prefers the smaller, more conventional width.
            (
                "tie breaks smaller",
                &["  two spaces", "    four spaces"],
                Some(2),
            ),
        ];
        for (label, texts, expected) in cases {
            let lines: Vec<RenderedLine> = texts.iter().map(|text| plain_line(text)).collect();
            assert_eq!(detect_indent_width(&lines), *expected, "{label}");
        }
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

    /// Finds the span whose byte range is *exactly* `text`'s first occurrence in `source`.
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

    /// The [`HighlightKind`] covering the first byte of `text`'s first occurrence in `source` -
    /// i.e. exactly the colour the File view would paint that token's first character. Falls back
    /// to [`HighlightKind::Text`] for a byte no span covers, matching [`build_lines`]' own
    /// gap-filling rule, so this always answers the same question the renderer does.
    pub(super) fn kind_at(spans: &[HighlightSpan], source: &str, text: &str) -> HighlightKind {
        let start = source.find(text).expect("substring present in source");
        spans
            .iter()
            .find(|span| span.start <= start && start < span.end)
            .map_or(HighlightKind::Text, |span| span.kind)
    }

    const SAMPLE_RUST: &str =
        "/// Adds one.\nfn add(left: i32) -> i32 {\n    let name = \"x\";\n    left + 1\n}\n";

    // Real TypeScript highlighting coverage - mirrors `highlight_rust`'s own test shape above.
    // Before this fix, none of `highlight_typescript`'s real, common-case behavior had any test
    // coverage at all.

    const SAMPLE_TYPESCRIPT: &str = "/** Adds one. */\nfunction add(left: number): number {\n    const name = \"x\";\n    return left + 1;\n}\n";

    /// One row per real token this module's docs claim a bucket for, across every grammar - the
    /// table form of what used to be one `#[test]` per row.
    ///
    /// Each row asks exactly what the renderer asks: which [`HighlightKind`] covers this token's
    /// first byte ([`kind_at`]). Behaviour that is *not* a single token -> single bucket lookup
    /// (definition vs. call site, casing sweeps, injected fences, bracket rings) keeps its own
    /// test below; this table is only for the flat ones.
    #[test]
    fn every_documented_token_lands_in_its_documented_bucket() {
        let cases: &[(
            &str,
            crate::language::HighlighterFn,
            &str,
            &str,
            HighlightKind,
        )] = &[
            (
                "rust",
                highlight_rust,
                SAMPLE_RUST,
                "fn",
                HighlightKind::Keyword,
            ),
            (
                "rust",
                highlight_rust,
                SAMPLE_RUST,
                "add",
                HighlightKind::FunctionDefinition,
            ),
            (
                "rust",
                highlight_rust,
                SAMPLE_RUST,
                "\"x\"",
                HighlightKind::String,
            ),
            (
                "rust",
                highlight_rust,
                SAMPLE_RUST,
                "i32",
                HighlightKind::TypeBuiltin,
            ),
            (
                "rust",
                highlight_rust,
                SAMPLE_RUST,
                "/// Adds one.",
                HighlightKind::CommentDoc,
            ),
            (
                "ts",
                highlight_ts,
                SAMPLE_TYPESCRIPT,
                "function",
                HighlightKind::Keyword,
            ),
            (
                "ts",
                highlight_ts,
                SAMPLE_TYPESCRIPT,
                "add",
                HighlightKind::FunctionDefinition,
            ),
            (
                "ts",
                highlight_ts,
                SAMPLE_TYPESCRIPT,
                "\"x\"",
                HighlightKind::String,
            ),
            (
                "ts",
                highlight_ts,
                SAMPLE_TYPESCRIPT,
                "number",
                HighlightKind::TypeBuiltin,
            ),
            (
                "ts",
                highlight_ts,
                SAMPLE_TYPESCRIPT,
                "/** Adds one. */",
                HighlightKind::Comment,
            ),
            // `(regex) @string.special`, its own bucket since GitHub issue #183 - it used to
            // fall through to the plain `"string"` entry and read as an ordinary string.
            (
                "ts",
                highlight_ts,
                "const pattern = /^[a-z]+$/;\n",
                "^[a-z]+$",
                HighlightKind::StringSpecial,
            ),
            // The three `name`-field collisions that all used to come out `Function`: a
            // `const` binding, an `interface` member, and a class method (which really is one).
            (
                "ts",
                highlight_ts,
                "const s: string = \"hi\";\n",
                "s: string",
                HighlightKind::Variable,
            ),
            (
                "ts",
                highlight_ts,
                "interface Point { x: number }\n",
                "x: number",
                HighlightKind::Property,
            ),
            (
                "ts",
                highlight_ts,
                "class Point {\n    length() {\n        return 0;\n    }\n}\n",
                "length",
                HighlightKind::FunctionDefinition,
            ),
            // A TSX tag name is the same collision once more, and gets its own `Tag` bucket.
            (
                "tsx",
                highlight_tsx,
                "const el = <div />;\n",
                "div",
                HighlightKind::Tag,
            ),
            (
                "python",
                highlight_python,
                SAMPLE_PYTHON,
                "def",
                HighlightKind::Keyword,
            ),
            (
                "python",
                highlight_python,
                SAMPLE_PYTHON,
                "add",
                HighlightKind::FunctionDefinition,
            ),
            (
                "python",
                highlight_python,
                SAMPLE_PYTHON,
                "\"x\"",
                HighlightKind::String,
            ),
            (
                "python",
                highlight_python,
                SAMPLE_PYTHON,
                "int",
                HighlightKind::Type,
            ),
            (
                "python",
                highlight_python,
                "# a real comment\nx = 1\n",
                "# a real comment",
                HighlightKind::Comment,
            ),
            // A method name and a class name are two different `name` fields of two different
            // node kinds, told apart inside one real parse.
            (
                "python",
                highlight_python,
                "class Foo:\n    def bar(self):\n        pass\n",
                "bar",
                HighlightKind::FunctionDefinition,
            ),
            (
                "toml",
                highlight_toml,
                SAMPLE_TOML,
                "name",
                HighlightKind::Property,
            ),
            (
                "toml",
                highlight_toml,
                SAMPLE_TOML,
                "\"jerry\"",
                HighlightKind::String,
            ),
            (
                "toml",
                highlight_toml,
                SAMPLE_TOML,
                "true",
                HighlightKind::ConstantBuiltin,
            ),
            (
                "toml",
                highlight_toml,
                SAMPLE_TOML,
                "1979-05-27",
                HighlightKind::StringSpecial,
            ),
            (
                "go",
                highlight_go,
                SAMPLE_GO,
                "func",
                HighlightKind::Keyword,
            ),
            (
                "go",
                highlight_go,
                SAMPLE_GO,
                "add",
                HighlightKind::FunctionDefinition,
            ),
            (
                "go",
                highlight_go,
                SAMPLE_GO,
                "len(",
                HighlightKind::FunctionBuiltin,
            ),
            // A JSON key must win the more specific `string.special.key` registration over the
            // plain `string` one; a JSON *value* must not.
            (
                "json",
                highlight_json,
                SAMPLE_JSON,
                "\"name\"",
                HighlightKind::Property,
            ),
            (
                "json",
                highlight_json,
                SAMPLE_JSON,
                "\"jerry\"",
                HighlightKind::String,
            ),
            (
                "json",
                highlight_json,
                SAMPLE_JSON,
                "3",
                HighlightKind::Number,
            ),
            (
                "yaml",
                highlight_yaml,
                SAMPLE_YAML,
                "count",
                HighlightKind::Property,
            ),
            (
                "yaml",
                highlight_yaml,
                SAMPLE_YAML,
                "true",
                HighlightKind::ConstantBuiltin,
            ),
            // `(anchor_name) @label` is the YAML half of the cross-language `"label"`
            // registration; the C `goto` target below is its other half.
            (
                "yaml",
                highlight_yaml,
                SAMPLE_YAML,
                "base\n",
                HighlightKind::Label,
            ),
            (
                "yaml",
                highlight_yaml,
                SAMPLE_YAML,
                "&base",
                HighlightKind::PunctuationSpecial,
            ),
            (
                "yaml",
                highlight_yaml,
                SAMPLE_YAML,
                "*base",
                HighlightKind::PunctuationSpecial,
            ),
            ("c", highlight_c, SAMPLE_C, "return", HighlightKind::Keyword),
            (
                "c",
                highlight_c,
                SAMPLE_C,
                "add",
                HighlightKind::FunctionDefinition,
            ),
            ("c", highlight_c, SAMPLE_C, "done:", HighlightKind::Label),
            (
                "c",
                highlight_c,
                SAMPLE_C,
                ";",
                HighlightKind::PunctuationDelimiter,
            ),
            (
                "markdown",
                highlight_markdown,
                SAMPLE_MARKDOWN,
                "Title",
                HighlightKind::Heading,
            ),
            (
                "markdown",
                highlight_markdown,
                SAMPLE_MARKDOWN,
                "bold",
                HighlightKind::Strong,
            ),
            (
                "markdown",
                highlight_markdown,
                SAMPLE_MARKDOWN,
                "italic",
                HighlightKind::Emphasis,
            ),
            (
                "markdown",
                highlight_markdown,
                SAMPLE_MARKDOWN,
                "inline code",
                HighlightKind::String,
            ),
            (
                "markdown",
                highlight_markdown,
                SAMPLE_MARKDOWN,
                "https://example.com",
                HighlightKind::Link,
            ),
            (
                "markdown",
                highlight_markdown,
                SAMPLE_MARKDOWN,
                "link",
                HighlightKind::Link,
            ),
            // `string.special` must not match JSON's more specific `string.special.key`: the
            // recognized-name rule needs every dot-part of the name present in the capture.
            (
                "css",
                highlight_css,
                SAMPLE_CSS,
                "ff0000",
                HighlightKind::StringSpecial,
            ),
        ];
        for (label, highlighter, source, token, expected) in cases {
            assert_eq!(
                kind_at(&highlighter(source), source, token),
                *expected,
                "{label}: {token:?}"
            );
        }
    }

    /// Tree-sitter produces a best-effort tree for malformed input rather than failing outright:
    /// every grammar must still classify the keyword it *can* see, and none may panic.
    #[test]
    fn highlighting_invalid_input_still_returns_a_real_non_empty_span_list() {
        let cases: &[(&str, crate::language::HighlighterFn, &str)] = &[
            ("rust", highlight_rust, "fn (((( broken"),
            ("typescript", highlight_ts, "function (((( broken"),
            ("python", highlight_python, "def (((( broken"),
        ];
        for (label, highlighter, source) in cases {
            let spans = highlighter(source);
            assert!(
                spans.iter().any(|span| span.kind == HighlightKind::Keyword),
                "{label}: malformed input must still classify its real leading keyword"
            );
        }
    }

    // GitHub issue #200's own doc-tag coverage. `every_documented_token_lands_in_its_documented_
    // bucket`'s TypeScript doc-comment row above deliberately calls `highlight_ts` directly - the
    // raw grammar layer, bypassing `HighlightOptions::apply` entirely (same reason
    // `colorize_bracket_pairs`'s own tests do this) - so it stays correct unchanged:
    // `split_doc_comment_tags` only ever runs inside `apply()`, which every real, live rendering
    // path (`load_file_with_options`, `highlight_block`) goes through but that pinned raw-grammar
    // row doesn't.

    #[test]
    fn doc_tag_ranges_recognizes_every_real_tag_shape_and_nothing_else() {
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "block tags",
                "Adds one.\n@param left the number to add to\n@returns the sum",
                &["@param", "@returns"],
            ),
            // An `@` directly preceded by an identifier byte (the `o` in `foo`) is an email
            // address, not a tag.
            ("email address", "Contact foo@example.com for details.", &[]),
            (
                "inline link tag",
                "See {@link Foo#bar} for more.",
                &["{@link Foo#bar}"],
            ),
            (
                "unclosed inline tag",
                "See {@link Foo#bar for more.",
                &["@link"],
            ),
        ];
        for (label, text, expected) in cases {
            let ranges: Vec<&str> = doc_tag_ranges(text)
                .into_iter()
                .map(|range| &text[range])
                .collect();
            assert_eq!(ranges, *expected, "{label}");
        }
    }

    #[test]
    fn a_real_typescript_block_doc_comment_is_promoted_to_comment_doc_through_the_real_pipeline() {
        let spans = HighlightOptions::default().highlight(SAMPLE_TYPESCRIPT, Some(highlight_ts));
        let span =
            find_span(&spans, SAMPLE_TYPESCRIPT, "/** Adds one. */").expect("doc comment span");
        assert_eq!(span.kind, HighlightKind::CommentDoc);
    }

    #[test]
    fn a_plain_typescript_line_comment_is_not_promoted_to_comment_doc() {
        let source = "// just a plain comment\nconst x = 1;\n";
        let spans = HighlightOptions::default().highlight(source, Some(highlight_ts));
        let span = find_span(&spans, source, "// just a plain comment").expect("comment span");
        assert_eq!(span.kind, HighlightKind::Comment);
    }

    #[test]
    fn an_empty_block_comment_is_not_misclassified_as_a_doc_comment() {
        let source = "/**/\nconst x = 1;\n";
        let spans = HighlightOptions::default().highlight(source, Some(highlight_ts));
        let span = find_span(&spans, source, "/**/").expect("comment span");
        assert_eq!(span.kind, HighlightKind::Comment);
    }

    #[test]
    fn a_real_jsdoc_block_tag_inside_a_typescript_doc_comment_gets_its_own_tag_span() {
        let source = "/**\n * Adds two numbers.\n * @param left the first number\n * @returns the sum\n */\nfunction add(left: number, right: number): number {\n    return left + right;\n}\n";
        let spans = HighlightOptions::default().highlight(source, Some(highlight_ts));
        assert_eq!(
            kind_at(&spans, source, "@param"),
            HighlightKind::CommentDocTag
        );
        assert_eq!(
            kind_at(&spans, source, "@returns"),
            HighlightKind::CommentDocTag
        );
        // The prose right after a tag must still read as ordinary doc-comment text, not also get
        // swept into the tag's own span.
        assert_eq!(
            kind_at(&spans, source, "the first number"),
            HighlightKind::CommentDoc
        );
    }

    #[test]
    fn a_real_jsdoc_style_tag_inside_a_rust_doc_comment_still_gets_its_own_tag_span() {
        let source = "/// Adds one.\n///\n/// @param left the number to add to\nfn add(left: i32) -> i32 {\n    left + 1\n}\n";
        let spans = HighlightOptions::default().highlight(source, Some(highlight_rust));
        assert_eq!(
            kind_at(&spans, source, "@param"),
            HighlightKind::CommentDocTag
        );
    }

    // Real Python highlighting coverage - mirrors `highlight_rust`'s own test shape above.
    // Before this fix, none of `highlight_python`'s real, common-case behavior had any test
    // coverage at all.

    const SAMPLE_PYTHON: &str =
        "def add(left: int) -> int:\n    name = \"x\"\n    return left + 1\n";

    // ---------------------------------------------------------------------------------------
    // `tree-sitter-highlight` migration coverage.
    //
    // These assert on specific real tokens, and each one exists because an old-vs-new span diff
    // over real source files in this repository actually showed that token changing (or, for the
    // "still" cases, showed a way it could plausibly have changed and did not). They are the
    // executable form of that diff, not a generic "renders something" smoke test.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn every_real_grammar_config_compiles() {
        let grammars = Grammar::ALL;
        for grammar in grammars {
            build_highlight_config(grammar)
                .unwrap_or_else(|error| panic!("{}'s highlights query: {error}", grammar.name()));
        }

        // `highlight_config` keys its process-wide cache on `Grammar::name`, so two grammars
        // sharing a name would silently hand one grammar's configuration to the other - a
        // wrong-language parse rendering as plausible-looking garbage rather than as an error.
        // Nothing in the type system prevents that, so it is asserted here instead.
        let mut names: Vec<&str> = grammars.iter().map(|grammar| grammar.name()).collect();
        names.sort_unstable();
        let distinct = names.len();
        names.dedup();
        assert_eq!(names.len(), distinct, "every Grammar needs a distinct name");

        // And every one of those names must really resolve to a live configuration, or that
        // grammar silently renders as plain text.
        for grammar in grammars {
            assert!(
                highlight_config(grammar).is_some(),
                "{} has no live highlight configuration",
                grammar.name()
            );
        }
    }

    #[test]
    fn a_plain_rust_local_is_a_real_variable_not_unclassified_text() {
        let source = "fn f(input: u8) {\n    let child = compute(input);\n    use_it(child);\n}\n";
        let spans = highlight_rust(source);

        assert_eq!(
            kind_at(&spans, source, "child ="),
            HighlightKind::Variable,
            "the binding site of a local must be a real Variable"
        );
        assert_eq!(
            kind_at(&spans, source, "child)"),
            HighlightKind::Variable,
            "and so must every later use of it"
        );
        assert_eq!(
            kind_at(&spans, source, "f(input"),
            HighlightKind::FunctionDefinition
        );
        assert_eq!(kind_at(&spans, source, "compute("), HighlightKind::Function);
        assert_eq!(
            kind_at(&spans, source, "input:"),
            HighlightKind::VariableParameter,
            "a parameter must still beat the blanket rule"
        );
        assert_eq!(kind_at(&spans, source, "u8"), HighlightKind::TypeBuiltin);
        assert_eq!(kind_at(&spans, source, "let"), HighlightKind::Keyword);
    }

    #[test]
    fn a_rust_const_is_a_real_constant_not_a_type() {
        let source =
            "const MAX_SIZE: usize = 42;\nstatic LIMIT: u32 = 7;\nfn f() -> usize { MAX_SIZE }\n";
        let spans = highlight_rust(source);
        assert_eq!(
            kind_at(&spans, source, "MAX_SIZE:"),
            HighlightKind::Constant,
            "a const's declaration site must be a Constant, not a Type"
        );
        assert_eq!(
            kind_at(&spans, source, "MAX_SIZE }"),
            HighlightKind::Constant,
            "and so must every later use of it"
        );
        assert_eq!(kind_at(&spans, source, "LIMIT"), HighlightKind::Constant);
        assert_eq!(kind_at(&spans, source, "usize"), HighlightKind::TypeBuiltin);
    }

    #[test]
    fn the_rust_constant_heuristic_does_not_claim_types_or_attributes() {
        let source = "#[derive(Debug)]\nstruct Widget { id: u32 }\n";
        let spans = highlight_rust(source);
        assert_eq!(kind_at(&spans, source, "Widget {"), HighlightKind::Type);
        assert_eq!(kind_at(&spans, source, "derive"), HighlightKind::Attribute);
        assert_eq!(kind_at(&spans, source, "Debug"), HighlightKind::Attribute);
    }

    #[test]
    fn python_parameters_are_real_parameters_and_self_is_still_builtin() {
        let source = "class C:\n    def f(self, a, b: int, c=1, d: str = \"x\", *args, **kw):\n        return self\n";
        let spans = highlight_python(source);
        for needle in ["a,", "b:", "c=1", "d: str", "args", "kw)"] {
            assert_eq!(
                kind_at(&spans, source, needle),
                HighlightKind::VariableParameter,
                "{needle:?} is a parameter binding site"
            );
        }
        assert_eq!(
            kind_at(&spans, source, "self,"),
            HighlightKind::VariableBuiltin,
            "`self` stays a builtin, not an ordinary parameter"
        );
        assert_eq!(
            kind_at(&spans, source, "self\n"),
            HighlightKind::VariableBuiltin
        );
    }

    #[test]
    fn go_parameters_constants_and_literal_keys_are_really_classified() {
        let source = "package main\n\nconst MaxRetries = 3\n\nfunc scale(n int, factor float64) int {\n\treturn n\n}\n\nfunc build() User { return User{Name: \"x\"} }\n";
        let spans = highlight_go(source);
        assert_eq!(
            kind_at(&spans, source, "n int"),
            HighlightKind::VariableParameter
        );
        assert_eq!(
            kind_at(&spans, source, "factor"),
            HighlightKind::VariableParameter
        );
        assert_eq!(
            kind_at(&spans, source, "MaxRetries"),
            HighlightKind::Constant,
            "a Go const is a constant because the grammar says so, not because of its casing"
        );
        assert_eq!(
            kind_at(&spans, source, "Name:"),
            HighlightKind::Property,
            "a struct literal's key is a property, as it is in Rust and TypeScript"
        );
    }

    #[test]
    fn a_typescript_shorthand_property_is_a_real_variable_not_unclassified_text() {
        let source = "const { alpha, beta } = config;\nconst obj = { alpha, gamma: 1 };\nconst { MAX_N } = limits;\n";
        let spans = highlight_typescript(source, false);
        assert_eq!(
            kind_at(&spans, source, "alpha, beta"),
            HighlightKind::Variable
        );
        assert_eq!(kind_at(&spans, source, "beta }"), HighlightKind::Variable);
        assert_eq!(
            kind_at(&spans, source, "alpha, gamma"),
            HighlightKind::Variable
        );
        assert_eq!(kind_at(&spans, source, "gamma:"), HighlightKind::Property);
        // The `#not-match?` guard: an all-caps shorthand still reaches JavaScript's own earlier
        // `@constant` rule rather than being repainted a variable.
        assert_eq!(kind_at(&spans, source, "MAX_N }"), HighlightKind::Constant);
    }

    #[test]
    fn typescript_arrow_rest_and_destructured_parameters_are_real_parameters() {
        let source = "const f = items.map(x => x + 1);\nfunction pick(a: number, ...rest: string[]) {}\nfunction g({ lo, hi }: Range) {}\n";
        let spans = highlight_typescript(source, false);
        assert_eq!(
            kind_at(&spans, source, "x =>"),
            HighlightKind::VariableParameter,
            "an unparenthesized arrow parameter is still a parameter"
        );
        assert_eq!(
            kind_at(&spans, source, "rest:"),
            HighlightKind::VariableParameter
        );
        assert_eq!(
            kind_at(&spans, source, "lo,"),
            HighlightKind::VariableParameter
        );
        assert_eq!(
            kind_at(&spans, source, "hi }"),
            HighlightKind::VariableParameter
        );
        assert_eq!(
            kind_at(&spans, source, "a: number"),
            HighlightKind::VariableParameter
        );
    }

    #[test]
    fn an_attributes_own_identifiers_stay_attributes_not_variables() {
        let source = "#[cfg(all(test, unix))]\n#[derive(Debug)]\nstruct S;\n";
        let spans = highlight_rust(source);
        for needle in ["cfg", "all(", "test,", "unix", "derive", "Debug"] {
            assert_eq!(
                kind_at(&spans, source, needle),
                HighlightKind::Attribute,
                "{needle:?} is inside an attribute and must render as one"
            );
        }
    }

    #[test]
    fn a_macro_invocations_arguments_are_ordinary_code_not_attributes() {
        let source = "fn f() { let total = 1; assert_eq!(total, other(total)); }\n";
        let spans = highlight_rust(source);
        assert_eq!(
            kind_at(&spans, source, "total,"),
            HighlightKind::Variable,
            "a variable inside a macro argument list is still a variable, not an attribute"
        );
        // `other` reads as a `Variable` rather than a `Function`, and that is honest rather than a
        // gap: a macro's body parses as an opaque `token_tree`, so the grammar genuinely cannot
        // tell a call from any other identifier in there. Before the blanket rule it was
        // unclassified `Text`; a tint is strictly more informative than nothing, and crucially it
        // is *not* `Attribute`, which is what this test exists to rule out.
        assert_eq!(kind_at(&spans, source, "other("), HighlightKind::Variable);
    }

    #[test]
    fn a_definition_site_and_a_call_site_are_really_different_buckets_in_every_language() {
        let cases: [(&str, crate::language::HighlighterFn, &str); 5] = [
            (
                "rust",
                highlight_rust,
                "fn zeta() {}\nfn caller() { zeta(); }\n",
            ),
            (
                "python",
                highlight_python,
                "def zeta():\n    pass\n\nzeta()\n",
            ),
            ("typescript", highlight_ts, "function zeta() {}\nzeta();\n"),
            (
                "go",
                highlight_go,
                "package m\n\nfunc zeta() {}\n\nfunc caller() { zeta() }\n",
            ),
            (
                "c",
                highlight_c,
                "int zeta(void) { return 0; }\nint caller(void) { return zeta(); }\n",
            ),
        ];
        for (label, highlighter, source) in cases {
            let spans = highlighter(source);
            let kinds: Vec<HighlightKind> = spans
                .iter()
                .filter(|span| source[span.start..span.end] == *"zeta")
                .map(|span| span.kind)
                .collect();
            assert_eq!(
                kinds.len(),
                2,
                "{label}: premise - the definition and the call must both be classified spans, \
                 got {kinds:?}"
            );
            assert_eq!(
                kinds[0],
                HighlightKind::FunctionDefinition,
                "{label}: the declaration of `zeta` must be a definition site"
            );
            assert_eq!(
                kinds[1],
                HighlightKind::Function,
                "{label}: the call to `zeta` must stay a plain function"
            );
        }
    }

    #[test]
    fn a_c_prototype_is_not_a_definition_site_but_the_definition_below_it_is() {
        let source = "int zeta(int a);\nint zeta(int a) { return a; }\n";
        let spans = highlight_c(source);
        let kinds: Vec<HighlightKind> = spans
            .iter()
            .filter(|span| source[span.start..span.end] == *"zeta")
            .map(|span| span.kind)
            .collect();
        assert_eq!(
            kinds.len(),
            2,
            "the prototype and the definition, got {kinds:?}"
        );
        assert_eq!(
            kinds[0],
            HighlightKind::Function,
            "a prototype declares nothing here - it must not read as the definition site"
        );
        assert_eq!(kinds[1], HighlightKind::FunctionDefinition);
    }

    #[test]
    fn recognized_highlight_names_map_to_the_intended_buckets() {
        let bucket = |name: &str| {
            let index = HIGHLIGHT_NAMES
                .iter()
                .position(|candidate| *candidate == name)
                .unwrap_or_else(|| panic!("{name} missing from HIGHLIGHT_NAMES"));
            HIGHLIGHT_KINDS[index]
        };
        assert_eq!(bucket("keyword"), HighlightKind::Keyword);
        assert_eq!(bucket("function"), HighlightKind::Function);
        assert_eq!(bucket("function.method"), HighlightKind::FunctionMethod);
        assert_eq!(
            bucket("function.definition"),
            HighlightKind::FunctionDefinition
        );
        assert_eq!(bucket("type"), HighlightKind::Type);
        assert_eq!(bucket("type.builtin"), HighlightKind::TypeBuiltin);
        assert_eq!(bucket("constant"), HighlightKind::Constant);
        assert_eq!(bucket("constant.builtin"), HighlightKind::ConstantBuiltin);
        assert_eq!(bucket("string"), HighlightKind::String);
        assert_eq!(bucket("number"), HighlightKind::Number);
        assert_eq!(bucket("comment"), HighlightKind::Comment);
        assert_eq!(bucket("variable"), HighlightKind::Variable);
        assert_eq!(
            bucket("variable.parameter"),
            HighlightKind::VariableParameter
        );
        assert_eq!(bucket("property"), HighlightKind::Property);
        assert_eq!(bucket("operator"), HighlightKind::Operator);
        assert_eq!(
            bucket("punctuation.bracket"),
            HighlightKind::PunctuationBracket
        );
        assert_eq!(
            bucket("punctuation.delimiter"),
            HighlightKind::PunctuationDelimiter
        );
        assert_eq!(bucket("attribute"), HighlightKind::Attribute);
        assert_eq!(bucket("embedded"), HighlightKind::Embedded);
        // The real capture names verified directly against the grammars' own bundled query
        // files. `escape` is Rust's and Python's; `string.escape` is Markdown's own
        // `(backslash_escape) @string.escape` - both sides of the pair resolve to one bucket.
        // (`comment.doc` used to be asserted here too, until the coverage audit found no grammar
        // emits it and it was removed from HIGHLIGHT_NAMES entirely.)
        assert_eq!(bucket("escape"), HighlightKind::StringEscape);
        assert_eq!(bucket("string.escape"), HighlightKind::StringEscape);
        assert_eq!(bucket("comment.documentation"), HighlightKind::CommentDoc);
        assert_eq!(bucket("constructor"), HighlightKind::Constructor);
        assert_eq!(bucket("tag"), HighlightKind::Tag);
        assert_eq!(bucket("variable.builtin"), HighlightKind::VariableBuiltin);
    }

    #[test]
    fn rust_method_calls_and_macros_are_classified_into_their_own_real_buckets() {
        let source = "fn main() {\n    let x = value.clone();\n    println!(\"{x}\");\n}\n";
        let spans = highlight_rust(source);
        assert_eq!(
            kind_at(&spans, source, "clone"),
            HighlightKind::FunctionMethod
        );
        assert_eq!(
            kind_at(&spans, source, "println"),
            HighlightKind::FunctionMacro
        );
    }

    #[test]
    fn rust_mut_is_now_really_classified_as_a_keyword() {
        let source = "fn main() { let mut count = 0; }\n";
        let spans = highlight_rust(source);
        assert_eq!(kind_at(&spans, source, "mut "), HighlightKind::Keyword);
    }

    #[test]
    fn rust_lifetime_is_classified_as_label() {
        let source = "fn longest<'a>(x: &'a str, y: &'a str) -> &'a str { x }\n";
        let spans = highlight_rust(source);
        // Starting the search at `a`, not the leading `'` - the grammar's own `(lifetime
        // (identifier) @label)` rule only captures the identifier, not the apostrophe sigil
        // (which is its own, separately-classified token).
        assert_eq!(kind_at(&spans, source, "a>"), HighlightKind::Label);
    }

    #[test]
    fn self_and_this_share_the_variable_builtin_bucket_across_languages() {
        let rust = "impl T { fn get(&self) -> u8 { self.value } }\n";
        assert_eq!(
            kind_at(&highlight_rust(rust), rust, "self.value"),
            HighlightKind::VariableBuiltin
        );
        let python = "class T:\n    def get(self):\n        return self.value\n";
        assert_eq!(
            kind_at(&highlight_python(python), python, "self.value"),
            HighlightKind::VariableBuiltin
        );
        let typescript = "class T { get() { return this.value; } }\n";
        assert_eq!(
            kind_at(&highlight_typescript(typescript, false), typescript, "this"),
            HighlightKind::VariableBuiltin
        );
    }

    #[test]
    fn python_word_operators_are_keywords_but_symbolic_ones_are_the_real_operator_bucket() {
        let source = "if a is not b and c in d or not e:\n    total = a + b\n";
        let spans = highlight_python(source);
        for word in ["is not", "and ", "in ", "or ", "not e"] {
            assert_eq!(
                kind_at(&spans, source, word),
                HighlightKind::Keyword,
                "python word operator {word:?}"
            );
        }
        assert_eq!(kind_at(&spans, source, "+ b"), HighlightKind::Operator);
    }

    #[test]
    fn python_compound_type_annotations_are_classified_as_types() {
        let source = "def f(a: dict[str, int], b: pathlib.Path) -> list[int]:\n    pass\n";
        let spans = highlight_python(source);
        assert_eq!(kind_at(&spans, source, "dict[str"), HighlightKind::Type);
        assert_eq!(kind_at(&spans, source, "pathlib.Path"), HighlightKind::Type);
        assert_eq!(kind_at(&spans, source, "list[int]"), HighlightKind::Type);
    }

    #[test]
    fn grammar_indices_match_their_position_in_all() {
        for (position, grammar) in Grammar::ALL.into_iter().enumerate() {
            assert_eq!(grammar.index(), position, "{}", grammar.name());
        }
        assert_eq!(Grammar::COUNT, HIGHLIGHT_CONFIGS.len());
    }

    #[test]
    fn python_class_names_are_classified_as_types_whatever_their_casing() {
        let source = "class Widget:\n    pass\nclass _Pickler:\n    pass\nclass socket:\n    pass\nclass FTP:\n    pass\n";
        let spans = highlight_python(source);
        for name in ["Widget", "_Pickler", "socket", "FTP"] {
            assert_eq!(
                kind_at(&spans, source, name),
                HighlightKind::Type,
                "class {name}"
            );
        }
    }

    #[test]
    fn python_method_calls_match_rust_and_typescript() {
        let source = "value = obj.method()\nother = cls(name)\n";
        let spans = highlight_python(source);
        assert_eq!(
            kind_at(&spans, source, "method"),
            HighlightKind::FunctionMethod,
            "a real `obj.method()` call is a method call, its own dedicated bucket since GitHub \
             issue #31"
        );
        assert_eq!(
            kind_at(&spans, source, "cls("),
            HighlightKind::Function,
            "a real `cls(...)` construction is a plain call, not the `cls` self-reference"
        );
        let reference = "def f(cls):\n    return cls\n";
        assert_eq!(
            kind_at(&highlight_python(reference), reference, "cls\n"),
            HighlightKind::VariableBuiltin
        );
    }

    // GitHub issue #32: the five new grammars. Each test parses a real, minimal snippet and
    // checks the exact real capture this module's own docs above claim for it - not just "some
    // colour or other".

    const SAMPLE_TOML: &str = "name = \"jerry\"\ncount = 3\nenabled = true\nbuilt = 1979-05-27\n";

    const SAMPLE_GO: &str =
        "package main\n\nfunc add(x int) int {\n\treturn len(fmt.Sprint(x))\n}\n";

    const SAMPLE_JSON: &str = "{\n  \"name\": \"jerry\",\n  \"count\": 3\n}\n";

    const SAMPLE_YAML: &str = "anchor: &base\n  enabled: true\nalias: *base\ncount: 3\n";

    const SAMPLE_C: &str =
        "int add(int x) {\n  int total = 0;\n  goto done;\n done:\n  return total;\n}\n";

    const SAMPLE_MARKDOWN: &str = "# Title\n\nSome **bold** and *italic* text with `inline code` and a [link](https://example.com).\n\n```rust\nfn main() {}\n```\n";

    #[test]
    fn inline_content_is_never_left_as_a_single_flat_text_region() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert!(
            spans.iter().any(|span| span.kind != HighlightKind::Text),
            "a real markdown document must produce at least one non-Text span - if this fails, \
             the inline grammar injection isn't firing at all"
        );
    }

    /// Fenced code content must never inherit the enclosing fence's own `@text.literal` `String`
    /// colour. GitHub issue #104 achieved that by registering `"none"`; GitHub issue #154 achieves
    /// it by cancelling `@text.literal` at source instead - see
    /// [`MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT`] for why the second way is the only one compatible
    /// with a real injected layer. Either way this assertion is the same one, and it is the guard
    /// that the swap did not quietly lose it.
    #[test]
    fn markdown_fenced_code_block_content_is_not_colored_like_a_string() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert_ne!(
            kind_at(&spans, SAMPLE_MARKDOWN, "fn main"),
            HighlightKind::String,
            "fenced code content must not inherit the fence's own text.literal colour"
        );
    }

    // ---------------------------------------------------------------------------------------
    // GitHub issue #154: HTML and CSS, as real file types and as real markdown fence languages.
    // ---------------------------------------------------------------------------------------

    const SAMPLE_HTML: &str = "<!DOCTYPE html>\n<!-- note -->\n<div class=\"card\">\n  <p>Hi</p>\n</div>\n<style>\n.card { color: #ff0000; }\n</style>\n<script>\nconst total = 1;\n</script>\n";

    #[test]
    fn html_captures_land_in_the_expected_existing_buckets() {
        let spans = highlight_html(SAMPLE_HTML);
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "<div"),
            HighlightKind::PunctuationBracket
        );
        assert_eq!(kind_at(&spans, SAMPLE_HTML, "div"), HighlightKind::Tag);
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "class"),
            HighlightKind::Attribute
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "card\""),
            HighlightKind::String
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "<!-- note -->"),
            HighlightKind::Comment
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "<!DOCTYPE html"),
            HighlightKind::Constant,
            "the doctype declaration is `(doctype) @constant`, not a tag"
        );
    }

    #[test]
    fn html_mismatched_closing_tag_is_classified_as_tag_error() {
        let source = "<div></spam>\n";
        let spans = highlight_html(source);
        assert_eq!(kind_at(&spans, source, "div"), HighlightKind::Tag);
        assert_eq!(kind_at(&spans, source, "spam"), HighlightKind::TagError);
    }

    #[test]
    fn html_style_and_script_bodies_are_injected_into_real_css_and_javascript() {
        let spans = highlight_html(SAMPLE_HTML);
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "color"),
            HighlightKind::Property,
            "a CSS property name inside <style>"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "#ff0000"),
            HighlightKind::PunctuationDelimiter,
            "the `#` of a CSS colour literal is CSS punctuation - only the CSS grammar has any \
             rule here at all"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "ff0000"),
            HighlightKind::StringSpecial,
            "the colour literal's own digits"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_HTML, "const"),
            HighlightKind::Keyword,
            "a JavaScript keyword inside <script>"
        );
        assert_eq!(kind_at(&spans, SAMPLE_HTML, "1;"), HighlightKind::Number);
    }

    const SAMPLE_CSS: &str =
        "/* note */\n@media screen {\n  .card > p#main { color: #ff0000; margin: 4px; }\n}\n";

    #[test]
    fn css_captures_land_in_the_expected_existing_buckets() {
        let spans = highlight_css(SAMPLE_CSS);
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "/* note */"),
            HighlightKind::Comment
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "@media"),
            HighlightKind::Keyword
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "card >"),
            HighlightKind::Property,
            "a class selector's name is `(class_name) @property`"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "> p"),
            HighlightKind::Operator,
            "the child combinator"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "p#main"),
            HighlightKind::Tag,
            "a bare element selector is `(tag_name) @tag`, the same bucket JSX element names use"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "margin"),
            HighlightKind::Property
        );
        assert_eq!(kind_at(&spans, SAMPLE_CSS, "4px"), HighlightKind::Number);
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "px;"),
            HighlightKind::Type,
            "`(unit) @type` - a real capture in `tree-sitter-css`'s own bundled query"
        );
    }

    const SAMPLE_MARKDOWN_FENCES: &str = "# Fences\n\n```html\n<div class=\"card\">hi</div>\n```\n\n```css\n.card { color: red; }\n```\n\n```rust\nfn main() {}\n```\n\n```zig\nconst x = 1;\n```\n\n```\nplain fence\n```\n";

    /// GitHub issue #154's own headline ask - "including in the markdown files". A tagged
    /// fence's *content* really is reparsed by that tag's own grammar, and each of these buckets
    /// is one no markdown query has any rule capable of producing.
    ///
    /// Three languages in one table because that is the point: the same injection query resolves
    /// all of them, with no per-language code anywhere in [`MARKDOWN_INJECTION_QUERY`] or
    /// [`Grammar::for_injection_name`]. The ` ```rust ` rows also cover the fence's very first
    /// token, which is exactly the one a parent highlight left open over the injected range
    /// would silently steal.
    #[test]
    fn a_tagged_markdown_fence_really_highlights_its_content_with_that_languages_grammar() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        let cases: &[(&str, &str, HighlightKind)] = &[
            ("html tag name", "div class", HighlightKind::Tag),
            ("html attribute", "class=", HighlightKind::Attribute),
            ("html attribute value", "card\">", HighlightKind::String),
            ("css class selector", "card { ", HighlightKind::Property),
            ("css declaration", "color: red", HighlightKind::Property),
            ("rust keyword", "fn main", HighlightKind::Keyword),
            ("rust fn name", "main()", HighlightKind::FunctionDefinition),
        ];
        for (label, token, expected) in cases {
            assert_eq!(
                kind_at(&spans, SAMPLE_MARKDOWN_FENCES, token),
                *expected,
                "{label}"
            );
        }
    }

    #[test]
    fn an_unknown_or_absent_fence_language_falls_back_to_plain_text() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "const x = 1;"),
            HighlightKind::Text,
            "```zig - a real fence tag with no grammar here"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "plain fence"),
            HighlightKind::Text,
            "a fence with no info string at all"
        );
    }

    #[test]
    fn a_fence_info_string_is_still_colored_like_a_literal() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "html\n"),
            HighlightKind::String
        );
    }

    #[test]
    fn an_indented_code_block_is_still_colored_like_a_literal() {
        let source = "Text.\n\n    indented code\n";
        let spans = highlight_markdown(source);
        assert_eq!(
            kind_at(&spans, source, "indented code"),
            HighlightKind::String
        );
    }

    #[test]
    fn a_raw_html_block_in_markdown_is_injected_into_the_html_grammar() {
        let source = "Before.\n\n<div class=\"card\">block</div>\n\nAfter.\n";
        let spans = highlight_markdown(source);
        assert_eq!(kind_at(&spans, source, "div class"), HighlightKind::Tag);
        assert_eq!(kind_at(&spans, source, "class="), HighlightKind::Attribute);
    }

    #[test]
    fn a_raw_html_tag_inside_a_markdown_paragraph_is_injected_into_the_html_grammar() {
        let source = "Inline <span class=\"i\">tag</span> here.\n";
        let spans = highlight_markdown(source);
        assert_eq!(kind_at(&spans, source, "span class"), HighlightKind::Tag);
        assert_eq!(kind_at(&spans, source, "class="), HighlightKind::Attribute);
    }

    #[test]
    fn yaml_frontmatter_in_markdown_is_injected_into_the_yaml_grammar() {
        let source = "---\nkey: true\n---\n\n# Title\n";
        let spans = highlight_markdown(source);
        assert_eq!(kind_at(&spans, source, "key"), HighlightKind::Property);
        assert_eq!(
            kind_at(&spans, source, "true"),
            HighlightKind::ConstantBuiltin
        );
    }

    #[test]
    fn a_fence_nested_in_a_list_or_block_quote_still_highlights_its_real_language() {
        let list = "- item\n\n  ```rust\n  fn main() {}\n  ```\n";
        let list_spans = highlight_markdown(list);
        assert_eq!(
            kind_at(&list_spans, list, "fn main"),
            HighlightKind::Keyword
        );
        assert_eq!(
            kind_at(&list_spans, list, "main()"),
            HighlightKind::FunctionDefinition
        );

        let quote = "> quote\n>\n> ```rust\n> fn main() {}\n> ```\n";
        let quote_spans = highlight_markdown(quote);
        assert_eq!(
            kind_at(&quote_spans, quote, "fn main"),
            HighlightKind::Keyword
        );
        assert_eq!(
            kind_at(&quote_spans, quote, "> fn"),
            HighlightKind::PunctuationSpecial,
            "the block quote marker on the fence's own content line keeps its markdown colour \
             (its own dedicated bucket since GitHub issue #183 - previously `Operator`)"
        );
    }

    #[test]
    fn every_grammars_own_name_resolves_back_to_that_grammar() {
        for grammar in Grammar::ALL {
            assert_eq!(
                Grammar::for_injection_name(grammar.name()),
                Some(grammar),
                "{}",
                grammar.name()
            );
        }
    }

    #[test]
    fn every_registry_extension_with_a_highlighter_is_reachable_as_a_fence_language() {
        for entry in crate::language::EXTENSIONS
            .iter()
            .filter(|entry| entry.highlighter.is_some())
        {
            let grammar = Grammar::for_injection_name(entry.extension).unwrap_or_else(|| {
                panic!(
                    "a fence tagged `{}` must resolve to a real grammar - the registry says that \
                     extension has a highlighter",
                    entry.extension
                )
            });
            assert_eq!(
                Grammar::for_extension(entry.extension),
                Some(grammar),
                "{}",
                entry.extension
            );
        }
    }

    #[test]
    fn grammar_for_extension_claims_exactly_the_registrys_own_highlighted_extensions() {
        let mut claimed: Vec<&str> = crate::language::EXTENSIONS
            .iter()
            .map(|entry| entry.extension)
            .filter(|extension| Grammar::for_extension(extension).is_some())
            .collect();
        claimed.sort_unstable();
        let mut with_highlighter: Vec<&str> = crate::language::EXTENSIONS
            .iter()
            .filter(|entry| entry.highlighter.is_some())
            .map(|entry| entry.extension)
            .collect();
        with_highlighter.sort_unstable();
        assert_eq!(claimed, with_highlighter);
    }

    #[test]
    fn an_unknown_injection_name_resolves_to_no_grammar() {
        assert_eq!(Grammar::for_injection_name("zig"), None);
        assert_eq!(Grammar::for_injection_name("latex"), None);
        assert_eq!(Grammar::for_injection_name(""), None);
    }

    #[test]
    fn typescript_highlighting_covers_the_javascript_half_of_the_composed_query() {
        let source = "// note\nasync function load() {\n  const url = \"/api\";\n  return fetch(url, 3);\n}\n";
        let spans = highlight_typescript(source, false);
        assert_eq!(kind_at(&spans, source, "// note"), HighlightKind::Comment);
        assert_eq!(kind_at(&spans, source, "async"), HighlightKind::Keyword);
        assert_eq!(kind_at(&spans, source, "\"/api\""), HighlightKind::String);
        assert_eq!(kind_at(&spans, source, "3"), HighlightKind::Number);
        assert_eq!(kind_at(&spans, source, "fetch"), HighlightKind::Function);
    }

    #[test]
    fn typescript_supplement_repairs_capitalised_functions_and_void() {
        let source = "function Badge(x: string): void {}\nconst s = String(x);\nconst w: Widget = new Widget();\n";
        let spans = highlight_typescript(source, false);
        assert_eq!(
            kind_at(&spans, source, "Badge"),
            HighlightKind::FunctionDefinition,
            "a capitalised function declaration is still a function, not a type"
        );
        assert_eq!(
            kind_at(&spans, source, "String("),
            HighlightKind::Function,
            "a capitalised *call* is still a call, not a type - and, since the definition-site \
             supplements landed, still a plain `Function` rather than the `FunctionDefinition` \
             the declaration above it gets"
        );
        assert_eq!(
            kind_at(&spans, source, "Widget = "),
            HighlightKind::Type,
            "a capitalised identifier that is neither declared nor called stays a type"
        );
        assert_eq!(
            kind_at(&spans, source, "void"),
            HighlightKind::TypeBuiltin,
            "`void` must classify like every other predefined_type keyword - its own dedicated \
             bucket since GitHub issue #31, not the plain `Type` a user-defined type name gets"
        );
        assert_eq!(
            kind_at(&spans, source, "string"),
            HighlightKind::TypeBuiltin
        );
    }

    #[test]
    fn tsx_jsx_element_names_are_classified_as_tag_or_type() {
        let source = "const view = <div className=\"row\"><Badge label={name} /></div>;\n";
        let spans = highlight_tsx(source);
        assert_eq!(
            kind_at(&spans, source, "div"),
            HighlightKind::Tag,
            "a lowercase JSX element name arrives as @tag"
        );
        assert_eq!(
            kind_at(&spans, source, "Badge"),
            HighlightKind::Type,
            "a capitalised JSX component name doesn't match @tag's own lowercase-only regex, so \
             it falls through to TypeScript's plain capitalised-identifier @type heuristic instead"
        );
        assert_eq!(kind_at(&spans, source, "\"row\""), HighlightKind::String);
    }

    #[test]
    fn interpolated_code_inside_strings_is_classified_as_code() {
        let typescript = "const msg = `n=${count.toFixed(1)}`;\n";
        let ts_spans = highlight_typescript(typescript, false);
        assert_eq!(
            kind_at(&ts_spans, typescript, "n=$"),
            HighlightKind::String,
            "the literal text of the template string is still a string"
        );
        assert_eq!(
            kind_at(&ts_spans, typescript, "toFixed"),
            HighlightKind::FunctionMethod,
            "a real `count.toFixed(...)` call is a method call, its own dedicated bucket since \
             GitHub issue #31"
        );

        let python = "msg = f\"n={value if value else other}\"\n";
        let py_spans = highlight_python(python);
        assert_eq!(kind_at(&py_spans, python, "n="), HighlightKind::String);
        assert_eq!(
            kind_at(&py_spans, python, "if value"),
            HighlightKind::Keyword
        );
    }

    #[test]
    fn produced_spans_tile_the_whole_source_exactly_once() {
        for (label, spans, len) in [
            ("rust", highlight_rust(SAMPLE_RUST), SAMPLE_RUST.len()),
            (
                "python",
                highlight_python(SAMPLE_PYTHON),
                SAMPLE_PYTHON.len(),
            ),
            (
                "tsx",
                highlight_tsx(SAMPLE_TYPESCRIPT),
                SAMPLE_TYPESCRIPT.len(),
            ),
        ] {
            let mut cursor = 0usize;
            for span in &spans {
                assert!(span.start < span.end, "{label}: empty span {span:?}");
                assert_eq!(span.start, cursor, "{label}: gap or overlap at {span:?}");
                cursor = span.end;
            }
            assert_eq!(cursor, len, "{label}: spans must cover the whole source");
        }
    }

    #[test]
    fn escapes_inside_a_string_are_their_own_real_string_escape_run() {
        let source = "fn main() { let s = \"a\\nb\\tc\"; }\n";
        let spans = highlight_rust(source);

        let newline_escape = find_span(&spans, source, "\\n").expect("\\n escape span");
        assert_eq!(newline_escape.kind, HighlightKind::StringEscape);
        let tab_escape = find_span(&spans, source, "\\t").expect("\\t escape span");
        assert_eq!(tab_escape.kind, HighlightKind::StringEscape);

        // The surrounding string content (opening quote through `a`, `b` between the two
        // escapes, `c` through the closing quote) stays real `String` - not `StringEscape`, and
        // not, critically, a `Text`-coloured hole where the escapes used to invisibly merge it.
        let literal_start = source.find('"').expect("string start");
        let literal_end = source.find("c\"").expect("string end") + 2;
        for offset in literal_start..literal_end {
            let kind = spans
                .iter()
                .find(|span| span.start <= offset && offset < span.end)
                .map_or(HighlightKind::Text, |span| span.kind);
            assert!(
                kind == HighlightKind::String || kind == HighlightKind::StringEscape,
                "byte {offset} of the string literal classified as {kind:?}, not String/\
                 StringEscape"
            );
        }
    }

    // Coverage for finding 5's fix: `highlighter_for_extension` reads from the real registry,
    // not a second, independent table.

    #[test]
    fn highlighter_for_extension_reads_from_the_real_language_registry() {
        assert!(highlighter_for_extension(Some("rs")).is_some());
        assert!(highlighter_for_extension(Some("ts")).is_some());
        assert!(highlighter_for_extension(Some("tsx")).is_some());
        assert!(highlighter_for_extension(Some("js")).is_some());
        assert!(highlighter_for_extension(Some("jsx")).is_some());
        assert!(highlighter_for_extension(Some("py")).is_some());
        assert!(highlighter_for_extension(Some("toml")).is_some());
        assert!(highlighter_for_extension(Some("go")).is_some());
        assert!(highlighter_for_extension(Some("json")).is_some());
        assert!(highlighter_for_extension(Some("yaml")).is_some());
        assert!(highlighter_for_extension(Some("yml")).is_some());
        assert!(highlighter_for_extension(Some("c")).is_some());
        assert!(highlighter_for_extension(Some("h")).is_some());
        assert!(highlighter_for_extension(Some("md")).is_some());
        assert!(highlighter_for_extension(Some("sql")).is_none());
        assert!(highlighter_for_extension(Some("vue")).is_none());
        assert!(highlighter_for_extension(None).is_none());
    }

    #[test]
    fn load_file_highlights_a_real_typescript_file_via_the_registry_dispatch() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("sample.ts");
        fs::write(
            &path,
            "function add(x: number): number {\n    return x;\n}\n",
        )
        .expect("write");

        let parsed = load_file(&path).expect("load_file");
        assert_eq!(parsed.language, "TypeScript");
        let has_keyword_run = parsed
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .any(|(_, kind)| *kind == HighlightKind::Keyword);
        assert!(
            has_keyword_run,
            "load_file should dispatch through the registry to a real TypeScript highlighter"
        );
    }

    #[test]
    fn load_file_highlights_a_real_python_file_via_the_registry_dispatch() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("sample.py");
        fs::write(&path, "def add(x):\n    return x\n").expect("write");

        let parsed = load_file(&path).expect("load_file");
        assert_eq!(parsed.language, "Python");
        let has_keyword_run = parsed
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .any(|(_, kind)| *kind == HighlightKind::Keyword);
        assert!(
            has_keyword_run,
            "load_file should dispatch through the registry to a real Python highlighter"
        );
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

    #[test]
    fn load_file_detects_a_real_non_utf8_file_as_lossily_decoded() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("latin1.txt");
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
        for rendered in &parsed.lines {
            assert!(rendered.text.is_empty() || rendered.text == "let value = 1;");
        }
    }

    // `highlight_block` coverage (Revision R9a's Diff/Merge highlighting helper) - proves real,
    // non-flat classification on realistic diff-hunk-/merge-hunk-shaped multi-line source, for
    // two different real languages, not just "doesn't panic".

    #[test]
    fn highlight_block_on_a_real_rust_diff_hunk_produces_real_non_flat_color_runs() {
        // Shaped like a real diff hunk's interleaved lines, lifted from this very crate's own
        // `highlighter_for_extension` - not a synthetic one-liner.
        let lines = [
            "pub fn highlighter_for_extension(",
            "    extension: Option<&str>,",
            ") -> Option<crate::language::HighlighterFn> {",
            "    crate::language::entry_for_extension(extension)?.highlighter",
            "}",
        ];
        let rendered = highlight_block(
            lines.iter().copied(),
            Some("rs"),
            HighlightOptions::default(),
        );
        let kinds: HashSet<HighlightKind> = rendered
            .iter()
            .flat_map(|line| &line.runs)
            .map(|(_, kind)| *kind)
            .collect();
        assert!(
            kinds.len() > 1,
            "a real multi-line Rust block must produce more than one distinct HighlightKind, \
             got {kinds:?}"
        );
        assert!(
            kinds.contains(&HighlightKind::Keyword),
            "pub/fn should be classified as keywords"
        );
        let has_fn_name = rendered
            .iter()
            .flat_map(|line| &line.runs)
            .any(|(text, kind)| {
                text.as_ref() == "highlighter_for_extension"
                    && *kind == HighlightKind::FunctionDefinition
            });
        assert!(
            has_fn_name,
            "the real fn name should be classified as a function"
        );
    }

    #[test]
    fn highlight_block_on_a_real_python_merge_hunk_produces_real_non_flat_color_runs() {
        let lines = [
            "def resolve(choice):",
            "    if choice == \"left\":",
            "        return ours",
            "    return theirs",
        ];
        let rendered = highlight_block(
            lines.iter().copied(),
            Some("py"),
            HighlightOptions::default(),
        );
        let kinds: HashSet<HighlightKind> = rendered
            .iter()
            .flat_map(|line| &line.runs)
            .map(|(_, kind)| *kind)
            .collect();
        assert!(
            kinds.len() > 1,
            "a real multi-line Python block must produce more than one distinct HighlightKind, \
             got {kinds:?}"
        );
        let has_def_keyword = rendered
            .iter()
            .flat_map(|line| &line.runs)
            .any(|(text, kind)| text.as_ref() == "def" && *kind == HighlightKind::Keyword);
        assert!(has_def_keyword, "def should be classified as a keyword");
        let has_string_literal = rendered
            .iter()
            .flat_map(|line| &line.runs)
            .any(|(text, kind)| text.as_ref() == "\"left\"" && *kind == HighlightKind::String);
        assert!(
            has_string_literal,
            "the real string literal should be classified as String"
        );
    }

    #[test]
    fn highlight_block_on_an_unregistered_extension_is_all_plain_text() {
        let rendered = highlight_block(
            ["-- a comment", "SELECT * FROM t;"],
            Some("sql"),
            HighlightOptions::default(),
        );
        let kinds: HashSet<HighlightKind> = rendered
            .iter()
            .flat_map(|line| &line.runs)
            .map(|(_, kind)| *kind)
            .collect();
        assert_eq!(
            kinds,
            HashSet::from([HighlightKind::Text]),
            "an extension with no wired grammar must render as plain text, matching load_file's \
             own 'unlisted extensions render as plain monospace text' precedent"
        );
    }

    #[test]
    fn highlight_block_on_zero_input_lines_returns_zero_rendered_lines() {
        let rendered = highlight_block(std::iter::empty(), Some("rs"), HighlightOptions::default());
        assert!(
            rendered.is_empty(),
            "zero real input lines must produce zero RenderedLines, not build_lines' one-empty-\
             line-for-an-empty-file convention: got {rendered:?}"
        );
    }

    #[test]
    fn highlight_block_on_one_real_blank_line_still_returns_one_rendered_line() {
        let rendered = highlight_block([""], Some("rs"), HighlightOptions::default());
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].text, "");
    }

    // ---------------------------------------------------------------------------------------
    // GitHub issue #168: bracket-pair colourization.
    //
    // The pure matcher first, driven by hand-built span lists - no grammar, no parse, no render.
    // A `HighlightSpan` list is exactly what `fold_highlight_events` hands `colorize_bracket_pairs`
    // in production, so building one directly tests the real contract rather than a stand-in.
    // The real-grammar and real-`RenderedLine` tests come after these.
    // ---------------------------------------------------------------------------------------

    /// Classifies every byte of `source` that is one of `bracket_chars` as
    /// [`HighlightKind::PunctuationBracket`] and everything else as [`HighlightKind::Text`],
    /// coalescing adjacent same-kind bytes exactly the way `fold_highlight_events` really does -
    /// so `"{}"` arrives as one two-character span, which is the case that forced
    /// `colorize_bracket_pairs` to be able to split a span at all.
    fn bracket_spans(source: &str, bracket_chars: &str) -> Vec<HighlightSpan> {
        let mut spans: Vec<HighlightSpan> = Vec::new();
        for (offset, ch) in source.char_indices() {
            let kind = if bracket_chars.contains(ch) {
                HighlightKind::PunctuationBracket
            } else {
                HighlightKind::Text
            };
            push_coalesced(
                &mut spans,
                HighlightSpan {
                    start: offset,
                    end: offset + ch.len_utf8(),
                    kind,
                    scope: OUTER_SCOPE,
                },
            );
        }
        spans
    }

    /// The bucket each byte of `source` ends up in after `colorize_bracket_pairs`, rendered as one
    /// compact string for easy whole-shape assertions: `.` for anything that isn't a bracket,
    /// `-` for a bracket left plain (unmatched, or an untracked `<`/`>`), and `1`..`6` for the ring
    /// colour a matched bracket really got.
    fn depth_map(source: &str, bracket_chars: &str) -> String {
        let spans = colorize_bracket_pairs(source, bracket_spans(source, bracket_chars));
        let mut out = String::new();
        for (offset, _) in source.char_indices() {
            let span = spans
                .iter()
                .find(|span| span.start <= offset && offset < span.end)
                .expect("colorize_bracket_pairs must stay gapless over the whole source");
            out.push(match span.kind {
                HighlightKind::PunctuationBracket => '-',
                HighlightKind::Text => '.',
                kind => match HighlightKind::BRACKET_DEPTH_RING
                    .iter()
                    .position(|ring| *ring == kind)
                {
                    Some(index) => char::from_digit(index as u32 + 1, 10).expect("1..=6"),
                    None => panic!("unexpected bucket {kind:?}"),
                },
            });
        }
        out
    }

    #[test]
    fn mixed_bracket_shapes_nest_and_each_pair_shares_one_ring_colour() {
        assert_eq!(depth_map("foo([{}])", "([{}])"), "...123321");
    }

    #[test]
    fn sibling_pairs_at_the_same_level_all_get_the_same_ring_colour() {
        assert_eq!(depth_map("()()()", "()"), "111111");
        // Both inner pairs are siblings *inside* the outer one, so both are at depth 1 - a
        // sibling never advances the ring, only nesting does.
        assert_eq!(depth_map("(()())", "()"), "122221");
    }

    #[test]
    fn the_seventh_nesting_level_wraps_back_to_the_first_ring_colour() {
        // Eight levels: depths 0..7 colour as 1,2,3,4,5,6 and then wrap to 1,2 - and every
        // closer comes back around with its own opener.
        assert_eq!(depth_map("(((((((())))))))", "()"), "1234561221654321");
    }

    #[test]
    fn a_stray_closer_is_left_plain_and_does_not_consume_the_open_bracket() {
        assert_eq!(depth_map("(a)b)", "()"), "1.1.-");
        assert_eq!(depth_map("()) ()", "()"), "11-.11");
        assert_eq!(depth_map("( ) )", "()"), "1.1.-");
        assert_eq!(depth_map("[ ) ]", "[)]"), "1.-.1");
    }

    #[test]
    fn an_opener_that_never_closes_is_left_plain() {
        assert_eq!(depth_map("fn f() {", "(){}"), "....11.-");
        assert_eq!(depth_map("{[", "{["), "--");
    }

    #[test]
    fn mismatched_shapes_do_not_pair_up() {
        assert_eq!(depth_map("([)]", "([)]"), "-1-1");
    }

    #[test]
    fn an_unmatched_opener_no_longer_shifts_the_depth_of_real_pairs_that_follow_it() {
        assert_eq!(depth_map("( (x) (y(z))", "()"), "-.1.1.1.2.21");
    }

    #[test]
    fn an_earlier_unmatched_opener_does_not_shift_a_later_well_formed_functions_own_depth() {
        let with_earlier_unmatched_openers =
            "fn a() { let x = ([)]; }\nfn b() { let y = ( ;\n}\nfn c() { ok(); }\n";
        let isolated = "fn c() { ok(); }\n";
        let bracket_chars = "(){}[]";

        let combined_map = depth_map(with_earlier_unmatched_openers, bracket_chars);
        let isolated_map = depth_map(isolated, bracket_chars);
        let combined_fn_c_tail = &combined_map[combined_map.len() - isolated_map.len()..];

        assert_eq!(
            combined_fn_c_tail, isolated_map,
            "fn c's own well-formed brackets must colour identically whether or not two \
             earlier, permanently-unmatched openers precede it in the same file - a real \
             difference here means the earlier unmatched brackets are still leaking into depth, \
             GitHub issue #182's own bug (combined tail: {combined_fn_c_tail:?}, isolated: \
             {isolated_map:?})"
        );
        // Pinned to the exact real values, not just "they match": `fn c`'s own body brace and
        // its nested `ok()` call must differ by one real ring step from each other, at the
        // file's own natural depth 0/1 - not the pre-fix bug's 4/5.
        assert_eq!(isolated_map, "....11.1...22..1.");
    }

    #[test]
    fn thoroughly_unbalanced_input_degrades_to_plain_brackets_without_panicking() {
        assert_eq!(depth_map(")))", "()"), "---");
        assert_eq!(depth_map("}{", "{}"), "--");
        assert_eq!(depth_map(")}]([{", "()[]{}"), "------");
    }

    #[test]
    fn brackets_inside_strings_and_comments_are_never_coloured() {
        let source = r#"f("(", /* ) */)"#;
        let spans = vec![
            HighlightSpan {
                start: 0,
                end: 1,
                kind: HighlightKind::Function,
                scope: OUTER_SCOPE,
            },
            HighlightSpan {
                start: 1,
                end: 2,
                kind: HighlightKind::PunctuationBracket,
                scope: OUTER_SCOPE,
            },
            HighlightSpan {
                start: 2,
                end: 5,
                kind: HighlightKind::String,
                scope: OUTER_SCOPE,
            },
            HighlightSpan {
                start: 5,
                end: 7,
                kind: HighlightKind::PunctuationDelimiter,
                scope: OUTER_SCOPE,
            },
            HighlightSpan {
                start: 7,
                end: 14,
                kind: HighlightKind::Comment,
                scope: OUTER_SCOPE,
            },
            HighlightSpan {
                start: 14,
                end: 15,
                kind: HighlightKind::PunctuationBracket,
                scope: OUTER_SCOPE,
            },
        ];
        let out = colorize_bracket_pairs(source, spans);
        let kind_of = |byte: usize| {
            out.iter()
                .find(|span| span.start <= byte && byte < span.end)
                .expect("gapless")
                .kind
        };
        assert_eq!(
            kind_of(1),
            HighlightKind::Bracket1,
            "the real opening paren must be a matched depth-0 pair"
        );
        assert_eq!(
            kind_of(14),
            HighlightKind::Bracket1,
            "its partner is the final paren, not the one inside the string"
        );
        assert_eq!(
            kind_of(3),
            HighlightKind::String,
            "the `(` inside the string literal must stay part of the string"
        );
        assert_eq!(
            kind_of(10),
            HighlightKind::Comment,
            "the `)` inside the comment must stay part of the comment"
        );
    }

    #[test]
    fn brackets_never_pair_across_two_separate_markdown_fences() {
        let source = "```rust\nfn a() { // no closer\n```\n\nprose\n\n```rust\n} fn b() {}\n```\n";
        let spans = ring_spans(source, highlight_markdown);

        let unclosed_brace = nth_offset(source, '{', 0);
        let stray_closer = nth_offset(source, '}', 0);
        assert_eq!(
            kind_at_byte(&spans, unclosed_brace),
            HighlightKind::PunctuationBracket,
            "the first fence's `{{` has no partner *inside its own fence* and must stay plain"
        );
        assert_eq!(
            kind_at_byte(&spans, stray_closer),
            HighlightKind::PunctuationBracket,
            "the second fence's leading `}}` has no partner inside its own fence either - pairing \
             it with the first fence's `{{` is exactly the bug this test pins"
        );
    }

    #[test]
    fn an_unbalanced_fence_does_not_shift_the_ring_for_later_fences() {
        let source = "```rust\nfn a() { let x = (;\n```\n\n```rust\nfn b() { c([1]); }\n```\n";
        let spans = ring_spans(source, highlight_markdown);

        let body_brace = nth_offset(source, '{', 1);
        assert_eq!(
            ring_index_at(&spans, body_brace),
            Some(0),
            "the second fence's outermost pair must start the ring over at 0"
        );
        // ...and the `(` of `c(` nested one level inside it. That is the *fourth* `(` in the
        // document: `fn a(`, the unclosed `(`, `fn b(`, then this one.
        let call_paren = nth_offset(source, '(', 3);
        assert_eq!(
            ring_index_at(&spans, call_paren),
            Some(1),
            "one level in from a depth-0 pair is ring 1, regardless of the previous fence"
        );
    }

    #[test]
    fn two_balanced_fences_each_start_the_ring_at_zero() {
        let source = "```rust\nfn a() { b(); }\n```\n\n```rust\nfn c() { d(); }\n```\n";
        let spans = ring_spans(source, highlight_markdown);
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 1)), Some(0));
    }

    #[test]
    fn angle_brackets_never_participate_and_never_disturb_the_stack() {
        assert_eq!(depth_map("<>", "<>"), "--");
        assert_eq!(depth_map("f::<A>(x)", "<>()"), "...-.-1.1");
        assert_eq!(depth_map("<p></p>", "<>"), "-.--..-");
    }

    #[test]
    fn a_coalesced_multi_character_bracket_run_is_split_per_bracket() {
        let source = "((a))";
        let spans = bracket_spans(source, "()");
        assert_eq!(
            spans.len(),
            3,
            "premise: the two leading parens must arrive as one coalesced span - got {spans:?}"
        );
        assert_eq!(depth_map(source, "()"), "12.21");
    }

    #[test]
    fn an_untouched_bracket_run_is_re_coalesced_rather_than_left_split() {
        let source = "a<>b()";
        let out = colorize_bracket_pairs(source, bracket_spans(source, "<>()"));
        let angle = out
            .iter()
            .find(|span| span.start == 1)
            .expect("a span starting at the `<`");
        assert_eq!(
            (angle.start, angle.end, angle.kind),
            (1, 3, HighlightKind::PunctuationBracket),
            "the untouched `<>` run must stay one span - got {out:?}"
        );
    }

    #[test]
    fn source_with_no_bracket_tokens_is_returned_completely_unchanged() {
        let source = "let x = 1;";
        let spans = bracket_spans(source, "");
        let before = spans.clone();
        assert_eq!(colorize_bracket_pairs(source, spans), before);
        assert!(colorize_bracket_pairs("", Vec::new()).is_empty());
    }

    #[test]
    fn the_rewritten_span_list_stays_gapless_ordered_and_non_overlapping() {
        let source = "fn main() { let v = vec![(1, 2), (3, 4)]; }";
        let out = colorize_bracket_pairs(source, bracket_spans(source, "()[]{}"));
        assert_eq!(out.first().map(|span| span.start), Some(0));
        assert_eq!(out.last().map(|span| span.end), Some(source.len()));
        for pair in out.windows(2) {
            assert_eq!(
                pair[0].end, pair[1].start,
                "spans must remain gapless and ordered: {out:?}"
            );
            assert_ne!(
                pair[0].kind, pair[1].kind,
                "adjacent same-kind spans must have been coalesced: {out:?}"
            );
        }
    }

    #[test]
    fn multi_byte_characters_do_not_shift_bracket_offsets() {
        let source = "(\"héllo → wörld\")";
        let out = colorize_bracket_pairs(source, bracket_spans(source, "()"));
        assert_eq!(
            out.first().map(|span| (span.start, span.end, span.kind)),
            Some((0, 1, HighlightKind::Bracket1))
        );
        assert_eq!(
            out.last().map(|span| (span.start, span.end, span.kind)),
            Some((source.len() - 1, source.len(), HighlightKind::Bracket1))
        );
    }

    #[test]
    fn the_depth_ring_is_six_distinct_buckets_and_wraps_at_six() {
        let ring = HighlightKind::BRACKET_DEPTH_RING;
        for (index, kind) in ring.iter().enumerate() {
            assert_eq!(HighlightKind::for_bracket_depth(index), *kind);
            assert_eq!(HighlightKind::for_bracket_depth(index + 6), *kind);
            assert_eq!(HighlightKind::for_bracket_depth(index + 600), *kind);
        }
        let unique: HashSet<HighlightKind> = ring.into_iter().collect();
        assert_eq!(unique.len(), ring.len(), "ring buckets must all differ");
    }

    // ---------------------------------------------------------------------------------------
    // Real grammars, real spans: the pure matcher above is only worth anything if the actual
    // highlighting pipeline really delivers depth-varying brackets for real source in real
    // languages. These parse with the genuine grammars, through the same `highlight_*` entry
    // points the app calls.
    // ---------------------------------------------------------------------------------------

    /// A real grammar's spans **with the default `HighlightOptions` applied** - i.e. the exact
    /// pipeline a File view with default settings really runs. The bare `highlight_*` functions
    /// are pure grammar classification and deliberately do not apply the depth ring themselves
    /// (that is what makes `appearance.bracket_pair_colorization` able to switch it off without
    /// undoing work), so a test about the ring has to go through `HighlightOptions` the same way
    /// production does.
    fn ring_spans(source: &str, highlighter: crate::language::HighlighterFn) -> Vec<HighlightSpan> {
        HighlightOptions::default().highlight(source, Some(highlighter))
    }

    /// The bucket the span covering `byte` classifies it as. Deliberately *not* `kind_at`, which
    /// looks its argument up by substring search and so always finds the first `(` in the file -
    /// useless when the whole question is which of several identical characters this one is.
    pub(super) fn kind_at_byte(spans: &[HighlightSpan], byte: usize) -> HighlightKind {
        spans
            .iter()
            .find(|span| span.start <= byte && byte < span.end)
            .map_or(HighlightKind::Text, |span| span.kind)
    }

    /// The ring colour a real bracket at `byte` got, or `None` if the pipeline left it plain.
    pub(super) fn ring_index_at(spans: &[HighlightSpan], byte: usize) -> Option<usize> {
        let kind = spans
            .iter()
            .find(|span| span.start <= byte && byte < span.end)
            .map(|span| span.kind)?;
        HighlightKind::BRACKET_DEPTH_RING
            .iter()
            .position(|ring| *ring == kind)
    }

    /// The byte offset of the `nth` occurrence (0-based) of `needle` in `source`.
    fn nth_offset(source: &str, needle: char, nth: usize) -> usize {
        source
            .char_indices()
            .filter(|(_, ch)| *ch == needle)
            .map(|(offset, _)| offset)
            .nth(nth)
            .unwrap_or_else(|| panic!("{source:?} has no {nth}th {needle:?}"))
    }

    #[test]
    fn real_rust_source_gets_real_depth_varying_bracket_colours() {
        let source = "fn main() {\n    let v = vec![(1, \"(\")];\n}\n";
        let spans = ring_spans(source, highlight_rust);

        let brace_open = nth_offset(source, '{', 0);
        let bracket_open = nth_offset(source, '[', 0);
        let paren_open = nth_offset(source, '(', 1); // after `main(`
        let paren_close = nth_offset(source, ')', 1);
        let bracket_close = nth_offset(source, ']', 0);
        let brace_close = nth_offset(source, '}', 0);

        assert_eq!(ring_index_at(&spans, brace_open), Some(0));
        assert_eq!(ring_index_at(&spans, bracket_open), Some(1));
        assert_eq!(ring_index_at(&spans, paren_open), Some(2));
        assert_eq!(
            ring_index_at(&spans, paren_close),
            Some(2),
            "a closer must carry its own pair's colour, not the next one's"
        );
        assert_eq!(ring_index_at(&spans, bracket_close), Some(1));
        assert_eq!(ring_index_at(&spans, brace_close), Some(0));

        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 0)), Some(0));

        let in_string = nth_offset(source, '(', 2);
        assert_eq!(
            kind_at_byte(&spans, in_string),
            HighlightKind::String,
            "a paren inside a string literal must never reach the bracket ring"
        );
        assert_eq!(ring_index_at(&spans, in_string), None);
    }

    #[test]
    fn real_typescript_source_gets_real_depth_varying_bracket_colours() {
        let source = "function f(m: Map<string, number>) {\n  return [{ a: 1 }];\n}\n";
        let spans = ring_spans(source, highlight_ts);

        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, ')', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '[', 0)), Some(1));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 1)), Some(2));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '}', 0)), Some(2));
        assert_eq!(ring_index_at(&spans, nth_offset(source, ']', 0)), Some(1));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '}', 1)), Some(0));

        let angle_open = nth_offset(source, '<', 0);
        assert_eq!(
            kind_at_byte(&spans, angle_open),
            HighlightKind::PunctuationBracket,
            "premise: this grammar really does capture a type-argument `<` as punctuation.bracket"
        );
        assert_eq!(
            ring_index_at(&spans, angle_open),
            None,
            "an angle bracket must stay plain - see colorize_bracket_pairs' own docs"
        );
        assert_eq!(ring_index_at(&spans, nth_offset(source, '>', 0)), None);
    }

    #[test]
    fn real_python_source_gets_real_depth_varying_bracket_colours() {
        let source =
            "def f():\n    return sorted([(k, v) for k, v in d.items()])  # ) not a bracket\n";
        let spans = ring_spans(source, highlight_python);

        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 1)), Some(0)); // sorted(
        assert_eq!(ring_index_at(&spans, nth_offset(source, '[', 0)), Some(1));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 2)), Some(2)); // (k, v)
        assert_eq!(ring_index_at(&spans, nth_offset(source, ')', 1)), Some(2));
        assert_eq!(ring_index_at(&spans, nth_offset(source, ']', 0)), Some(1));

        let in_comment = nth_offset(source, ')', 4);
        assert_eq!(
            kind_at_byte(&spans, in_comment),
            HighlightKind::Comment,
            "a paren inside a `#` comment must never reach the bracket ring"
        );
        assert_eq!(ring_index_at(&spans, in_comment), None);
    }

    #[test]
    fn rendered_line_runs_really_carry_distinct_bracket_colours() {
        let source = "fn main() {\n    let v = vec![(1, 2)];\n}\n";
        let lines = build_lines(source, &ring_spans(source, highlight_rust));

        let ring_runs: Vec<(String, HighlightKind)> = lines
            .iter()
            .flat_map(|line| line.runs.iter())
            .filter(|(_, kind)| HighlightKind::BRACKET_DEPTH_RING.contains(kind))
            .map(|(text, kind)| (text.to_string(), *kind))
            .collect();

        // Every bracket character in this sample is really matched, so every one of them must be
        // inside a ring run - counted by character, because two *adjacent* brackets at the same
        // depth (`main()`'s own empty parameter list) legitimately coalesce back into one run.
        // That merge is invisible on screen - both halves paint the same colour either way - and
        // it is the same run-count economy `fold_highlight_events` already practises.
        let coloured_chars: usize = ring_runs.iter().map(|(text, _)| text.chars().count()).sum();
        assert_eq!(
            coloured_chars,
            source.chars().filter(|ch| "()[]{}".contains(*ch)).count(),
            "every matched bracket character must be inside a ring run - got {ring_runs:?}"
        );
        for (text, _) in &ring_runs {
            assert!(
                text.chars().all(|ch| "()[]{}".contains(ch)),
                "a ring run must contain nothing but brackets - got {ring_runs:?}"
            );
        }
        let distinct: HashSet<HighlightKind> = ring_runs.iter().map(|(_, kind)| *kind).collect();
        assert!(
            distinct.len() >= 3,
            "this sample nests three levels deep, so at least three ring buckets must appear - \
             got {ring_runs:?}"
        );

        let colors: HashSet<[u32; 4]> = distinct
            .iter()
            .map(|kind| {
                let color = color_for_kind(*kind);
                [
                    color.r.to_bits(),
                    color.g.to_bits(),
                    color.b.to_bits(),
                    color.a.to_bits(),
                ]
            })
            .collect();
        assert_eq!(
            colors.len(),
            distinct.len(),
            "every distinct ring bucket must resolve to its own distinct colour"
        );
        assert!(
            !colors.contains(&{
                let plain = color_for_kind(HighlightKind::PunctuationBracket);
                [
                    plain.r.to_bits(),
                    plain.g.to_bits(),
                    plain.b.to_bits(),
                    plain.a.to_bits(),
                ]
            }),
            "no ring colour may equal the plain unmatched-bracket colour"
        );
    }

    #[test]
    fn bracket_colouring_really_works_in_every_language_that_can_support_it() {
        /// `(language label, its real highlighter, a nesting sample)`.
        type LanguageSample = (&'static str, crate::language::HighlighterFn, &'static str);

        let coloured: [LanguageSample; 10] = [
            ("rust", highlight_rust, "fn f() { g(vec![1]); }"),
            ("typescript", highlight_ts, "function f() { g([1]); }"),
            ("tsx", highlight_tsx, "function f() { g([1]); }"),
            ("python", highlight_python, "def f():\n    g([1])\n"),
            ("toml", highlight_toml, "a = [[1], [2]]\n"),
            ("go", highlight_go, "func f() { g([]int{1}) }"),
            ("json", highlight_json, "{\"a\": [1]}"),
            ("yaml", highlight_yaml, "a: [1, {b: 2}]\n"),
            (
                "c",
                highlight_c,
                "int f(void) { int a[] = {1}; return a[0]; }",
            ),
            ("css", highlight_css, ".a { color: rgb(1, 2, 3); }"),
        ];
        for (name, highlight, source) in coloured {
            let spans = ring_spans(source, highlight);
            let ring: Vec<&str> = spans
                .iter()
                .filter(|span| HighlightKind::BRACKET_DEPTH_RING.contains(&span.kind))
                .map(|span| &source[span.start..span.end])
                .collect();
            assert!(
                ring.len() >= 2,
                "{name} must really get depth-coloured brackets - got {ring:?}"
            );
            let distinct: HashSet<HighlightKind> = spans
                .iter()
                .map(|span| span.kind)
                .filter(|kind| HighlightKind::BRACKET_DEPTH_RING.contains(kind))
                .collect();
            assert!(
                distinct.len() >= 2,
                "{name}'s sample nests, so it must really show more than one ring colour - got \
                 {ring:?}"
            );
        }

        let excluded: [LanguageSample; 2] = [
            ("markdown", highlight_markdown, "prose (a) [b]\n"),
            ("html", highlight_html, "<div><p>x</p></div>"),
        ];
        for (name, highlight, source) in excluded {
            let spans = ring_spans(source, highlight);
            assert!(
                !spans
                    .iter()
                    .any(|span| HighlightKind::BRACKET_DEPTH_RING.contains(&span.kind)),
                "{name} is a deliberate exclusion - see this test's own docs"
            );
        }
    }

    #[test]
    fn disabling_bracket_pair_colorization_really_leaves_brackets_plain() {
        let off = HighlightOptions {
            bracket_pair_colorization: false,
        };
        let cases: [(&str, crate::language::HighlighterFn, &str); 3] = [
            (
                "rust",
                highlight_rust,
                "fn main() { let v = vec![(1, 2)]; }",
            ),
            (
                "typescript",
                highlight_ts,
                "function f() { g([{ a: 1 }]); }",
            ),
            ("python", highlight_python, "def f():\n    g([(1, 2)])\n"),
        ];
        for (name, highlighter, source) in cases {
            let raw = highlighter(source);
            let disabled = off.apply(source, raw.clone());
            let enabled = HighlightOptions::default().apply(source, raw.clone());

            assert_eq!(
                disabled, raw,
                "{name}: with the setting off the span list must be exactly what the grammar                  produced - the pass must not run at all"
            );
            assert!(
                !disabled
                    .iter()
                    .any(|span| HighlightKind::BRACKET_DEPTH_RING.contains(&span.kind)),
                "{name}: no ring bucket may survive with the setting off"
            );
            assert!(
                disabled
                    .iter()
                    .any(|span| span.kind == HighlightKind::PunctuationBracket),
                "{name}: brackets must still be classified as plain PunctuationBracket, not lost"
            );
            assert!(
                enabled
                    .iter()
                    .any(|span| HighlightKind::BRACKET_DEPTH_RING.contains(&span.kind)),
                "{name}: premise - the same source really does get the ring when enabled"
            );
        }
    }

    #[test]
    fn re_enabling_bracket_pair_colorization_restores_the_identical_ring() {
        let source = "fn main() { let v = vec![(1, 2)]; }";
        let raw = highlight_rust(source);
        let on = HighlightOptions::default();
        let off = HighlightOptions {
            bracket_pair_colorization: false,
        };

        let first = on.apply(source, raw.clone());
        let disabled = off.apply(source, raw.clone());
        let restored = on.apply(source, raw.clone());

        assert_eq!(
            first, restored,
            "re-enabling must reproduce the ring exactly"
        );
        assert_ne!(
            disabled, restored,
            "premise: off and on really do differ for this source"
        );
    }

    #[test]
    fn the_options_aware_entry_points_really_honour_the_setting() {
        let off = HighlightOptions {
            bracket_pair_colorization: false,
        };
        let ring_count = |lines: &[RenderedLine]| {
            lines
                .iter()
                .flat_map(|line| line.runs.iter())
                .filter(|(_, kind)| HighlightKind::BRACKET_DEPTH_RING.contains(kind))
                .count()
        };

        let hunk = ["fn main() {", "    let v = vec![(1, 2)];", "}"];
        assert!(
            ring_count(&highlight_block(
                hunk,
                Some("rs"),
                HighlightOptions::default()
            )) > 0,
            "premise: the default really colours this hunk"
        );
        assert_eq!(
            ring_count(&highlight_block(hunk, Some("rs"), off)),
            0,
            "highlight_block_with_options must really honour the setting"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "fn main() { let v = vec![(1, 2)]; }\n").expect("write");
        let (on_parsed, _) =
            load_file_with_options(&path, HighlightOptions::default()).expect("load");
        let (off_parsed, _) = load_file_with_options(&path, off).expect("load");
        assert!(
            ring_count(&on_parsed.lines) > 0,
            "premise: loading really colours this file by default"
        );
        assert_eq!(
            ring_count(&off_parsed.lines),
            0,
            "load_file_with_options must really honour the setting"
        );
    }

    #[test]
    fn brackets_inside_a_markdown_fenced_code_block_reach_the_ring() {
        let source = "# Title\n\n```rust\nfn f() { g(1); }\n```\n";
        let spans = ring_spans(source, highlight_markdown);
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 1)), Some(1));
    }

    #[test]
    fn a_partial_hunk_colours_its_own_balanced_pairs_and_leaves_the_dangling_ones_plain() {
        let rendered = highlight_block(
            ["    let v = vec![1];", "}"],
            Some("rs"),
            HighlightOptions::default(),
        );
        let runs: Vec<HighlightKind> = rendered
            .iter()
            .flat_map(|line| line.runs.iter())
            .map(|(_, kind)| *kind)
            .collect();
        assert!(
            runs.iter()
                .any(|kind| HighlightKind::BRACKET_DEPTH_RING.contains(kind)),
            "the hunk's own balanced `[1]` must still be coloured - got {runs:?}"
        );
        assert!(
            runs.contains(&HighlightKind::PunctuationBracket),
            "the trailing `}}`, whose partner is outside the hunk, must stay plain - got {runs:?}"
        );
    }
}

/// GitHub issue: syntax theme redesign - the real fixture corpus.
#[cfg(test)]
mod fixture_corpus_tests {
    use super::tests::{kind_at, kind_at_byte, ring_index_at};
    use super::*;
    use crate::theme;

    const RUST: &str = include_str!("testdata/fixture.rs.txt");
    const TSX: &str = include_str!("testdata/fixture.tsx.txt");
    const PYTHON: &str = include_str!("testdata/fixture.py.txt");
    const MARKDOWN: &str = include_str!("testdata/fixture.md.txt");
    const DEEP: &str = include_str!("testdata/fixture.deep.rs.txt");
    const TORTURE: &str = include_str!("testdata/fixture.torture.rs.txt");

    fn spans_of(source: &str, highlighter: crate::language::HighlighterFn) -> Vec<HighlightSpan> {
        HighlightOptions::default().highlight(source, Some(highlighter))
    }

    /// Every distinct `(bucket, resolved colour, contrast against the editor background)` a fixture
    /// actually produces - the reviewable table a screenshot would only have implied.
    fn dump(
        label: &str,
        source: &str,
        highlighter: crate::language::HighlighterFn,
    ) -> Vec<HighlightKind> {
        let spans = spans_of(source, highlighter);
        let background = theme::surface::CENTER.resolve();
        let mut seen: Vec<HighlightKind> = Vec::new();
        for span in &spans {
            if !seen.contains(&span.kind) {
                seen.push(span.kind);
            }
        }
        println!("\n=== {label} ===");
        for kind in &seen {
            let color = color_for_kind(*kind);
            let rgba = color;
            println!(
                "  {:24} #{:02x}{:02x}{:02x}  contrast {:5.2}:1",
                kind.name(),
                (rgba.r * 255.0).round() as u32,
                (rgba.g * 255.0).round() as u32,
                (rgba.b * 255.0).round() as u32,
                theme::contrast_ratio(rgba, background)
            );
        }
        seen
    }

    /// Every bucket a fixture reaches must clear its own contrast floor. This is the property a
    /// reviewer would otherwise be squinting at a screenshot to judge.
    fn assert_every_bucket_is_readable(label: &str, seen: &[HighlightKind]) {
        let background = theme::surface::CENTER.resolve();
        for kind in seen {
            let rgba: gpui::Rgba = color_for_kind(*kind);
            let key: String = format!("syntax.{}", kind.name());
            let Some(floor) = theme::syntax_contrast_floor_for_test(&key) else {
                continue;
            };
            let ratio = theme::contrast_ratio(rgba, background);
            assert!(
                ratio >= floor,
                "{label}: {} renders at {ratio:.2}:1, below its {floor}:1 floor",
                kind.name()
            );
        }
    }

    #[test]
    fn rust_fixture_is_fully_readable_and_colours_only_what_it_should() {
        let seen = dump("Rust", RUST, highlight_rust);
        assert_every_bucket_is_readable("Rust", &seen);
        let spans = spans_of(RUST, highlight_rust);
        assert_eq!(
            kind_at(&spans, RUST, "parse(input"),
            HighlightKind::FunctionDefinition
        );
        // `push` is a *method* call, so it lands in the FunctionMethod bucket. Both call buckets
        // share one blue, and the definition site is a distinct violet-blue - the restraint
        // revision held all three at plain foreground and this is the assertion that replaced it.
        assert_eq!(
            kind_at(&spans, RUST, "push("),
            HighlightKind::FunctionMethod
        );
        for call in [HighlightKind::Function, HighlightKind::FunctionMethod] {
            assert_ne!(
                color_for_kind(call),
                color_for_kind(HighlightKind::Text),
                "a call site carries real colour"
            );
            assert_ne!(
                color_for_kind(call),
                color_for_kind(HighlightKind::FunctionDefinition),
                "and is still tellable apart from a definition site"
            );
        }
        for needle in ["{ brace ) in", "] ["] {
            let offset = RUST.find(needle).expect("fixture contains it");
            assert!(
                matches!(
                    kind_at_byte(&spans, offset),
                    HighlightKind::String | HighlightKind::StringEscape
                ),
                "the brackets in {needle:?} must stay part of the string"
            );
        }
    }

    #[test]
    fn tsx_fixture_is_fully_readable_and_leaves_generics_plain() {
        let seen = dump("TSX", TSX, highlight_tsx);
        assert_every_bucket_is_readable("TSX", &seen);
        let spans = spans_of(TSX, highlight_tsx);
        for (index, _) in TSX.match_indices('<') {
            assert_eq!(
                kind_at_byte(&spans, index),
                HighlightKind::PunctuationBracket,
                "a `<` at byte {index} must never join the bracket ring"
            );
        }
    }

    #[test]
    fn python_fixture_is_fully_readable() {
        let seen = dump("Python", PYTHON, highlight_python);
        assert_every_bucket_is_readable("Python", &seen);
        let spans = spans_of(PYTHON, highlight_python);
        assert_eq!(
            kind_at(&spans, PYTHON, "parse(self"),
            HighlightKind::FunctionDefinition
        );
    }

    #[test]
    fn markdown_fixture_is_fully_readable_and_pairs_each_fence_independently() {
        let seen = dump("Markdown", MARKDOWN, highlight_markdown);
        assert_every_bucket_is_readable("Markdown", &seen);
        let spans = spans_of(MARKDOWN, highlight_markdown);
        let rust_brace = MARKDOWN.find("fn a() {").expect("rust fence") + 7;
        let python_brace = MARKDOWN.find("d = {").expect("python fence") + 4;
        for offset in [rust_brace, python_brace] {
            assert_eq!(
                ring_index_at(&spans, offset),
                Some(0),
                "each fence's outermost pair must start at ring 0"
            );
        }
    }

    #[test]
    fn the_deep_nesting_fixture_stays_legible_with_the_ring_on_and_off() {
        let seen = dump("Deep nesting", DEEP, highlight_rust);
        assert_every_bucket_is_readable("Deep nesting", &seen);

        let on = spans_of(DEEP, highlight_rust);
        let opens: Vec<usize> = DEEP.match_indices('(').map(|(index, _)| index).collect();
        let ring: Vec<Option<usize>> = opens.iter().map(|o| ring_index_at(&on, *o)).collect();
        let used: std::collections::HashSet<usize> = ring.iter().flatten().copied().collect();
        assert_eq!(
            used.len(),
            6,
            "a twelve-level fixture must exercise all six ring colours, got {used:?}"
        );

        // Ring off: every bracket falls back to the one de-emphasized punctuation tone, and that
        // tone is still readable - which is what "legible with rainbow off" actually means.
        let off = HighlightOptions {
            bracket_pair_colorization: false,
        }
        .highlight(DEEP, Some(highlight_rust));
        for (index, _) in DEEP.match_indices('(') {
            assert_eq!(
                kind_at_byte(&off, index),
                HighlightKind::PunctuationBracket,
                "with the ring off every bracket must be the plain punctuation tone"
            );
        }
        let rgba: gpui::Rgba = color_for_kind(HighlightKind::PunctuationBracket);
        assert!(
            theme::contrast_ratio(rgba, theme::surface::CENTER.resolve()) >= 3.0,
            "the ring-off bracket tone must still clear 3:1"
        );
    }

    #[test]
    fn the_torture_fixture_never_colours_a_bracket_that_is_not_real_code() {
        let seen = dump("Bracket torture", TORTURE, highlight_rust);
        assert_every_bucket_is_readable("Bracket torture", &seen);
        let spans = spans_of(TORTURE, highlight_rust);

        let comment_paren = TORTURE.find("Unmatched (").expect("fixture") + 10;
        assert_eq!(kind_at_byte(&spans, comment_paren), HighlightKind::Comment);

        let stray = TORTURE.find("([)]").expect("fixture") + 2;
        assert_eq!(
            kind_at_byte(&spans, stray),
            HighlightKind::PunctuationBracket,
            "a closer whose shape does not match the innermost opener must stay plain"
        );
    }
}
