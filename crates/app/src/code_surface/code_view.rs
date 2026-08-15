//! Pure logic for Surface C's File view (`design_handoff_jerry_ade/README.md`'s "File view"
//! subsection): reads a file off disk, detects its line-ending style, picks a language label
//! from its extension, and - for a real subset of extensions - produces syntax-colored spans by
//! parsing with `tree-sitter` and walking the resulting AST. Deliberately `gpui`-window-free
//! (only [`gpui::Rgba`] is used, for plain colour data), mirroring this crate's split between
//! pure logic modules and `crate::root`'s `Div` construction.
//!
//! `.rs`/`.ts`/`.tsx`/`.js`/`.jsx`/`.py`/`.toml`/`.go`/`.json`/`.yaml`/`.yml`/`.c`/`.h`/`.md`/
//! `.html`/`.htm`/`.css` get real syntax spans; other extensions (including `.vue` - see
//! `crate::language`'s docs for why this phase doesn't spawn an LSP client for it, unrelated to
//! highlighting, and `.sql` - a real, stated follow-up, GitHub issue #32's own remaining scope)
//! render as plain monospace text.
//!
//! Markdown additionally drives **real cross-language injection** (GitHub issue #154): a fenced
//! code block's content is reparsed with whatever language its info string names, resolved against
//! this crate's own registry through [`Grammar::for_injection_name`], and raw HTML written into a
//! markdown document - block-level or inline - reaches `tree-sitter-html`. HTML in turn injects
//! its own `<style>`/`<script>` bodies into CSS/JavaScript. See [`MARKDOWN_INJECTION_QUERY`] and
//! [`MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT`] for the two real, empirically-found repairs that
//! actually takes.
//!
//! ## How highlighting actually works here
//!
//! Classification is done by the official `tree-sitter-highlight` crate, driving each grammar
//! crate's **own published `queries/highlights.scm`** (exposed by all three as a `&'static str`
//! constant, so no query file is vendored or read from disk). This replaced a hand-rolled
//! recursive AST walk matching node kinds against per-language node-kind tables maintained here -
//! see [`HIGHLIGHT_NAMES`] and [`HIGHLIGHT_KINDS`] for how the standard capture vocabulary those
//! real query files emit (`keyword`, `function.method`, `constant.builtin`, `punctuation.bracket`,
//! ...) is folded down into the six buckets `design_handoff_jerry_ade/README.md`'s File view
//! colour table actually defines, and [`highlight_query_for`] for the one genuinely surprising
//! part (TypeScript's own query file is a supplement, not a whole query).
//!
//! Adding a further grammar is now a [`Grammar`] variant plus a thin wrapper - no node-kind table.
//!
//! ## API verification
//!
//! `tree-sitter-highlight` is not used anywhere in `vendor/zed`, so there is no in-tree call site
//! to check this against. Every non-obvious behavioural claim made in the docs below was instead
//! verified by reading the real crate source on disk
//! (`~/.cargo/registry/src/*/tree-sitter-highlight-0.26.9/src/highlight.rs`) and is cited to the
//! exact lines there - specifically the recognized-name matching rule
//! (`configure`, lines 458-484), the last-pattern-wins rule for two patterns capturing one node
//! (lines 1043-1066), and the fact that the engine's internal parse passes `None` as its old tree
//! with no way for a caller to supply one (lines 531-541).
//!
//! ## Why there is no incremental reparse here
//!
//! There is a real, measured ~55% win available from incremental reparsing, and it is
//! nevertheless deliberately not taken. Both halves of that are worth recording, because the
//! numbers say "obviously do it" and the API says "you cannot".
//!
//! Measured on this repository's own largest file at the time (`root/code_surface.rs`, 5931
//! lines, since split into `crate::code_surface`'s several files), release
//! build, median of five runs on an idle machine: one full `highlight_rust` call costs ~31.4ms, of
//! which the bare `tree_sitter::Parser::parse` is ~16.6ms (53%) and the query walk is the rest.
//! Reparsing the same file after a real one-character edit, via `Tree::edit` +
//! `parse(.., Some(&old_tree))`, costs **0.21ms** - a ~79x reduction, verified in the same run to
//! produce a tree identical (compared as s-expressions) to a fresh parse of the edited text.
//!
//! For scale, the same call under the replaced hand-rolled walker cost ~21.5ms: the real query
//! walk is genuinely more work than the old node-kind table lookup, and buys the accuracy gains
//! this migration is for.
//!
//! It cannot be reached from here. `tree_sitter_highlight::Highlighter::highlight` owns its parse
//! entirely and hard-codes `None` as the old-tree argument
//! (`tree-sitter-highlight-0.26.9/src/highlight.rs:531-541`); there is no parameter, no builder
//! and no callback to supply a previous tree, and the crate's newest release at time of writing
//! (0.26.11) is identical in that respect - so this is an API limitation, not a version to
//! upgrade past. The only way to have both would be to stop using the official highlight iterator
//! and drive `config.query` by hand against a self-parsed tree, which would mean reimplementing
//! its capture-precedence rules *and* its `#match?` predicate evaluation - and every one of these
//! three grammars leans on `#match?` for its "identifier starting with a capital letter" rules.
//! That is precisely the bespoke engine this module exists to have deleted, so it is not done.
//!
//! What makes that an acceptable trade rather than a swallowed regression is where the cost lands
//! - which differs by caller, so it is worth being precise rather than sweeping:
//!
//! - **The File view's live re-highlight**, the one path that runs repeatedly while typing, is
//!   already off the foreground thread: `crate::code_surface::editing::AdeApp::schedule_rehighlight` runs
//!   it on the background executor 150ms after the last keystroke, and the view keeps rendering
//!   the previous highlighting until it lands. Not having incremental reparse costs that path a
//!   background thread for ~31ms instead of ~15ms on the largest file here; it is not frame time.
//! - **[`highlight_block`]'s Diff and Merge callers** (`crate::code_surface`'s
//!   `ensure_diff_highlight_cache`, `crate::merge::render`'s
//!   `ensure_merge_highlight_cache`) do run synchronously on the main thread, by their own
//!   deliberate design - but they are called only when the content actually changes, never per
//!   frame, and each caps the work at a real per-file line budget rather than highlighting a whole
//!   file. Incremental reparse would not have helped them regardless: each hunk is highlighted as
//!   its own isolated source unit, so there is no previous tree of the *same* text to reuse.

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
    ///
    /// It exists for exactly one consumer, [`colorize_bracket_pairs`], which must not pair a `{`
    /// in one fenced code block with a `}` in the *next* one. Carrying the region on the span is
    /// what lets that pass stay a pure function of the span list while still being
    /// injection-aware - the alternative (teaching every caller to also thread a range list
    /// alongside the spans) would put the same invariant in nine places instead of one.
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
    ///
    /// Two vocabularies genuinely feed into this, which is why it is two lookups rather than one
    /// table:
    ///
    /// 1. **A grammar's own internal name** ([`Grammar::name`]), which is what a hard-coded
    ///    `#set!` predicate in a bundled `injections.scm` says: `tree-sitter-md`'s
    ///    `"markdown_inline"` and `"html"`, `tree-sitter-html`'s `"css"`.
    /// 2. **A fenced code block's own info string**, which is free-form author-written text
    ///    (` ```rust `, ` ```py `, ` ```yml `) and is matched through
    ///    [`crate::language::extension_for_fence_language`] - the one shared fence-tag alias table
    ///    this crate has, also used by `crate::code_surface::markdown_preview`'s rendered code
    ///    blocks, so source-view and preview-mode fences can never disagree about what ` ```py `
    ///    means.
    ///
    /// An unrecognized name returns `None`, which `tree-sitter-highlight` handles by simply not
    /// creating an injected layer (`highlight.rs:910-917`, read directly) - the content then stays
    /// exactly as the outer grammar classified it. That is the honest fallback for a ` ```zig `
    /// fence this app has no grammar for, and for `tree-sitter-md`'s own `"latex"` injection: no
    /// panic, no fabricated colouring.
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
    ///
    /// This deliberately does *not* try to derive itself from `ExtensionEntry::highlighter`'s own
    /// `fn` pointer: comparing function pointers for equality is not something Rust guarantees
    /// (identical monomorphisations may be merged), so a table that looked derived would in fact
    /// be relying on unspecified behaviour. An explicit match plus a real drift test is the honest
    /// version of the same guarantee.
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
///
/// ## The matching rule - and how it *is* the fallback chain (GitHub issue #31)
///
/// The real matching rule is not a prefix match, and this is the single most important thing to
/// understand before editing this list. `HighlightConfiguration::configure`
/// (`tree-sitter-highlight-0.26.9/src/highlight.rs:458-484`, read directly) splits both the
/// query's capture name and each recognized name on `.`, and a recognized name matches when
/// **every one of its own dot-parts is present among the capture's dot-parts**; among all
/// matches, the one with the *most* parts wins, ties going to the earliest entry here.
///
/// This is exactly the mechanism issue #31 asks for: registering both a parent scope (`"variable"`)
/// and a child scope (`"variable.parameter"`) means a real `@variable.parameter` capture prefers
/// the more specific entry, while a grammar that only ever emits the plain parent capture still
/// gets a real, intentional bucket rather than falling through unmatched - the specificity rule
/// *is* the fallback chain, enforced by the engine itself rather than by a second, hand-rolled
/// lookup here. `theme::syntax`'s own module docs describe the second half of the chain: even a
/// scope with its own dedicated [`HighlightKind`] variant can still resolve to a *colour* that is a
/// real, direct alias of its parent's, rather than an independently-authored hue.
///
/// ## Real capture names, verified per grammar - not guessed
///
/// Every name below was checked against the real, fetched grammar crates' own bundled
/// `queries/highlights.scm`/`highlights-jsx.scm` files under
/// `~/.cargo/registry/src/*/tree-sitter-{rust,python,javascript,typescript}-*/queries/`, not
/// assumed from GitHub issue #31's own checklist wording (which turned out to name two scopes,
/// `comment.doc` and `string.escape`, that none of these four grammars actually emit - see below).
///
/// - **`keyword`/`function`/`type`/`comment`/`constructor`/`variable.builtin`** - unchanged from
///   before this issue; see the historical notes further down.
/// - **`function.method`** - real (`tree-sitter-rust`: `@function.method` on a `field_expression`
///   call callee; `-javascript`: `@function.method` on a `property_identifier` method definition/
///   call).
/// - **`type.builtin`** - real (`-rust`: `(primitive_type) @type.builtin`; `-typescript`:
///   `(predefined_type) @type.builtin`, e.g. `void`/`number`/`string`/`unknown`).
/// - **`constant`/`constant.builtin`** - real (`-rust`: an all-caps identifier is `@constant`, a
///   bool/int/float literal is `@constant.builtin`; `-python`/`-javascript`: the same all-caps
///   heuristic for `@constant`, and `true`/`false`/`None`/`null`/`undefined` for
///   `@constant.builtin`).
/// - **`string`** - real in all four (`(string_literal)`/`(char_literal)`/`(raw_string_literal)`
///   for Rust, `(string)` for Python, `[(string)(template_string)]` for JS/TS).
/// - **`escape`, registered here alongside `string.escape`** - `escape`, not `string.escape`, is
///   the real capture name (`-rust`: `(escape_sequence) @escape`; `-python`: identical). Neither
///   JavaScript's nor TypeScript's own bundled query captures an escape sequence at all - verified
///   directly, not assumed - so this bucket is genuinely reachable for Rust/Python source only.
///   `"string.escape"` (the issue's own checklist name) is registered too, and - correcting what
///   this comment claimed until the theme redesign audited it - it is **not** dead: both
///   `tree-sitter-markdown` and `tree-sitter-markdown-inline` really do emit
///   `(backslash_escape) @string.escape`, so a `\*` in a markdown document reaches this bucket
///   through it. The claim that it "never matches anything today" was written before Markdown
///   support existed (GitHub issue #104) and was never revisited.
/// - **`number`** - real in Python/JS/TS (`[(integer)(float)] @number` / `(number) @number`).
///   Rust has no `number` capture at all; its numeric literals are `@constant.builtin` instead
///   (unchanged from before this issue).
/// - **`comment.documentation`** - the real capture name (`-rust`: `(line_comment (doc_comment))
///   @comment.documentation`); no other grammar here has a doc-comment concept in its bundled
///   query. `"comment.doc"` (GitHub issue #31's own checklist name) used to be registered
///   alongside it as a forward-compatibility synonym; the theme redesign's coverage audit found it
///   was the *only* genuinely dead entry in this list - no grammar emits it, and unlike
///   `"string.escape"` none has started to - so it was removed rather than left to imply a
///   coverage it never had.
/// - **`variable`** - real, and a genuinely large behavioural change from before this issue:
///   `-python`'s query captures *every* identifier as `@variable` via one blanket top-of-file
///   rule (`(identifier) @variable`), and `-javascript`'s does the same. Previously unregistered,
///   so every one of those was silently `Text`; seeing `theme::syntax::VARIABLE` is a direct alias
///   of `theme::syntax::TEXT` (see that module's docs) is what keeps this a classification-only
///   change with **no** visual difference for existing files.
/// - **`variable.parameter`** - real (`-rust`: `(parameter (identifier) @variable.parameter)`;
///   `-typescript`: `required_parameter`/`optional_parameter`). Neither Python nor JavaScript's
///   own bundled query captures a parameter name distinctly at all - it arrives as a plain
///   `@variable` there instead, which is a real, grammar-level limitation, not a gap in this list.
/// - **`property`** - real (`-rust`: `(field_identifier) @property`; `-python`:
///   `(attribute attribute: (identifier) @property)`; `-javascript`:
///   `(property_identifier) @property`, unconditionally - this crate's own `code_view` test
///   module has two pre-existing TypeScript regression tests
///   (`typescript_const_variable_name_is_not_misclassified_as_a_function`,
///   `typescript_interface_member_name_is_not_misclassified_as_a_function`) whose expected
///   [`HighlightKind`] needed updating for exactly this reason).
/// - **`operator`** - real in all four grammars' own bundled queries (Rust: `*`/`&`/`'` only, a
///   short list; Python/JS: their full symbolic-operator tables).
/// - **`punctuation.bracket`/`punctuation.delimiter`** - real in all four.
/// - **`tag`/`attribute`** - real, both JSX-only (`-javascript`'s `highlights-jsx.scm`): a
///   lowercase JSX element name is `@tag`, a JSX attribute name is `@attribute`.
/// - **`embedded`** - real (`-python`'s f-string interpolation, `-javascript`'s template-literal
///   `${...}` substitution) - see [`HighlightKind::Embedded`]'s own docs (via `theme::syntax`) for
///   why this rarely shows through in practice despite being a real, live capture.
///
/// GitHub issue #32 ("add language highlight support") added five more grammars and, with them,
/// five more real registered names - each checked against that grammar's own bundled
/// `queries/highlights.scm`, not guessed:
///
/// - **`boolean`** (`-toml`'s `(boolean) @boolean`, `-yaml`'s `(boolean_scalar) @boolean`) ->
///   [`HighlightKind::ConstantBuiltin`], the same bucket `true`/`false` already reach through the
///   original four grammars' own `constant.builtin` capture.
/// - **`delimiter`** (`-c`'s plain `"." @delimiter`/`";" @delimiter` - distinct from the
///   already-registered `punctuation.delimiter`, one dot-part short of it) ->
///   [`HighlightKind::PunctuationDelimiter`], the bucket its dotted cousin already reaches.
/// - **`label`** (`-c`'s `(statement_identifier) @label`, a goto target; `-yaml`'s
///   `[(anchor_name)(alias_name)] @label`, an anchor/alias reference; `-rust`'s own `@label`, a
///   lifetime annotation `'a`) -> [`HighlightKind::Label`] - its own real, dedicated bucket as of
///   GitHub issue #183 (previously folded into [`HighlightKind::Variable`], a different real
///   concept again). See [`HighlightKind::Label`]'s own docs for why a Rust lifetime, a C goto
///   target and a YAML anchor still share this one bucket rather than three.
/// - **`punctuation.special`** (`-yaml`'s `["*" "&" "---" "..."] @punctuation.special`, anchor/
///   alias sigils and document markers; `-javascript`'s own template-literal `${`/`}`
///   interpolation delimiters; Markdown's ATX `#` heading marker and list bullets) ->
///   [`HighlightKind::PunctuationSpecial`] - its own real bucket as of GitHub issue #183
///   (previously folded into [`HighlightKind::Operator`], which read a heading marker as an
///   arithmetic operator).
/// - **`string.special.key`** (`-json`'s `key: (_) @string.special.key`, an object key) ->
///   [`HighlightKind::Property`], not [`HighlightKind::String`]: a JSON key is semantically a
///   property name, matching how the other grammars already classify a struct/object field name.
///   Registered as its own, more-specific 3-part name specifically to *win* over the
///   already-registered plain `"string"` (which would otherwise also match, by the "most parts
///   wins" rule described above, and misclassify it as a plain string value) - and, since GitHub
///   issue #183, over the new, less-specific 2-part `"string.special"` below too, for the same
///   reason.
///
/// GitHub issue #183 registered four more real captures, each previously falling through to a
/// coarser, already-registered entry via the "fewer parts still matches" rule - genuinely losing
/// grammar-level meaning, not just an internal implementation detail, since e.g. a JS/TS regex
/// literal and an ordinary string both used to read as [`HighlightKind::String`]:
///
/// - **`string.special`** (`-javascript`'s `(regex) @string.special`; `-toml`'s date/time
///   literals; `-css`'s `(color_value) @string.special`) -> [`HighlightKind::StringSpecial`],
///   previously falling through to the plain `"string"` entry above.
/// - **`function.builtin`** (`-python`'s `len`/`print`/...; `-go`'s `append`/`make`/`panic`/...;
///   `-javascript`'s `require`) -> [`HighlightKind::FunctionBuiltin`], previously falling through
///   to the plain `"function"` entry.
/// - **`function.macro`** (`-rust`'s `println!`-style macro invocations, both the macro name and
///   its own `!`) -> [`HighlightKind::FunctionMacro`], previously falling through to the plain
///   `"function"` entry.
/// - **`tag.error`** (`-html`'s `(erroneous_end_tag_name) @tag.error`, a mismatched closing tag)
///   -> [`HighlightKind::TagError`], previously falling through to the plain `"tag"` entry.
///
/// `-c`'s own `@function.special` (a `#define` macro name) is deliberately **not** given its own
/// entry here - it stays folded into [`HighlightKind::Function`] via the plain `"function"` entry,
/// out of scope for GitHub issue #183's own table (which named `function.macro` and
/// `function.builtin`, not C's own `function.special`).
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
///
/// The non-obvious mappings, each a real judgement call rather than a mechanical rename:
///
/// - **`constructor` -> `Constructor`, its own real bucket as of GitHub issue #183** (was folded
///   into `Type` before). All four grammars use `@constructor` for their own "identifier that
///   starts with a capital letter" heuristic (`tree-sitter-rust`'s own comment calls these "enum
///   constructors ... either that, or struct names") - a real capture rule, but a distinct
///   grammar-level concept from a type name used elsewhere. `theme::syntax::CONSTRUCTOR` keeps
///   `Type`'s own colour, so this is a classification-precision improvement, not a visual change,
///   see [`HighlightKind::Constructor`]'s own docs. Note this is *not* what makes Python's
///   `class Foo:` come out right - `@constructor`'s `^[A-Z]` guard makes it unreliable for exactly
///   that, which is [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]'s fourth rule's whole reason for existing.
/// - **`tag` -> `Tag`, a real, dedicated bucket now (was folded into `Type` before this issue).**
///   `tree-sitter-javascript`'s JSX query captures a *lowercase* JSX element name (`<div>`) as
///   `@tag`, while an uppercase one (`<Foo>`) is already `@constructor`/`@type`. `theme::syntax::
///   TAG` is a direct alias of `theme::syntax::TYPE` (see that module's docs), so the two still
///   *render* identically - this is a classification-precision improvement, not a visual change -
///   which is why `code_view` test module's `tsx_jsx_element_names_are_classified_as_types` needed
///   renaming and its `"div"` assertion updating from `HighlightKind::Type` to `HighlightKind::Tag`
///   (its `"Badge"` assertion, reached through `@type`/`@constructor` rather than `@tag`, is
///   unaffected).
/// - **`variable.builtin` -> `VariableBuiltin`.** The bucket the replaced six-colour design table
///   called "literal/self", now its own dedicated variant rather than folded into a general
///   `Literal` bucket - `theme::syntax::VARIABLE_BUILTIN` keeps that original colour value (see
///   that module's docs). Rust's `self` reaches it via the real grammar's own
///   `(self) @variable.builtin` rule; TypeScript's `this`/`super` and JavaScript's own blanket
///   built-in-identifier rule land here too, matching Rust's and Python's `self` - the one
///   deliberate cross-language reclassification this app's original migration made (TypeScript's
///   `this`/`super` used to be plain `Keyword`).
/// - **`escape`/`string.escape` -> `StringEscape`, and `number`/`constant`/`constant.builtin` ->
///   their own real buckets - not `Literal`.** The six-bucket original design folded numbers,
///   strings, escapes, constants and `self` all into one `Literal` colour; this issue's whole point
///   is to stop doing that. See `theme::syntax`'s own module docs for exactly which of these five
///   keep the old `Literal` hex value (as a real, deliberate fallback-chain alias) versus which
///   ([`String`](HighlightKind::String)/[`StringEscape`](HighlightKind::StringEscape)) get a
///   genuinely new, distinct colour.
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
///
/// 1. **Python's word operators.** `tree-sitter-python`'s bundled query puts `and`/`or`/`not`/
///    `in`/`is` (and the two-word `not in`/`is not`) in its big `@operator` list, alongside `+`,
///    `==` and friends. This app has no operator bucket - `@operator` is deliberately unrecognized
///    and resolves to `Text` - so without this they would render as plain text, while the replaced
///    `PYTHON_KEYWORDS` table listed all five as real keywords. Promoting only the *word*
///    operators (never the symbolic ones, which really are punctuation by this app's colour table)
///    keeps Python's keyword colouring exactly as complete as it was.
/// 2. **`self`/`cls`.** `tree-sitter-python`'s bundled query has no rule for either; both arrive
///    as a plain `(identifier) @variable`, indistinguishable from any other name. The replaced
///    implementation carried a whole `Lexicon::literal_identifier_texts` field, and a long comment
///    justifying it, for the single purpose of giving Python's `self` the same treatment Rust's
///    gets. `@variable.builtin` is the real capture name Rust's own bundled query uses for exactly
///    this, so matching it here keeps the two languages consistent through the standard
///    vocabulary instead of a bespoke side table. `cls` is included because it plays the identical
///    role in a `@classmethod`, and the replaced code's omission of it was an accident of only
///    ever having been written with `self` in mind.
/// 3. **Compound type annotations.** `tree-sitter-python`'s bundled rule is
///    `(type (identifier) @type)` - it only fires when the annotation is a bare identifier that is
///    a *direct* child of the `type` node. A real annotation usually is not: `dict[str, int]`
///    parses as `(type (generic_type (identifier) (type_parameter (type (identifier)) (type
///    (identifier)))))`, and `pathlib.Path` as `(type (attribute object: (identifier) attribute:
///    (identifier)))` (both shapes read off a real parse via `tree_sitter::Node::to_sexp`, not
///    assumed - `dict`/`list`'s own base-name node and `pathlib`'s own object node sit *outside*
///    any per-identifier `(type (identifier) ...)` wrapper, while each of `dict[str, int]`'s own
///    `str`/`int` type-parameter identifiers is individually re-wrapped in its own nested `(type
///    (identifier))` and so already matches the bundled rule directly). Every one of those
///    non-wrapped identifiers rendered as plain text, where the replaced implementation classified
///    the whole `type` node as `Type` - a real, measured regression across ~620 bytes of the Python
///    files checked. Capturing `(type) @type` itself (this rule) restores that whole-node
///    behaviour for any byte the two more specific rules below don't otherwise cover.
///
///    That whole-node capture is not, on its own, enough once GitHub issue #31 registers a real
///    `"variable"`/`"property"` bucket, though - a real, second-order regression an audit of this
///    issue's own test suite caught. `tree-sitter-python`'s own blanket `(identifier) @variable`
///    rule (line 3 of its bundled query) captures `dict`/`list`/`pathlib` too, since each is
///    genuinely, structurally an `(identifier)` node; nested *inside* the whole `type` node this
///    rule's own `@type` capture covers, the inner `@variable` capture wins (nesting: the innermost
///    open highlight always wins - see [`highlight_with`]'s own docs) and silently downgrades
///    `dict`/`list`/`pathlib` from `Type` back to `Variable`. The two rules directly below close
///    that gap the same way rule 5 closes the method-call one: by capturing the *exact same*
///    `dict`/`list`/`pathlib` identifier nodes themselves (not a wrapping parent) as `@type`, so
///    the tie is now between two captures of the *same* node rather than parent-vs-child nesting -
///    and, being the textually later match, `@type` wins.
///
///    Note this machinery does *not* drag literals inside an annotation along with it: a `None`
///    return type is an inner `(none) @constant.builtin` node, and nesting means that inner capture
///    still wins over any of this rule's own outer `@type`, so `None` renders as `ConstantBuiltin`
///    - consistent with every other `None` in the file, not a leftover special case.
/// 4. **Class names.** `tree-sitter-python` has no `class_definition name:` rule; a class name
///    reaches a bucket only via the query's two casing heuristics, and *neither is reliable*.
///    `@constructor` requires `^[A-Z]`, so `class _Pickler:` and `class socket:` - a leading
///    underscore and a lowercase name, both pervasive real Python - matched nothing and rendered
///    as plain text. Worse, the `@constant` rule (`^[A-Z][A-Z_]*$`) fires *later* than
///    `@constructor` and therefore wins, so an all-caps class like `class FTP:` came out
///    `Literal`. The replaced implementation classified all four shapes `Type` via a dedicated
///    table field. Capturing the name node directly restores that unconditionally, independent of
///    casing. (Found by an adversarial re-audit against the CPython standard library after an
///    earlier corpus, which happened to contain only conventionally-capitalised class names,
///    showed nothing.)
/// 5. **Method calls, and `cls(...)`.** Two ordering casualties, both fixed by these rules coming
///    last. `tree-sitter-python` captures `(call function: (attribute attribute: (identifier)
///    @function.method))` *before* its blanket `(attribute attribute: (identifier) @property)`,
///    and last-pattern-wins means `@property` - which this app does not recognise - takes the
///    node, so `obj.method()` rendered as plain text while Rust and TypeScript both coloured the
///    equivalent call. Restating the method-call rule last genuinely closes that gap rather than
///    just claiming to. Separately, rule 2's `self`/`cls` match is itself a later pattern than the
///    bundled `(call function: (identifier) @function)`, so it had captured the *callee* of a real
///    `cls(...)` construction and turned it from `Function` into `Literal`; restating the plain
///    call rule after it puts that back.
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
///
/// [`colorize_bracket_pairs`] colours brackets the *grammar* identified, which is precisely what
/// keeps a `{` inside a string or comment out of it. That only works if the grammar's own query
/// actually names its bracket tokens - and `tree-sitter-python`, `-go`, `-json` and `-c` do not
/// (verified by reading each crate's own bundled `queries/highlights.scm` on disk: Python's
/// mentions `punctuation` only for `@punctuation.delimiter`/`.special`, and Go's, JSON's and C's
/// not at all). Before this, those four languages emitted **no**
/// [`HighlightKind::PunctuationBracket`] span whatsoever - a real pre-existing gap, not something
/// bracket colouring introduced, and the reason bracket colouring would otherwise have silently
/// done nothing in half of this app's supported languages.
///
/// Each list below is per-grammar rather than one shared constant because these are *anonymous
/// token* patterns: naming a token a grammar doesn't have is a query **compile** error, not a
/// pattern that harmlessly never matches, and JSON genuinely has no `(`/`)` token at all.
/// (`every_real_grammar_config_compiles` is what would catch getting that wrong.)
///
/// Matching an anonymous token is also exactly why this stays safe: a `(` inside a string literal
/// or a comment is not a token in the tree at all - it is part of the string/comment node's own
/// text - so it cannot match, and needs no exclusion rule of its own. This is the same blanket
/// shape `tree-sitter-rust`'s own bundled query already uses (`"(" @punctuation.bracket`, ...),
/// not a new idea.
///
/// Appended last, so the last-pattern-wins rule this module's docs describe means these win over
/// any earlier pattern that had captured the same token under another name.
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
///
/// ## Why these have to exist at all
///
/// Not one of the bundled grammar queries distinguishes the two. `tree-sitter-rust`'s own
/// `queries/highlights.scm` captures a call as `function: (identifier) @function` (line 43) and a
/// definition as `(function_item (identifier) @function)` (line 67) - the *same* capture name.
/// Python, JavaScript, TypeScript, Go and C all do the same. So "colour the definition, leave the
/// call at plain foreground" is not a palette change: it needs new query rules, which is what these
/// are.
///
/// The design principle behind wanting it is `theme::syntax`' own module docs' "what earns a
/// colour" section: a definition site is rare and tells you where a name *comes from*. One revision
/// of the palette used that to hold call sites at plain foreground entirely; that was rejected on
/// the rendered result and walked back, so both are coloured now and the distinction is carried by
/// hue instead - calls on the blue at H 250, definitions on the violet-blue at H 285. Either way
/// these rules are what makes the distinction *expressible*, and without them the palette has
/// nothing to attach it to.
///
/// ## How they win, and how they fail safe
///
/// Appended after the bundled query, so the "last matching pattern wins" rule cited on
/// [`PYTHON_HIGHLIGHTS_SUPPLEMENT`] makes them override the bundled `@function`. Verified by
/// execution against the real grammars, not assumed: with these appended, `fn alpha()` classifies
/// as `function.definition` while the call `beta()` stays `function`.
///
/// Verified the other way too: if `"function.definition"` were ever dropped from
/// [`HIGHLIGHT_NAMES`], `HighlightConfiguration::configure` leaves it unrecognized and the token
/// falls back to the `@function` its dotted parent still matches - so a half-finished change
/// degrades to the old colour rather than to unstyled text.
///
/// Every node kind and field name below was checked against that grammar's own real
/// `src/node-types.json` under `~/.cargo/registry/src/`, and each pattern was compiled and run.
/// Real supplement **prepended** to `tree-sitter-rust`'s own bundled query, giving Rust the blanket
/// `(identifier) @variable` rule that every other language here already has.
///
/// ## The bug this fixes, which nothing else could have
///
/// `tree-sitter-rust`'s bundled `queries/highlights.scm` has **no** blanket identifier rule - its
/// only `@variable`-family pattern is `(parameter (identifier) @variable.parameter)` at line 96.
/// `tree-sitter-python` has `(identifier) @variable` at line 3, `-go` at line 26, `-c` at line 1.
/// Rust was the sole outlier, so a plain local (`let child = ...`, and every later use of it)
/// classified as [`HighlightKind::Text`] rather than [`HighlightKind::Variable`].
///
/// That made `theme::syntax::VARIABLE` **unreachable in Rust source**. Whatever colour that token
/// held, a Rust local rendered as plain foreground - which is exactly the "most of the text is just
/// white" the maintainer reported twice, in this app's own primary language, and which no amount of
/// palette work could have fixed. It was found by taking a screenshot after a palette change and
/// diffing it against the previous one: **zero pixels** in the code area moved.
///
/// ## Why prepended rather than appended
///
/// The opposite of the definition-site supplements below, and deliberately. The rule cited on
/// [`PYTHON_HIGHLIGHTS_SUPPLEMENT`] is that the **last** matching pattern wins, so a blanket rule
/// appended last would override every specific classification in the file - functions, constants,
/// types would all collapse to `variable`. Prepending makes it the *fallback* instead: every later,
/// more specific pattern still wins, and only identifiers nothing else claims come out as
/// variables. That is precisely the position Python's and Go's own queries put their blanket rule
/// in, so this makes Rust consistent with them rather than special.
const RUST_VARIABLE_PREFIX: &str = r#"
(identifier) @variable
"#;

/// Real supplement **appended** after `tree-sitter-rust`'s own query, repairing the one genuine
/// regression [`RUST_VARIABLE_PREFIX`] introduced.
///
/// The bundled query captures an attribute with `(attribute_item) @attribute` (line 156) - the
/// whole *ancestor* node, not the identifiers inside it. Before the blanket variable rule those
/// identifiers carried no capture of their own, so the ancestor's colour simply showed through.
/// Afterwards they were claimed by `@variable`, and since `fold_highlight_events` resolves each
/// byte to its **innermost** open highlight, the leaf beat the ancestor and `#[cfg(all(test,
/// unix))]` started rendering `cfg`/`all`/`test`/`unix` as variables.
///
/// Caught by diffing a screenshot against the previous one and noticing 65 pixels going the wrong
/// way (`ATTRIBUTE`'s amber to `VARIABLE`'s rose) alongside the 231 going the right way. Query
/// order alone could not have fixed it: pattern order decides which pattern claims a given *node*,
/// and these are two different nodes at different depths. Re-asserting `@attribute` on the leaves
/// is what actually resolves it.
///
/// `token_tree` is the node an attribute's own argument list parses as, which is why
/// `all(test, unix)` needs its own rule rather than being covered by the direct-child case - and
/// why there is one rule per nesting depth: `#[cfg(all(test, unix))]` nests a `token_tree` inside
/// a `token_tree`, so `test` and `unix` sit two levels down. Three levels are written, which
/// covers every attribute shape in this workspace.
///
/// Anchoring every one of these on `attribute` is load-bearing rather than tidy: a bare
/// `(token_tree (identifier) @attribute)` would also match a **macro invocation**'s argument list
/// (`macro_invocation` carries a `token_tree` too), and would repaint every argument of every
/// `println!`/`assert!` call as an attribute.
const RUST_ATTRIBUTE_SUPPLEMENT: &str = r#"
(attribute (identifier) @attribute)
(attribute (scoped_identifier) @attribute)
(attribute (token_tree (identifier) @attribute))
(attribute (token_tree (token_tree (identifier) @attribute)))
(attribute (token_tree (token_tree (token_tree (identifier) @attribute))))
"#;

/// Real supplement appended after `tree-sitter-rust`'s own bundled query, repairing a genuine
/// **upstream typo** that makes `theme::syntax::CONSTANT` unreachable in Rust.
///
/// `tree-sitter-rust-0.24.2/queries/highlights.scm` lines 10-11 read:
///
/// ```scheme
/// ((identifier) @constant
///  (#match? @constant "^[A-Z][A-Z\d_]+$'"))
/// ```
///
/// Note the stray apostrophe between the `$` and the closing quote - confirmed byte-for-byte with
/// `od -c` against the real file on disk, not inferred. The regex therefore demands a literal `'`
/// after end-of-input and can never match anything, so the rule is dead.
///
/// Two things go wrong as a result, and the second one survives even if upstream fixes the first:
/// line 14's `((identifier) @constructor (#match? @constructor "^[A-Z]"))` is a *later* pattern and
/// `MAX_SIZE` matches `^[A-Z]` too, so last-pattern-wins hands it to `@constructor` ->
/// [`HighlightKind::Type`]. Measured before this supplement: `const MAX_SIZE: usize = 42;`
/// classified `MAX_SIZE` as `Type` at its declaration *and* at every use.
///
/// That was invisible while the palette held constants and types at related colours. It stopped
/// being invisible when the redesign gave `syntax.constant` its own orange and `syntax.type` its
/// own gold - a Rust constant rendered in the *type* colour, colliding with the very tokens it is
/// supposed to be told apart from.
///
/// This is the same *class* of bug as [`RUST_VARIABLE_PREFIX`]: Python (`highlights.scm:8-9`),
/// JavaScript (`:54-59`) and C (`:3-4`) all reach `@constant` for the identical all-caps
/// construct; Rust is the sole outlier. Placed before [`RUST_ATTRIBUTE_SUPPLEMENT`] so an
/// all-caps identifier inside an attribute still reads as an attribute.
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
///
/// Same class of gap as [`RUST_VARIABLE_PREFIX`], found the same way: by asking, for each token
/// the redesign gives a real distinct colour, which grammars can actually emit it. Rust and
/// TypeScript could; Python and Go could not.
///
/// One rule per real parameter shape, every node kind and field checked against
/// `tree-sitter-python-0.25.0/src/node-types.json` (`parameters`' children are the `parameter`
/// supertype: bare `identifier`, `default_parameter{name}`, `typed_parameter{children}`,
/// `typed_default_parameter{name}`, `list_splat_pattern`, `dictionary_splat_pattern`) and each one
/// compiled and executed rather than assumed.
///
/// `self`/`cls` are deliberately restated **after** the parameter rules, and deliberately scoped
/// to `(parameters ...)` rather than restated blanket. Appending the parameter rules alone would
/// let them beat [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]'s earlier `variable.builtin` rule and repaint
/// every `self` in a signature as an ordinary parameter. Restating the *blanket* rule instead
/// would fix that but break something else: it would also run after that supplement's
/// `(call function: (identifier) @function)` rule and turn a real `cls(...)` construction back
/// into a self-reference - which `python_method_calls_match_rust_and_typescript` catches. Scoping
/// it to the parameter list is what satisfies both.
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
///
/// Three of these fix real gaps rather than just recolouring:
/// - `generator_function_declaration` has **no** rule in `tree-sitter-javascript`'s own bundled
///   query at all, so `function* gen()` was previously classified as a plain `@variable`.
/// - `private_property_identifier` is why the `method_definition` rule is an alternation: matching
///   only `(property_identifier)` silently misses a `#priv()` private method.
/// - the `variable_declarator` rule catches `const f = () => {}` / `const f = function () {}`,
///   which is how a large fraction of real JavaScript declares functions. It keeps working under a
///   TypeScript type annotation, because `type` is a separate field from `value`.
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
///
/// ## Shorthand properties had no capture at all
///
/// `tree-sitter-javascript-0.23.1/queries/highlights.scm` mentions `shorthand_property_identifier`
/// and `shorthand_property_identifier_pattern` exactly once, at lines 54-59, and there they are
/// guarded by an **all-caps `#match?` predicate** for `@constant`. The line-4 blanket
/// `(identifier) @variable` does not reach them - they are different node kinds. So every
/// ordinary shorthand name emitted **no highlight event whatsoever** and fell to
/// [`HighlightKind::Text`]:
///
/// ```text
/// const { alpha, beta } = config;    alpha: <none>   beta: <none>
/// const obj = { alpha, gamma: 1 };   alpha: <none>   gamma: property
/// ```
///
/// That is every object destructure (`const { data, error } = useQuery()`) and every object-literal
/// shorthand (`return { id, name, count }`) in idiomatic TypeScript - a very large fraction of a
/// real file rendering as plain foreground for the same reason Rust locals used to.
///
/// The `#not-match?` guard is load-bearing: JavaScript's own all-caps `@constant` rule is an
/// *earlier* pattern, so an unguarded blanket rule appended here would repaint `const { MAX_N } =
/// limits` from a constant to a variable. Verified both ways - `alpha` becomes `variable`, `MAX_N`
/// stays `constant`.
///
/// ## Three parameter shapes TypeScript's own rules miss
///
/// `tree-sitter-typescript-0.23.2/queries/highlights.scm:15-16` captures
/// `(required_parameter (identifier))` and `(optional_parameter (identifier))` - a **direct**
/// identifier child only. Three very common shapes therefore missed: an unparenthesized arrow
/// parameter (`items.map(x => x + 1)`, how most callbacks are written), a rest parameter
/// (`...rest: string[]`), and a destructured parameter (`function g({ lo, hi }: Range)`).
///
/// The rest-parameter rule uses the `pattern:` field rather than `name:`, which is what a real
/// parse says even though `node-types.json` lists `name: [identifier, rest_pattern]` - dumped from
/// an actual tree rather than trusted from the schema.
///
/// The object-pattern rule is placed after the shorthand rules on purpose: a destructured
/// parameter is both a shorthand property *and* a parameter, and the parameter reading is the more
/// specific one.
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
///
/// 1. **`@variable.parameter`.** `tree-sitter-go-0.25.0/queries/highlights.scm` has no
///    `@variable.parameter` pattern anywhere, so parameters fell through to its line-26 blanket
///    `(identifier) @variable`. Fields confirmed in that grammar's own `node-types.json`:
///    `parameter_declaration{name: identifier, type: _type}` and
///    `variadic_parameter_declaration{name: identifier}`.
/// 2. **`@constant`.** Go's query has only `@constant.builtin` (`true`/`false`/`nil`/`iota`) and
///    no `@constant`, because Go has no all-caps convention for upstream to key a heuristic off.
///    It does have something better: a real `const_spec` node, so the rule here is *semantic*
///    rather than a name-shape guess - `const MaxRetries = 3` is a constant because the grammar
///    says it is a const declaration, not because of how it is capitalised.
/// 3. **`@property` for composite-literal keys.** Go's `(field_identifier) @property` (line 25)
///    covers `v.Name` but not `User{Name: "x"}`, where the key parses as a plain `identifier`
///    inside a `literal_element`. Rust's `S { field: a }` and TypeScript's `{ gamma: 1 }` both
///    reach `@property` for the identical construct; Go was the outlier, so a struct literal's
///    field names rendered as ordinary locals.
///
/// Every pattern compiled and executed against the real grammar before being written down.
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
///
/// Anchoring on `function_definition` fixes that, at the cost of one pattern per pointer depth: a
/// pointer return type nests the `function_declarator` under one `pointer_declarator` per `*`
/// (`char **three(void) {}` parses as `pointer_declarator > pointer_declarator >
/// function_declarator > identifier`, confirmed from a real parse). Two levels is what is written
/// here - it covers `T *f()` and `T **f()`, i.e. essentially all real code. A three-star return
/// type would need a fourth pattern and is deliberately not chased.
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
///
/// 1. **A capitalised function declaration, and a capitalised call.** `function Badge(...)` is a
///    real function and `String(value)` is a real call, and JavaScript's query says so for both -
///    but TypeScript's capital-letter heuristic runs later and reclassifies the name as a type.
///    Restating the declaration and call rules last puts them back. This is a pure restoration:
///    the replaced implementation classified both as `Function` too. Note this deliberately does
///    *not* disturb the far more common capitalised identifier that is **not** being declared or
///    called (`const x: SchemaObject`, `new Widget()` - a `new_expression`, not a
///    `call_expression`), which keeps the real, large `Type` gain this migration brings.
/// 2. **`void`.** In `(): void`, `void` is a `predefined_type`, which TypeScript's query captures
///    as `@type.builtin`. But `void` is *also* a real JavaScript operator keyword, and the
///    anonymous `"void"` token that JavaScript's `@keyword` list matches is a **child node** of
///    that `predefined_type`. Nesting, not pattern order, decides that case - the inner node's
///    highlight wins over the enclosing one - so no amount of reordering whole-node patterns fixes
///    it; the inner token has to be captured directly. Without this, `void` was the only one of
///    TypeScript's seven `predefined_type` keywords (`number`/`string`/`boolean`/`void`/`any`/
///    `unknown`/`never`) not rendering as a type, which is an inconsistency a reader would notice.
const TYPESCRIPT_HIGHLIGHTS_SUPPLEMENT: &str = r#"
(function_declaration name: (identifier) @function)
(function_signature name: (identifier) @function)
(call_expression function: (identifier) @function)
(predefined_type "void" @type.builtin)
"#;

/// Real supplement appended after `tree-sitter-md`'s own bundled block `highlights.scm` (GitHub
/// issue #154). Two lines, and the first one is the single thing that makes real per-fence
/// language injection produce *correct* spans rather than shifted, half-missing ones.
///
/// ## Why a parent highlight over an injected range is actively harmful
///
/// `tree_sitter_highlight::Highlighter::highlight` flattens every layer into one event stream, and
/// its `HighlightEnd` events carry no identity - a consumer can only keep a stack and pop it (see
/// [`fold_highlight_events`]). That model is only sound while highlights properly nest. They do
/// not, across layers: at the same start byte the engine emits the *deeper* layer's start first
/// (`sort_key` orders on `-depth`, `highlight.rs:770-800`), so a shallower highlight spanning the
/// whole injected range gets pushed **on top of** the injected layer's own first highlight, and
/// then the injected highlight's `HighlightEnd` pops the wrong entry.
///
/// That is not hypothetical - it is exactly what a ` ```rust ` fence produced before this
/// supplement existed, measured directly off the real event stream: `fn` came out
/// [`Text`](HighlightKind::Text) (shadowed by the block grammar's own `(code_fence_content)
/// @none`, which this app used to register as a real `Text` bucket) while the rest of the line
/// came out `Keyword` (the mis-popped stack). Both symptoms disappear together once no parent
/// highlight is left open over the fence's content.
///
/// 1. **`(fenced_code_block) @none`** cancels the bundled query's own `[(link_title)
///    (indented_code_block) (fenced_code_block)] @text.literal` for fenced blocks only, by the
///    "last matching pattern wins" rule cited on [`PYTHON_HIGHLIGHTS_SUPPLEMENT`].
///    `"none"` is deliberately absent from [`HIGHLIGHT_NAMES`], so it resolves to *no highlight at
///    all* rather than to a `Text` one - `HighlightConfiguration::configure` leaves an unrecognized
///    capture's `highlight_indices` entry `None`, and `next()` emits no start event for it
///    (`highlight.rs:1058-1066`). The bundled `(code_fence_content) @none` now resolves the same
///    way, for free. Indented code blocks and link titles keep their `@text.literal` colour; only
///    fenced blocks, the ones that can carry an injection, are cleared.
/// 2. **`(info_string) @text.literal`** puts back the one visible thing step 1 would otherwise
///    take away: the language tag after the opening ` ``` ` rendered as
///    [`String`](HighlightKind::String) before this change (it inherited the fence's own
///    `@text.literal`), and it still does. Restating the *same* recognized name keeps that exactly
///    as it was rather than picking a new colour for it.
const MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT: &str = r#"
(fenced_code_block) @none
(info_string) @text.literal
"#;

/// The real, composed highlights query source for `grammar`, built from the grammar crates' own
/// published `queries/*.scm` files (exposed by each crate as a `&'static str` constant, so nothing
/// here reads from disk or vendors a copy of a query file).
///
/// Rust and Python are one file each. **TypeScript and TSX are not**, and this is the single
/// least obvious fact in this module: `tree-sitter-typescript`'s own `HIGHLIGHTS_QUERY` is a
/// 35-line *supplement*, not a whole highlighting query - it defines types, type-argument
/// brackets, parameters and the TypeScript-only keywords, and nothing else. It contains no rule
/// for strings, comments, numbers, function calls or any of the JavaScript keywords, because
/// upstream expects it to be concatenated onto `tree-sitter-javascript`'s query the way the
/// `tree-sitter` CLI's own language inheritance does. Using it alone would compile and run
/// perfectly happily while silently rendering every TypeScript comment, string and function call
/// as plain text - which is exactly why `tree-sitter-javascript` is a real dependency of this
/// crate despite this app never opening a `.js` file with a JavaScript-only grammar.
///
/// Order matters and is deliberate, per the "last matching pattern wins" rule cited on
/// [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]: JavaScript's base rules go first so that TypeScript's own
/// `(type_identifier) @type` and its capitalised-identifier rule can override JavaScript's
/// blanket `(identifier) @variable` for the same node. The JSX query sits between them, and only
/// for [`Grammar::Tsx`] - it references `jsx_opening_element` and friends, which genuinely do not
/// exist in the plain TypeScript grammar, so including it there would fail query compilation
/// outright rather than degrade quietly.
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
///
/// 1. **Missing the required outer-paren wrapping.** A pattern's own `#set!` predicates must be
///    grouped in the *same* top-level S-expression as the node pattern they apply to - `((inline)
///    @injection.content (#set! ...))`, not `(inline) @injection.content (#set! ...)` - or
///    tree-sitter parses each `#set!` as its own disconnected top-level pattern, and
///    `injection_for_match` never sees the property at all. `tree-sitter-rust`'s own
///    `queries/injections.scm` uses the correct, wrapped form; `tree-sitter-md`'s own file does
///    not, for its `(inline)` pattern.
/// 2. **Missing `#set! injection.include-children`.** `tree-sitter-highlight` excludes an
///    `@injection.content` node's own *children* from the reparsed range by default (its own real
///    design: "for other injections, only the content node's own content is reparsed"). The block
///    grammar's own `(inline)` node is not the pure leaf it looks like - it carries real, unnamed
///    child tokens for a handful of characters (`*`, `.`, ...) it scans for its own block-level
///    heuristics - so without this flag, exactly the delimiter characters `**bold**`/`*italic*`/...
///    depend on get excluded from what `Grammar::MarkdownInline` ever sees, and every inline
///    capture silently fails to fire (confirmed directly: the exact same source highlights
///    correctly once this one predicate is added, nothing else changed).
///
/// ## Real cross-language injection (GitHub issue #154)
///
/// The three patterns after the `(inline)` one are the feature this constant's own docs used to
/// describe as "a separate, larger feature ... left for a future revision": a fenced code block's
/// content is now genuinely reparsed with the language its info string names. Each is copied
/// **verbatim** from `tree-sitter-md`'s own bundled `tree-sitter-markdown/queries/injections.scm`
/// (unlike the `(inline)` pattern above, whose two real bugs are described in detail below, these
/// three are correct as shipped and needed no repair), and each was checked against the block
/// grammar's own `src/node-types.json` rather than guessed - `(fenced_code_block)` really does
/// carry an `(info_string)` whose own `(language)` child holds the fence tag, and a separate
/// `(code_fence_content)` child holding the body.
///
/// Two details of that fenced-block pattern are load-bearing and easy to get wrong:
///
/// - The language comes from an **`@injection.language` capture**, not a `#set!` predicate.
///   `tree-sitter-highlight` reads the captured node's own source text as the language name
///   (`injection_for_match`, `highlight.rs:1262-1269`, read directly), which is what makes an
///   arbitrary, author-written fence tag resolvable at all. [`Grammar::for_injection_name`] is
///   what that text is then matched against.
/// - It deliberately does **not** set `injection.include-children`, the exact opposite of the
///   `(inline)` pattern above. `(code_fence_content)`'s only real child type is
///   `(block_continuation)` - the leading `> ` / list-indent prefix repeated on each line of a
///   fence nested inside a block quote or list item. Excluding children (the engine's default) is
///   precisely what keeps those prefixes out of the reparsed range, so the injected grammar sees
///   the code and not the markdown scaffolding around it.
///
/// `(html_block)` is the same feature's block-level HTML half, and the `(minus_metadata)`/
/// `(plus_metadata)` patterns are real YAML/TOML frontmatter, all three now resolvable because
/// this app has those grammars. The bundled file's remaining pattern - the `(document . (section
/// . (thematic_break) ...))` frontmatter fallback - is deliberately left out: it is a
/// heuristic for grammars built *without* the frontmatter extension, and this crate's
/// `tree-sitter-md` build does have `(minus_metadata)`/`(plus_metadata)`, so including both would
/// mean two patterns racing for the same bytes.
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
///
/// Both patterns are correctly paren-wrapped upstream, so they need none of the first repair the
/// block file's `(inline)` pattern needs - but they need the **second** one, for exactly the same
/// underlying reason, and without it neither fires at all. `(html_tag)` looks like a leaf and is
/// not: read off a real parse of `Inline <span class="i">tag</span> here.`, its `<span class="i">`
/// node carries unnamed child tokens for `<`, both `"`s and `>`. `tree-sitter-highlight` excludes
/// an `@injection.content` node's children from the injected range by default, so what survives
/// is a shredded, non-contiguous handful of bytes - and `intersect_ranges` can hand back nothing
/// at all, which the engine silently drops (`highlight.rs:917`, an `if !ranges.is_empty()` with no
/// else). That silent-drop path is precisely what this constant produced before
/// `injection.include-children` was added: the callback was called with `"html"`, resolved
/// correctly, and still highlighted nothing.
///
/// `"latex"` is kept rather than stripped: this app has no LaTeX grammar, so
/// [`Grammar::for_injection_name`] returns `None` for it and `tree-sitter-highlight` simply
/// creates no layer - the same honest no-op an unrecognized fence tag gets. Keeping it means the
/// day a LaTeX grammar is added, nothing here needs editing.
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
///
/// The locals query is deliberately always empty, and the injection query is empty for every
/// grammar except the three that genuinely have one - both real decisions, not gaps left to fill
/// in later:
///
/// - **Injections.** Only Markdown (block), Markdown (inline) and HTML get one. TSX parses JSX as
///   part of its own single grammar rather than as an injected language, and the only injections
///   the Rust and Python query files describe are things like SQL-in-a-string-literal, which this
///   app has no second grammar to inject *into* anyway. None of the five GitHub-issue-#32 grammars
///   bundles a real `queries/injections.scm` at all - verified directly against each one's own
///   `queries/` directory, not assumed - and neither does `tree-sitter-css`.
///   `Markdown` was the original exception (GitHub issue #104): its own bundled block grammar
///   genuinely never parses prose content itself, only the injected `Grammar::MarkdownInline` -
///   see [`MARKDOWN_INJECTION_QUERY`]'s own docs. GitHub issue #154 added the other two, both
///   real and both now backed by grammars that actually exist here: `MarkdownInline`'s inline
///   `(html_tag)` (see [`MARKDOWN_INLINE_INJECTION_QUERY`]), and `tree-sitter-html`'s own bundled
///   `INJECTIONS_QUERY`, used verbatim - it routes a `<style>` element's `(raw_text)` to
///   `"css"` and a `<script>` element's to `"javascript"`, and
///   [`Grammar::for_injection_name`] resolves both (the latter through the fence-alias table's
///   `"javascript" -> "js"` entry, landing on the plain TypeScript grammar `.js` files already
///   use).
/// - **Locals.** `tree-sitter-typescript` ships a `locals.scm`, and feeding it would switch on
///   `tree-sitter-highlight`'s local-variable tracking, which exists to let a query colour a
///   variable *reference* like its *definition*. This app's buckets do not distinguish variables
///   at all - every plain identifier is `Text` - so that machinery has nothing to express here,
///   and enabling it would only add per-parse scope-tracking cost for an outcome that cannot
///   differ.
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
///
/// Caching at all is not a micro-optimisation: building one configuration means compiling a
/// several-hundred-pattern `tree_sitter::Query` from source text, which costs far more than
/// parsing a whole file (tens of milliseconds each, measured) and would otherwise be paid again on
/// every keystroke's debounced re-highlight.
///
/// Caching them *separately* is the part that was gotten wrong first and is worth recording. A
/// single `OnceLock<HashMap<..>>` populated in one pass reads naturally and is what this started
/// as - but it makes the first request for *any* grammar compile *every one of them*. That cost
/// is not
/// hypothetical and it does not always land on a background thread: `CodeView::Diff` is the
/// default view, and `crate::code_surface`'s `ensure_diff_highlight_cache` calls into here
/// synchronously on the main thread, so opening the first `.rs` diff of an agent paid the
/// TypeScript, TSX *and* Python query-compile cost for grammars that diff would never use. Per-
/// grammar slots mean a `.rs` file pays for Rust only.
///
/// A `HighlightConfiguration` is immutable once configured, so a shared `&'static` reference is
/// all any caller needs; the per-call mutable state lives in [`tree_sitter_highlight::Highlighter`],
/// which is cheap to construct and is created fresh per call.
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
///
/// ## Why this exists (GitHub issue: bracket pairs matched across fenced code blocks)
///
/// [`colorize_bracket_pairs`] is a stack matcher over the whole file's bracket tokens. Before this,
/// it ran one single stack over every bracket in the document, which is wrong the moment a file
/// contains more than one *independent* body of code. A markdown file with two ` ```rust ` fences
/// is exactly that, and the bug it produced was real and reproducible: an unclosed `{` in the first
/// fence paired with a `}` in the second, painting two brackets in two different code blocks - in
/// two different languages, potentially - as one matched pair, and shifting every depth in the
/// second fence by the first fence's leftover stack.
///
/// `tree_sitter_highlight::HighlightEvent` carries no layer identity (`Source`/`HighlightStart`/
/// `HighlightEnd` only - `tree-sitter-highlight-0.26.9/src/highlight.rs:106-110`, read directly),
/// and the engine flattens every injected layer into one event stream, so [`fold_highlight_events`]
/// genuinely cannot tell which layer a byte came from. Recovering it needs a separate look at the
/// tree, which is what this does.
///
/// ## Cost, stated plainly
///
/// This is a real second parse of `source`. It runs **only** for a grammar that actually has an
/// injection query - Markdown, MarkdownInline and HTML, three of thirteen - and returns immediately
/// with no parse at all for the other ten, so opening a Rust or TypeScript file costs exactly what
/// it did before. For the three that do pay it, one extra parse is the honest price of not
/// mis-pairing brackets across code blocks, and it is still amortised over a real content change
/// rather than a frame.
///
/// Deliberately **one level deep**, not recursive: an injected region's own nested injections
/// (HTML inside a markdown fence, CSS inside that HTML) are not sub-divided further. That is a real
/// limitation rather than an oversight - it keeps this to a single parse, and one level is what
/// separates the case that actually misbehaves (sibling fences). A bracket pair spanning two
/// *nested* injected regions is not reachable in practice, since the inner region is entirely
/// contained in the outer one.
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
///
/// `scopes` is sorted by start byte, and the ranges are either disjoint or properly nested (they
/// are syntax-node ranges), so the *last* range that starts at or before `start` and still contains
/// it is the innermost one.
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
///
/// A compile failure here is not expected and is not silently tolerated as an acceptable state:
/// the query sources are compile-time constants, so a failure is fully deterministic and is caught
/// by this module's own `every_real_grammar_config_compiles` test. `None` exists so that a
/// hypothetical failure degrades to honest uncoloured text at runtime instead of panicking a
/// running editor, and it is logged at `error` level rather than passing quietly. It is also
/// cached like any other outcome, so a failing grammar is not re-compiled on every keystroke.
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
///
/// It is a plain function rather than a closure so that all three real injection sources - a
/// markdown fence's info string, a markdown `(html_block)`, an HTML `<style>`/`<script>` element -
/// go through exactly one resolution path. See [`Grammar::for_injection_name`] for what that path
/// actually matches against.
///
/// Recursion is real and terminates for a real, structural reason rather than a depth cap: an
/// injected range is always strictly inside its host node, so a ` ```markdown ` fence inside a
/// markdown document reparses strictly less text each time, and a zero-length range is dropped by
/// the engine before a layer is even built. The [`HIGHLIGHT_CONFIGS`] `OnceLock` slots cannot
/// deadlock on that recursion either: this callback only ever runs *during*
/// `Highlighter::highlight`, which is after the outer grammar's own `get_or_init` has already
/// returned, so no slot is ever re-entered while it is still initialising.
///
/// The `'a` return lifetime is deliberate and load-bearing, not noise: `Highlighter::highlight`'s
/// own signature is `impl FnMut(&str) -> Option<&'a HighlightConfiguration> + 'a` where that same
/// `'a` also bounds the *source* slice and the `Highlighter` itself
/// (`tree-sitter-highlight-0.26.9/src/highlight.rs:284-290`). A plain `fn(&str) ->
/// Option<&'static HighlightConfiguration>` therefore forces `'a == 'static` and makes the whole
/// call require a `'static` source - a real borrow-check error, not a style preference. Being
/// generic here lets the caller instantiate `'a` at whatever the borrow actually is; the value
/// returned is still always a `&'static` one out of [`HIGHLIGHT_CONFIGS`].
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
///
/// Note that `tree-sitter-javascript` *is* nonetheless a real dependency of this crate - for its
/// query file, not its grammar. See [`highlight_query_for`] for why.
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
///
/// Unlike most wrappers here this one really does reach two further grammars: `tree-sitter-html`'s
/// own bundled `INJECTIONS_QUERY` is wired (see [`build_highlight_config`]), so a `<style>`
/// element's body is genuinely parsed as CSS and a `<script>` element's body as
/// JavaScript/TypeScript, through [`injection_config`].
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
///
/// [`injection_config`] is passed as the real injection callback for *every* grammar, not only the
/// ones that have an injection query (GitHub issue #154). That is not speculative machinery: a
/// grammar built with an empty injection query has no injection capture indices at all, so the
/// callback is never reached for it, and the three grammars that do have one
/// ([`build_highlight_config`]) all resolve through the same single path.
///
/// The event stream is a flat, in-order sequence of `HighlightStart`/`Source`/`HighlightEnd`, and
/// `HighlightStart`s **nest** - a Rust `\n` escape inside a string literal arrives as a real
/// `escape` highlight opened while the enclosing `string` one is still open. The innermost open
/// highlight is therefore the one that wins, which is what the stack below tracks; the engine has
/// already split the `Source` events at every boundary, so each one falls entirely under exactly
/// one innermost highlight and no span produced here can ever overlap another.
///
/// Every byte of `source` is covered, including whitespace between tokens (as explicit
/// [`HighlightKind::Text`]). That is a real difference from the hand-rolled walker this replaced,
/// which emitted spans only for leaf nodes and left the gaps to [`build_lines`]; the rendered
/// result is identical either way, since `build_lines` fills any gap with exactly that same
/// `Text`, but it means the span list here is gapless by construction rather than by downstream
/// repair.
///
/// Errors are not expected to be reachable: `Error::Cancelled` requires the cancellation flag this
/// passes `None` for, and `Error::InvalidLanguage` requires a language the config was not built
/// with. If one does occur mid-stream, the spans accumulated so far are returned rather than
/// discarded - partial real highlighting of a file's earlier lines is strictly better for the
/// reader than dropping the whole file back to plain text, and matches the same best-effort
/// posture [`highlight_block`] already documents for partial input.
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
///
/// This used to carry its own bespoke, `"markdown_inline"`-only callback and so could not be
/// expressed as a plain [`highlight_with`] call. GitHub issue #154 replaced that with the shared
/// [`injection_config`] resolver, which is what makes a ` ```html ` / ` ```rust ` fence's *content*
/// really reach that language's own grammar - so this is now the same one-line wrapper every other
/// language here is, and the difference lives entirely in `Markdown`'s injection query.
pub fn highlight_markdown(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, Grammar::Markdown)
}

/// The one real event-folding path every highlighting entry point in this module funnels into:
/// collapses a [`tree_sitter_highlight::Highlighter::highlight`] event stream down into
/// this app's [`HighlightKind`] buckets.
///
/// The event stream is a flat, in-order sequence of `HighlightStart`/`Source`/`HighlightEnd`, and
/// `HighlightStart`s **nest** - a Rust `\n` escape inside a string literal arrives as a real
/// `escape` highlight opened while the enclosing `string` one is still open. The innermost open
/// highlight is therefore the one that wins, which is what the stack below tracks; the engine has
/// already split the `Source` events at every boundary, so each one falls entirely under exactly
/// one innermost highlight and no span produced here can ever overlap another. An injected layer
/// (`highlight_markdown`'s own inline recursion) is no different from this app's own perspective -
/// `tree-sitter-highlight` already interleaves an injected layer's events into this exact same
/// flat stream in source-byte order, so this folding logic needs no injection-awareness of its
/// own.
///
/// Every byte of `source` is covered, including whitespace between tokens (as explicit
/// [`HighlightKind::Text`]). That is a real difference from the hand-rolled walker this replaced,
/// which emitted spans only for leaf nodes and left the gaps to [`build_lines`]; the rendered
/// result is identical either way, since `build_lines` fills any gap with exactly that same
/// `Text`, but it means the span list here is gapless by construction rather than by downstream
/// repair.
///
/// Errors are not expected to be reachable: `Error::Cancelled` requires the cancellation flag this
/// passes `None` for, and `Error::InvalidLanguage` requires a language the config was not built
/// with. If one does occur mid-stream, the spans accumulated so far are returned rather than
/// discarded - partial real highlighting of a file's earlier lines is strictly better for the
/// reader than dropping the whole file back to plain text, and matches the same best-effort
/// posture [`highlight_block`] already documents for partial input.
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
///
/// - A **block tag** (`@param foo`, `@returns`, `@example`) - `@` immediately followed by one or
///   more ASCII letters, with the byte immediately before the `@` (if any) not itself an
///   identifier byte. `foo@bar.com` never matches this way: its `@` sits directly after `o`, while
///   a real block tag's `@` always starts a fresh word (line start, or right after whitespace/`*`).
/// - An **inline tag** (`{@link Foo#bar}`, `{@see ...}`) - the whole `{@word ...}` span, brace to
///   brace, so a reader-visible inline reference reads as one unit rather than just its own `@word`
///   prefix. A `{` with no matching `}` before the text ends is left alone entirely (a real,
///   well-formed inline tag is always closed) rather than swallowing the rest of the comment.
///
/// Deliberately a plain text scan, not a real grammar parse - no bundled tree-sitter grammar this
/// app ships parses *inside* a comment/doc-string body at all (see [`HighlightKind::CommentDocTag`]'s
/// own docs), and a hand-authored JSDoc tag vocabulary is genuinely unbounded (real projects invent
/// their own `@internal`/`@beta`-style tags constantly) - matching "looks like a tag" structurally
/// is what stays correct for all of them, at the cost of occasionally accenting a real `@` used
/// some other way inside prose (rare in practice, and never mis-renders anything - it just gets an
/// accent colour it didn't strictly need).
///
/// Byte-index (not `char`) ranges, but always land on real `char` boundaries: every marker this
/// scan looks for (`{`, `@`, `}`, ASCII letters) is itself a single ASCII byte, and an ASCII byte
/// can never be a UTF-8 continuation byte, so a `text[range]` slice on the result never panics.
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
///
/// Language-agnostic by construction: the `/** */` shape check only ever matches a real C-style
/// block comment (Rust, TypeScript, JavaScript, Go, ...) and simply never fires for a `#`-style
/// Python comment or any other shape - no per-language branch needed. Worth noting as a side
/// effect rather than a bug: this also promotes a Rust `/** ... */` block doc comment, which
/// `tree-sitter-rust`'s own bundled query captures only the `///` line-comment shape of (see
/// [`HighlightKind::CommentDoc`]'s own docs) - a real, previously-uncovered gap this closes for
/// free.
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
///
/// Everything a `tree-sitter-highlight` query itself produces is unconditional - a keyword is a
/// keyword regardless of preference. This struct is for the real classification passes this module
/// layers *on top* of that, which a user can genuinely want off. There is one so far.
///
/// [`Default`] is every option **enabled**, deliberately: each of these shipped on, so the default
/// has to reproduce current behaviour exactly. That is also what lets the plain [`load_file`] /
/// [`highlight_block`] / `EditBuffer::new` entry points stay unchanged (they delegate to their own
/// `*_with_options` sibling with `HighlightOptions::default()`), so only the handful of real
/// production call sites that can actually see a user's settings have to thread anything.
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
    ///
    /// The one real seam between "what the grammar said" and "what this user asked for". Callers
    /// that have a `Settings` to consult go through here; callers that don't get
    /// [`HighlightOptions::default`], which is every option on.
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
///
/// Pure, whole-source, and deliberately *not* a second parse. It needs no tree-sitter tree of its
/// own because [`fold_highlight_events`] has already done the hard part: the grammar's own
/// `punctuation.bracket` capture is what identifies a real bracket *token*, so a `{` written
/// inside a string literal or a comment never arrives here as `PunctuationBracket` at all - it is
/// part of a `String`/`Comment` span and is invisible to this pass by construction, not by a
/// string-skipping heuristic this function would otherwise have had to reimplement per language.
/// (`brackets_inside_strings_and_comments_are_never_coloured` pins exactly that.)
///
/// ## `<` and `>` deliberately do not participate
///
/// They genuinely do reach this function as `PunctuationBracket`: `tree-sitter-rust` captures
/// `(type_arguments "<" @punctuation.bracket ">" @punctuation.bracket)` (and the same for
/// `type_parameters`), `tree-sitter-typescript` the same for `type_arguments`, and
/// `tree-sitter-html` captures a *tag's* own `<`/`>` under that name too. They are still skipped
/// entirely - not pushed, not popped, left plain - for two real reasons rather than one:
///
/// 1. HTML would be actively wrong. `<div>` `</div>` are separate `<`/`>` pairs at the *same*
///    nesting level, not an open/close pair, so a stack matcher colours a whole HTML document as
///    one flat depth-0 ring while implying structure that isn't there.
/// 2. Rust and TypeScript would be noisy for no gain. `HashMap<String, Vec<u8>>` is already
///    unambiguous to read, and `a < b` / `->` / `=>` mean the same characters are a comparison or
///    an arrow elsewhere in the same file - the grammar tells those apart, but the reader now has
///    to, every time they see a coloured `<`.
///
/// VSCode's own bracket colourization makes the same call for the same reason.
///
/// ## What "unmatched" does
///
/// A bracket is recoloured **if and only if** it is half of a really-matched pair. Everything else
/// keeps `PunctuationBracket`'s plain-text colour, which is why that token is still deliberately
/// aliased to `syntax::TEXT` (see `theme::syntax`' own docs). Concretely:
///
/// - A closer with an empty stack, or whose shape doesn't match the innermost open bracket
///   (`([)]`'s `)`), is left plain and **does not pop**. Not popping is the conservative choice: a
///   stray closer is a local anomaly, and consuming a legitimately-open bracket for it would
///   miscolour everything after it rather than just itself.
/// - An opener still on the stack at the end of the source is left plain too. This is the common
///   mid-edit case - the moment you type `{`, that `{` is genuinely unmatched - and leaving it
///   plain until its partner exists is honest about what is actually known.
///
/// One consequence worth stating rather than hiding: an unmatched opener stays on the stack, so
/// every pair *after* it in the file is nested one deeper than it looks, and the ring is offset by
/// one from there on. Fixing that would need lookahead this pass deliberately doesn't do; the
/// pairs it colours are all still really matched, and the offset resolves itself the moment the
/// source balances again.
///
/// ## Injected regions pair independently
///
/// Matching runs one **separate stack per [`HighlightSpan::scope`]**, so a `{` in one ` ```rust `
/// fence can never pair with a `}` in the next one, and an unbalanced fence cannot shift the ring
/// for the fences after it. Before that, one global stack ran over the whole document and did
/// exactly both of those things - see [`injection_scopes`] for the real reproduction and for why
/// the region has to be recovered from a separate parse rather than read off the highlight event
/// stream.
///
/// Depth is counted *within* a scope, so every fence's outermost pair starts at ring colour 0
/// regardless of what the fences before it did, and regardless of how deeply the host document
/// nests the fence.
///
/// ## Why it re-splits spans
///
/// [`fold_highlight_events`] coalesces adjacent same-bucket spans, so `}}` or `<(` arrive as *one*
/// `PunctuationBracket` span covering several characters. Every tracked bracket therefore has to
/// be split out to its own span before it can be coloured individually - and the output is
/// re-coalesced on the way out, so a run this pass leaves entirely plain (`<>`, `))` where both
/// are unmatched) collapses back to the single span it came in as rather than inflating
/// [`build_lines`]' per-line run count.
///
/// Runs once per real content change, on whatever thread already owned that highlight - never per
/// frame: it is called from [`highlight_with`], the single funnel every `highlight_*` entry point
/// (and so every `crate::language::HighlighterFn`) goes through, whose three callers are
/// [`load_file_with_source`], `EditBuffer::new` and `crate::code_surface::editing`'s debounced
/// background re-highlight. Cost is one linear pass over a span list that was just built by a
/// parse costing orders of magnitude more.
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
///
/// This is a deliberate, honest simplification: a hunk/conflict side is highlighted on its own,
/// not as part of the true old- or new-file content, since neither is fully available from a
/// hunk/conflict block alone. `tree-sitter`'s own best-effort recovery on imperfect/partial input
/// (this module's `highlighting_invalid_rust_still_returns_a_real_non_empty_span_list`-style
/// tests) is what makes this reasonable, not a claim of perfect semantic accuracy.
///
/// Zero input lines yields an empty `Vec`, *not* [`build_lines`]'s own one-empty-line convention:
/// that convention is correct for [`load_file`] (a genuinely empty *file* still has one real,
/// empty line to show), but wrong here - a diff hunk/merge-conflict side with zero lines has zero
/// real content, and a caller driving row count off this `Vec`'s length (`crate::root::
/// merge_flow_render::AdeApp::render_conflict_columns`) must render zero rows for it, not one
/// fabricated blank row.
///
/// `options` is required rather than defaulted (unlike [`load_file`], whose plain form stays for
/// its many existing callers): every one of this function's real callers - the Diff view, the
/// Merge view, the Markdown preview - has an `&self` to read the user's own settings from, so
/// there is no call site that would legitimately want to guess.
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

    /// Finds the span whose byte range is *exactly* `text`'s first occurrence in `source`.
    ///
    /// This works for any token the highlighter actually classifies, because a classified token is
    /// bounded on both sides by differently-classified neighbours. It deliberately does **not**
    /// work for a token that comes out as plain [`HighlightKind::Text`]: [`highlight_with`]
    /// coalesces adjacent same-kind spans, so an unclassified identifier is merged into one
    /// wider `Text` run covering the surrounding whitespace and punctuation too, and has no span
    /// of its own to find. Use [`kind_at`] for those - asserting on the kind covering a byte is
    /// the meaningful question there, and it is the one the renderer itself asks.
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

    #[test]
    fn fn_keyword_is_classified_as_keyword() {
        let spans = highlight_rust(SAMPLE_RUST);
        let span = find_span(&spans, SAMPLE_RUST, "fn").expect("fn span");
        assert_eq!(span.kind, HighlightKind::Keyword);
    }

    #[test]
    fn a_string_literal_is_classified_as_string() {
        let spans = highlight_rust(SAMPLE_RUST);
        let span = find_span(&spans, SAMPLE_RUST, "\"x\"").expect("string literal span");
        assert_eq!(span.kind, HighlightKind::String);
    }

    #[test]
    fn a_function_name_is_classified_as_function() {
        let spans = highlight_rust(SAMPLE_RUST);
        let span = find_span(&spans, SAMPLE_RUST, "add").expect("function name span");
        assert_eq!(span.kind, HighlightKind::FunctionDefinition);
    }

    #[test]
    fn a_primitive_type_identifier_is_classified_as_type_builtin() {
        let spans = highlight_rust(SAMPLE_RUST);
        // "i32" appears twice (parameter type, return type); just confirm at least one
        // occurrence was classified as TypeBuiltin - `i32` is a real `(primitive_type)` node in
        // `tree-sitter-rust`'s own grammar, captured `@type.builtin`, not the plain `@type` a
        // user-defined type name would get.
        let type_spans: Vec<_> = spans
            .iter()
            .filter(|span| SAMPLE_RUST[span.start..span.end] == *"i32")
            .collect();
        assert!(!type_spans.is_empty());
        assert!(type_spans
            .iter()
            .all(|span| span.kind == HighlightKind::TypeBuiltin));
    }

    #[test]
    fn a_doc_comment_is_classified_as_comment_doc() {
        let spans = highlight_rust(SAMPLE_RUST);
        // The `line_comment` node's byte range includes its trailing newline; it's treated as
        // one span rather than recursed into, since its children are just lexical pieces, not
        // separately-colourable syntax. `///` is a real `(doc_comment)` child node, captured
        // `@comment.documentation` - its own dedicated bucket since GitHub issue #31, not the
        // plain `Comment` an ordinary `//` line gets.
        let span = find_span(&spans, SAMPLE_RUST, "/// Adds one.\n").expect("doc comment span");
        assert_eq!(span.kind, HighlightKind::CommentDoc);
    }

    #[test]
    fn self_is_classified_as_variable_builtin_not_keyword() {
        let source = "impl Foo {\n    fn bar(&self) -> i32 {\n        self.value\n    }\n}\n";
        let spans = highlight_rust(source);
        let span = find_span(&spans, source, "self").expect("self span");
        assert_eq!(span.kind, HighlightKind::VariableBuiltin);
    }

    #[test]
    fn highlighting_invalid_rust_still_returns_a_real_non_empty_span_list() {
        // Tree-sitter produces a best-effort tree for malformed input rather than failing
        // outright - confirm this doesn't panic and still classifies the keyword token present.
        let spans = highlight_rust("fn (((( broken");
        assert!(spans.iter().any(|span| span.kind == HighlightKind::Keyword));
    }

    // Real TypeScript highlighting coverage - mirrors `highlight_rust`'s own test shape above.
    // Before this fix, none of `highlight_typescript`'s real, common-case behavior had any test
    // coverage at all.

    const SAMPLE_TYPESCRIPT: &str = "/** Adds one. */\nfunction add(left: number): number {\n    const name = \"x\";\n    return left + 1;\n}\n";

    #[test]
    fn typescript_function_keyword_is_classified_as_keyword() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span = find_span(&spans, SAMPLE_TYPESCRIPT, "function").expect("function span");
        assert_eq!(span.kind, HighlightKind::Keyword);
    }

    #[test]
    fn typescript_string_literal_is_classified_as_string() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span = find_span(&spans, SAMPLE_TYPESCRIPT, "\"x\"").expect("string literal span");
        assert_eq!(span.kind, HighlightKind::String);
    }

    /// `(regex) @string.special` - its own dedicated bucket since GitHub issue #183 (previously
    /// fell through to the plain `"string"` entry, reading a regex literal as an ordinary
    /// string).
    #[test]
    fn typescript_regex_literal_is_classified_as_string_special() {
        let source = "const pattern = /^[a-z]+$/;\n";
        let spans = highlight_typescript(source, false);
        assert_eq!(
            kind_at(&spans, source, "^[a-z]+$"),
            HighlightKind::StringSpecial
        );
    }

    #[test]
    fn typescript_function_declaration_name_is_classified_as_function() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span = find_span(&spans, SAMPLE_TYPESCRIPT, "add").expect("function name span");
        assert_eq!(span.kind, HighlightKind::FunctionDefinition);
    }

    #[test]
    fn typescript_predefined_type_is_classified_as_type_builtin() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        // `number` is a real `(predefined_type)` node, captured `@type.builtin`.
        let type_spans: Vec<_> = spans
            .iter()
            .filter(|span| SAMPLE_TYPESCRIPT[span.start..span.end] == *"number")
            .collect();
        assert!(!type_spans.is_empty());
        assert!(type_spans
            .iter()
            .all(|span| span.kind == HighlightKind::TypeBuiltin));
    }

    #[test]
    fn typescript_doc_comment_is_classified_as_comment() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span =
            find_span(&spans, SAMPLE_TYPESCRIPT, "/** Adds one. */").expect("doc comment span");
        assert_eq!(span.kind, HighlightKind::Comment);
    }

    // GitHub issue #200's own doc-tag coverage. `typescript_doc_comment_is_classified_as_comment`
    // just above deliberately calls `highlight_typescript` directly - the raw grammar layer,
    // bypassing `HighlightOptions::apply` entirely (same reason `colorize_bracket_pairs`'s own
    // tests do this) - so it stays correct unchanged: `split_doc_comment_tags` only ever runs
    // inside `apply()`, which every real, live rendering path (`load_file_with_options`,
    // `highlight_block`) goes through but this one pinned raw-grammar test doesn't.

    #[test]
    fn doc_tag_ranges_finds_a_real_block_tag_preceded_by_whitespace() {
        let text = "Adds one.\n@param left the number to add to\n@returns the sum";
        let ranges: Vec<&str> = doc_tag_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(ranges, vec!["@param", "@returns"]);
    }

    #[test]
    fn doc_tag_ranges_never_matches_an_email_style_at_sign() {
        let text = "Contact foo@example.com for details.";
        assert!(
            doc_tag_ranges(text).is_empty(),
            "an `@` directly preceded by an identifier byte (the `o` in `foo`) is not a real \
             doc tag - got: {:?}",
            doc_tag_ranges(text)
        );
    }

    #[test]
    fn doc_tag_ranges_finds_a_real_inline_link_tag_brace_to_brace() {
        let text = "See {@link Foo#bar} for more.";
        let ranges: Vec<&str> = doc_tag_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(ranges, vec!["{@link Foo#bar}"]);
    }

    /// An unclosed `{@link` never matches the *inline*-tag shape (no real `}` to close it), but
    /// still degrades gracefully to the plain block-tag rule for the `@link` word itself, rather
    /// than emitting nothing at all for an honest, if malformed, doc comment.
    #[test]
    fn doc_tag_ranges_falls_back_to_a_bare_block_tag_for_an_unclosed_inline_tag() {
        let text = "See {@link Foo#bar for more.";
        let ranges: Vec<&str> = doc_tag_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(ranges, vec!["@link"]);
    }

    /// The real, full pipeline (`HighlightOptions::default().highlight(..)`, the same call
    /// [`load_file_with_options`] itself makes) - unlike `typescript_doc_comment_is_classified_as_comment`
    /// just above, this one *does* run [`split_doc_comment_tags`], and is the real, live-user-facing
    /// behavior a `.ts`/`.js` file's own doc comment actually gets: promoted from plain `Comment` to
    /// `CommentDoc`, the same bucket a Rust `///` comment already got before this issue.
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

    /// A real Rust `///` doc comment already reaches [`HighlightKind::CommentDoc`] through
    /// `tree-sitter-rust`'s own grammar capture (see `a_doc_comment_is_classified_as_comment_doc`
    /// above) - this proves [`split_doc_comment_tags`] still finds and splits out a real tag
    /// *inside* that already-correctly-classified span, not just when it's the one doing the
    /// Comment -> CommentDoc promotion itself.
    #[test]
    fn a_real_jsdoc_style_tag_inside_a_rust_doc_comment_still_gets_its_own_tag_span() {
        let source = "/// Adds one.\n///\n/// @param left the number to add to\nfn add(left: i32) -> i32 {\n    left + 1\n}\n";
        let spans = HighlightOptions::default().highlight(source, Some(highlight_rust));
        assert_eq!(
            kind_at(&spans, source, "@param"),
            HighlightKind::CommentDocTag
        );
    }

    /// The real, live-verified regression this fix addresses: a `variable_declarator`'s own
    /// `name` field collides with `function_declaration`'s `name` field in
    /// `tree-sitter-typescript`'s real grammar, and the old, parent-kind-unaware matching
    /// misclassified every `const`/`let`/`var` binding's name as a Function. `s` here must be
    /// classified `Variable` (a use/declaration of a variable, not a function) - previously
    /// asserted as plain `Text`, back when `"variable"` wasn't yet a registered highlight name at
    /// all (GitHub issue #31 registered it); `theme::syntax::VARIABLE` is still a real, direct
    /// alias of `theme::syntax::TEXT` (see that module's docs), so this is a classification-only
    /// change with no visual difference.
    #[test]
    fn typescript_const_variable_name_is_not_misclassified_as_a_function() {
        // The audit's exact reproduction. `find_span`'s plain substring search would otherwise
        // match the embedded "s" inside "const" itself, so the real declared variable's own byte
        // offset (right after "const ") is computed explicitly instead.
        let source = "const s: string = \"hi\";\n";
        let variable_start = source.find("const ").expect("const") + "const ".len();
        let spans = highlight_typescript(source, false);
        let kind = spans
            .iter()
            .find(|span| span.start <= variable_start && variable_start < span.end)
            .map_or(HighlightKind::Text, |span| span.kind);
        assert_eq!(
            kind,
            HighlightKind::Variable,
            "a const/let/var binding's own name must never be classified as a function"
        );
    }

    /// The same real collision, for an `interface` member name (`property_signature`'s `name`
    /// field) - must not be classified as a function either. Classified `Property` (a real,
    /// registered bucket since GitHub issue #31 - `tree-sitter-javascript`'s own
    /// `(property_identifier) @property` rule is unconditional), not plain `Text` as before that
    /// scope existed.
    #[test]
    fn typescript_interface_member_name_is_not_misclassified_as_a_function() {
        let source = "interface Point { x: number }\n";
        let spans = highlight_typescript(source, false);
        assert_eq!(
            kind_at(&spans, source, "x: number"),
            HighlightKind::Property
        );
    }

    /// The same real collision, for a class method's own name (`method_definition`'s `name`
    /// field, a `property_identifier`) - this one, unlike the two above, genuinely *should* be
    /// classified as a function.
    #[test]
    fn typescript_class_method_name_is_classified_as_a_function_method() {
        let source = "class Point {\n    length() {\n        return 0;\n    }\n}\n";
        let spans = highlight_typescript(source, false);
        let span = find_span(&spans, source, "length").expect("method name span");
        // `(method_definition name: (property_identifier) @function.method)` - its own
        // dedicated bucket since GitHub issue #31 (previously folded into plain `Function`).
        assert_eq!(span.kind, HighlightKind::FunctionDefinition);
    }

    /// The same real collision, for a real function call's callee (`call_expression`'s
    /// `function` field) - genuinely a function, and must stay classified as one.
    #[test]
    fn typescript_call_expression_callee_is_classified_as_a_function() {
        let source = "function top() {}\ntop();\n";
        let spans = highlight_typescript(source, false);
        let function_spans: Vec<_> = spans
            .iter()
            .filter(|span| source[span.start..span.end] == *"top")
            .collect();
        assert_eq!(function_spans.len(), 2, "the declaration and the call site");
        assert_eq!(
            function_spans[0].kind,
            HighlightKind::FunctionDefinition,
            "the `function top()` declaration is a real definition site"
        );
        assert_eq!(
            function_spans[1].kind,
            HighlightKind::Function,
            "the `top()` call site stays a plain function - this split is the whole point of \
             JAVASCRIPT_DEFINITION_SUPPLEMENT"
        );
    }

    /// A real TSX tag name (`jsx_self_closing_element`'s own `name` field) is the same real
    /// collision one more time - must not render as a Function either. Classified `Tag` (its own
    /// real, dedicated bucket since GitHub issue #31 - previously folded into `Type`; see
    /// `theme::syntax::TAG`'s own docs for why the two still render identically).
    #[test]
    fn tsx_tag_name_is_not_misclassified_as_a_function() {
        let source = "const el = <div />;\n";
        let spans = highlight_typescript(source, true);
        let span = find_span(&spans, source, "div").expect("tag name span");
        assert_ne!(span.kind, HighlightKind::Function);
        assert_eq!(span.kind, HighlightKind::Tag);
    }

    #[test]
    fn highlighting_invalid_typescript_still_returns_a_real_non_empty_span_list() {
        let spans = highlight_typescript("function (((( broken", false);
        assert!(spans.iter().any(|span| span.kind == HighlightKind::Keyword));
    }

    // Real Python highlighting coverage - mirrors `highlight_rust`'s own test shape above.
    // Before this fix, none of `highlight_python`'s real, common-case behavior had any test
    // coverage at all.

    const SAMPLE_PYTHON: &str =
        "def add(left: int) -> int:\n    name = \"x\"\n    return left + 1\n";

    #[test]
    fn python_def_keyword_is_classified_as_keyword() {
        let spans = highlight_python(SAMPLE_PYTHON);
        let span = find_span(&spans, SAMPLE_PYTHON, "def").expect("def span");
        assert_eq!(span.kind, HighlightKind::Keyword);
    }

    #[test]
    fn python_string_literal_is_classified_as_string() {
        let spans = highlight_python(SAMPLE_PYTHON);
        let span = find_span(&spans, SAMPLE_PYTHON, "\"x\"").expect("string literal span");
        assert_eq!(span.kind, HighlightKind::String);
    }

    #[test]
    fn python_function_definition_name_is_classified_as_function() {
        let spans = highlight_python(SAMPLE_PYTHON);
        let span = find_span(&spans, SAMPLE_PYTHON, "add").expect("function name span");
        assert_eq!(span.kind, HighlightKind::FunctionDefinition);
    }

    #[test]
    fn python_type_annotation_is_classified_as_type() {
        let spans = highlight_python(SAMPLE_PYTHON);
        let type_spans: Vec<_> = spans
            .iter()
            .filter(|span| SAMPLE_PYTHON[span.start..span.end] == *"int")
            .collect();
        assert!(!type_spans.is_empty());
        assert!(type_spans
            .iter()
            .all(|span| span.kind == HighlightKind::Type));
    }

    #[test]
    fn python_comment_is_classified_as_comment() {
        let source = "# a real comment\nx = 1\n";
        let spans = highlight_python(source);
        let span = find_span(&spans, source, "# a real comment").expect("comment span");
        assert_eq!(span.kind, HighlightKind::Comment);
    }

    /// Matches Rust's own `self_is_classified_as_variable_builtin_not_keyword` test - a
    /// deliberate, documented choice that Python's `self` gets the same `VariableBuiltin`
    /// treatment Rust's does. Rust gets it from its grammar's own `(self) @variable.builtin` rule;
    /// Python's grammar has no rule for `self` at all, so it comes from
    /// [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]'s second rule - see there.
    #[test]
    fn python_self_is_classified_as_variable_builtin_not_a_plain_identifier() {
        let source = "class Foo:\n    def bar(self):\n        return self.value\n";
        let spans = highlight_python(source);
        let self_spans: Vec<_> = spans
            .iter()
            .filter(|span| source[span.start..span.end] == *"self")
            .collect();
        assert!(!self_spans.is_empty());
        assert!(self_spans
            .iter()
            .all(|span| span.kind == HighlightKind::VariableBuiltin));
    }

    /// The real, live-verified regression this fix addresses: `class_definition`'s own `name`
    /// field is a plain `identifier` in `tree-sitter-python`'s real grammar (unlike Rust/
    /// TypeScript, where it's a distinct `type_identifier` node), and the old, parent-kind-
    /// unaware matching misclassified every class name as a Function instead of a Type.
    #[test]
    fn python_class_name_is_classified_as_type_not_function() {
        let source = "class Foo:\n    pass\n";
        let spans = highlight_python(source);
        let span = find_span(&spans, source, "Foo").expect("class name span");
        assert_eq!(
            span.kind,
            HighlightKind::Type,
            "a class's own declared name should be a Type, not a Function"
        );
    }

    /// The same fixture's `def`, to prove the two real, colliding `"name"`-field cases (function
    /// vs. class) are correctly told apart within one real, combined parse - not just correct in
    /// isolation.
    #[test]
    fn python_method_name_inside_a_class_is_still_classified_as_function() {
        let source = "class Foo:\n    def bar(self):\n        pass\n";
        let spans = highlight_python(source);
        let span = find_span(&spans, source, "bar").expect("method name span");
        assert_eq!(span.kind, HighlightKind::FunctionDefinition);
    }

    /// The real call-expression collision one more time, for Python's own `call` node kind
    /// (distinct from Rust/TypeScript's `call_expression`).
    #[test]
    fn python_call_callee_is_classified_as_a_function() {
        let source = "def top():\n    pass\n\ntop()\n";
        let spans = highlight_python(source);
        let function_spans: Vec<_> = spans
            .iter()
            .filter(|span| source[span.start..span.end] == *"top")
            .collect();
        assert_eq!(function_spans.len(), 2, "the definition and the call site");
        assert_eq!(
            function_spans[0].kind,
            HighlightKind::FunctionDefinition,
            "the `def top()` name is a real definition site"
        );
        assert_eq!(
            function_spans[1].kind,
            HighlightKind::Function,
            "the `top()` call site stays a plain function"
        );
    }

    #[test]
    fn highlighting_invalid_python_still_returns_a_real_non_empty_span_list() {
        let spans = highlight_python("def (((( broken");
        assert!(spans.iter().any(|span| span.kind == HighlightKind::Keyword));
    }

    // ---------------------------------------------------------------------------------------
    // `tree-sitter-highlight` migration coverage.
    //
    // These assert on specific real tokens, and each one exists because an old-vs-new span diff
    // over real source files in this repository actually showed that token changing (or, for the
    // "still" cases, showed a way it could plausibly have changed and did not). They are the
    // executable form of that diff, not a generic "renders something" smoke test.
    // ---------------------------------------------------------------------------------------

    /// The migration's load-bearing precondition: all four real grammar configurations - including
    /// the two *composed* TypeScript ones - actually compile. `highlight_config` degrades to
    /// `None` (i.e. silently unhighlighted text) if a query fails to compile, so without this
    /// test a broken composition would show up only as a file rendering flat, with every other
    /// test in this module still passing.
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

    /// [`HIGHLIGHT_NAMES`] and [`HIGHLIGHT_KINDS`] are a positional parallel array pair, and the
    /// whole classification mapping is wrong-but-compiling if they ever drift. The array length is
    /// already tied together at compile time; this pins the *content* of every real mapping -
    /// A plain Rust local is a real `Variable`, not unclassified `Text`.
    ///
    /// This is the regression test for the single most consequential bug in the whole theme
    /// redesign, and for how it was found. `tree-sitter-rust` is the only grammar here whose
    /// bundled query has no blanket `(identifier) @variable` rule, so before
    /// `RUST_VARIABLE_PREFIX` every local in every Rust file classified as `Text` - which made
    /// `theme::syntax::VARIABLE` literally unreachable in this app's own primary language. Two
    /// rounds of palette work aimed at that token could not have changed a single pixel of a Rust
    /// file, and the maintainer's "most of the text is just white" was exactly right both times.
    ///
    /// No amount of contrast maths could have caught it. What did was taking a screenshot after a
    /// palette change, diffing it against the one before, and finding **zero** changed pixels in
    /// the code area.
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
        // The blanket rule is a *fallback*, not an override: everything more specific still wins.
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

    /// A Rust `const`/`static` is a real `Constant`, not a `Type`.
    ///
    /// The same class of bug as `a_plain_rust_local_is_a_real_variable_not_unclassified_text`, and
    /// found by the same question - "which grammars can actually emit the token this colour is
    /// attached to?". `tree-sitter-rust`'s own `@constant` rule carries a stray apostrophe inside
    /// its `#match?` regex and can never fire, and its later `@constructor` heuristic then claims
    /// the identifier. See `RUST_CONSTANT_SUPPLEMENT`.
    ///
    /// This was cosmetically invisible until the redesign gave `syntax.constant` its own orange and
    /// `syntax.type` its own gold, at which point every Rust constant rendered in the type colour.
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
        // The all-caps rule must not eat ordinary capitalised type names.
        assert_eq!(kind_at(&spans, source, "usize"), HighlightKind::TypeBuiltin);
    }

    /// A capitalised *type* is still a type, and an attribute's own identifiers are still
    /// attributes - the two things `RUST_CONSTANT_SUPPLEMENT`'s all-caps heuristic could plausibly
    /// have broken.
    #[test]
    fn the_rust_constant_heuristic_does_not_claim_types_or_attributes() {
        let source = "#[derive(Debug)]\nstruct Widget { id: u32 }\n";
        let spans = highlight_rust(source);
        assert_eq!(kind_at(&spans, source, "Widget {"), HighlightKind::Type);
        assert_eq!(kind_at(&spans, source, "derive"), HighlightKind::Attribute);
        assert_eq!(kind_at(&spans, source, "Debug"), HighlightKind::Attribute);
    }

    /// A Python parameter is a real `VariableParameter`. `tree-sitter-python`'s bundled query has
    /// **no** `@variable.parameter` pattern at all, so this token was unreachable in Python before
    /// `PYTHON_PARAMETER_SUPPLEMENT` - the same gap class as the Rust `@variable` one.
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
        // Restated last on purpose - the parameter rules must not repaint `self`.
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

    /// Go's bundled query emits neither `@variable.parameter` nor `@constant`, and its
    /// `@property` rule misses a composite literal's own keys. See `GO_CLASSIFICATION_SUPPLEMENT`.
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

    /// A shorthand property had **no capture of any kind** in TypeScript/JavaScript - the closest
    /// twin of the Rust `@variable` bug, and by blast radius the largest of the gaps this round
    /// found. See `TYPESCRIPT_IDENTIFIER_SUPPLEMENT`.
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

    /// The three parameter shapes `tree-sitter-typescript`'s own `required_parameter`/
    /// `optional_parameter` rules miss, the arrow-function one being how most callbacks are
    /// written. See `TYPESCRIPT_IDENTIFIER_SUPPLEMENT`.
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
        // The rules the bundled query already got right must be untouched.
        assert_eq!(
            kind_at(&spans, source, "a: number"),
            HighlightKind::VariableParameter
        );
    }

    /// The blanket variable rule must not swallow the contents of an attribute. See
    /// `RUST_ATTRIBUTE_SUPPLEMENT` - the bundled query only captures the enclosing
    /// `attribute_item`, and a leaf beats its ancestor when both are highlighted.
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

    /// The other half of `RUST_ATTRIBUTE_SUPPLEMENT`'s anchoring: a **macro invocation** also
    /// carries a `token_tree`, so an unanchored rule would repaint every `println!`/`assert!`
    /// argument as an attribute. Its arguments must stay ordinary code.
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

    /// The definition-site split, as one cross-language contract rather than six per-language
    /// tests: in every language that has real definition-site rules, the *declared* name is a
    /// `FunctionDefinition` and a *call* of that same name is a plain `Function`.
    ///
    /// This is what makes "colour definition sites, leave calls at plain foreground" implementable
    /// at all - see `RUST_DEFINITION_SUPPLEMENT` for why no bundled grammar query gives it to us.
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

    /// C is the one language whose definition rule had to anchor on the outer `function_definition`
    /// node: the shape the bundled query uses for `@function` fires identically on a prototype and
    /// on a definition, so the obvious rule would relabel every prototype in a header as a
    /// definition. See `C_DEFINITION_SUPPLEMENT`.
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

    /// GitHub issue #31's full extended scope list, not just the original six-bucket handful.
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
        // The non-obvious ones, each argued for in `HIGHLIGHT_KINDS`' own docs.
        assert_eq!(bucket("constructor"), HighlightKind::Constructor);
        assert_eq!(bucket("tag"), HighlightKind::Tag);
        assert_eq!(bucket("variable.builtin"), HighlightKind::VariableBuiltin);
    }

    /// Real gains the replaced implementation genuinely did not have. Its own docs called the
    /// method-call gap "an intentionally narrow, documented gap" it did not cover; the real
    /// grammar queries do. `clone` is a real `@function.method` capture (its own dedicated bucket
    /// since GitHub issue #31 registered `"function.method"` as more specific than the plain
    /// `"function"` it used to fall back to); `println!` is a macro invocation, `@function.macro`,
    /// its own dedicated bucket since GitHub issue #183 (previously unregistered, falling back to
    /// the plain `Function` bucket - see [`HIGHLIGHT_NAMES`]'s own docs).
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

    /// `mut` was listed in the replaced implementation's own Rust keyword table and never actually
    /// matched: that table compared a leaf's `kind()`, and `mut`'s real node kind is
    /// `mutable_specifier`, not `"mut"` - so it silently rendered as plain text the whole time.
    /// The real grammar query captures `(mutable_specifier) @keyword` directly.
    #[test]
    fn rust_mut_is_now_really_classified_as_a_keyword() {
        let source = "fn main() { let mut count = 0; }\n";
        let spans = highlight_rust(source);
        assert_eq!(kind_at(&spans, source, "mut "), HighlightKind::Keyword);
    }

    /// `(lifetime (identifier) @label)` - the `"label"` registration's Rust half, its own
    /// dedicated bucket since GitHub issue #183 (see [`HighlightKind::Label`]'s own docs on the
    /// cross-language unification with `c_goto_label_is_classified_as_label`/
    /// `yaml_anchor_name_is_classified_as_label`).
    #[test]
    fn rust_lifetime_is_classified_as_label() {
        let source = "fn longest<'a>(x: &'a str, y: &'a str) -> &'a str { x }\n";
        let spans = highlight_rust(source);
        // Starting the search at `a`, not the leading `'` - the grammar's own `(lifetime
        // (identifier) @label)` rule only captures the identifier, not the apostrophe sigil
        // (which is its own, separately-classified token).
        assert_eq!(kind_at(&spans, source, "a>"), HighlightKind::Label);
    }

    /// Rust's `self` stays `VariableBuiltin` (its own dedicated bucket since GitHub issue #31,
    /// carrying forward the same colour the old, unsplit `Literal` bucket used - see
    /// `theme::syntax::VARIABLE_BUILTIN`'s own docs), now via the real grammar's own
    /// `(self) @variable.builtin` rather than the replaced implementation's bespoke node-kind
    /// table entry - and TypeScript's `this` *joins* it, which is the one deliberate
    /// cross-language reclassification in the original migration (see [`HIGHLIGHT_KINDS`]' docs).
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

    /// Python's `and`/`or`/`not`/`in`/`is` live in `tree-sitter-python`'s `@operator` list, not
    /// its `@keyword` list - [`PYTHON_HIGHLIGHTS_SUPPLEMENT`] promotes the word operators
    /// specifically to real keywords (the supplement is appended text, so its rule wins the
    /// "last matching pattern wins" tie against the base query's own `@operator` capture for the
    /// same node - see [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]'s own docs). A symbolic operator like `+`
    /// is left alone, so it keeps its base-query `@operator` capture - now a real, registered
    /// bucket since GitHub issue #31 (`theme::syntax::OPERATOR` is still a direct alias of
    /// `theme::syntax::TEXT`, so this is a classification-only change with no visual difference).
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

    /// A compound Python type annotation. `tree-sitter-python`'s own rule only fires for a bare
    /// identifier directly under the `type` node, so all three of these real shapes rendered as
    /// plain text until [`PYTHON_HIGHLIGHTS_SUPPLEMENT`]'s `(type) @type` restored the replaced
    /// implementation's whole-node behaviour - a regression a real old-vs-new diff caught.
    #[test]
    fn python_compound_type_annotations_are_classified_as_types() {
        let source = "def f(a: dict[str, int], b: pathlib.Path) -> list[int]:\n    pass\n";
        let spans = highlight_python(source);
        assert_eq!(kind_at(&spans, source, "dict[str"), HighlightKind::Type);
        assert_eq!(kind_at(&spans, source, "pathlib.Path"), HighlightKind::Type);
        assert_eq!(kind_at(&spans, source, "list[int]"), HighlightKind::Type);
    }

    /// [`HIGHLIGHT_CONFIGS`] is indexed by [`Grammar::index`], so that mapping has to stay in step
    /// with [`Grammar::ALL`] or one grammar silently reads another's cache slot.
    #[test]
    fn grammar_indices_match_their_position_in_all() {
        for (position, grammar) in Grammar::ALL.into_iter().enumerate() {
            assert_eq!(grammar.index(), position, "{}", grammar.name());
        }
        assert_eq!(Grammar::COUNT, HIGHLIGHT_CONFIGS.len());
    }

    /// Python's `class Foo:` name, across all four real casing shapes.
    ///
    /// The casing matters and is the entire point of this test. `tree-sitter-python` has no
    /// `class_definition name:` rule, so before [`PYTHON_HIGHLIGHTS_SUPPLEMENT`] gained one this
    /// depended on the query's casing heuristics and got three of these four wrong - a leading
    /// underscore or a lowercase name matched nothing and fell to `Text`, and an all-caps name was
    /// captured by the `@constant` rule and came out `Literal`. An earlier version of this test
    /// used only `Widget`, the one shape that happened to work, and so passed against a genuinely
    /// broken implementation. All four are pinned now.
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

    /// Python method calls, which the replaced implementation left as plain text and which
    /// `tree-sitter-python`'s own bundled query *also* leaves as plain text - its blanket
    /// `@property` rule outranks its own method-call rule. Rust and TypeScript both colour the
    /// equivalent call, so this is the rule that makes the claim of closing the method-call gap
    /// true for all three languages rather than two.
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
        // ...while a bare `cls`/`self` reference stays in the variable-builtin/self bucket.
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

    #[test]
    fn toml_key_is_classified_as_property() {
        let spans = highlight_toml(SAMPLE_TOML);
        assert_eq!(
            kind_at(&spans, SAMPLE_TOML, "name"),
            HighlightKind::Property
        );
    }

    #[test]
    fn toml_string_value_is_classified_as_string() {
        let spans = highlight_toml(SAMPLE_TOML);
        assert_eq!(
            kind_at(&spans, SAMPLE_TOML, "\"jerry\""),
            HighlightKind::String
        );
    }

    #[test]
    fn toml_boolean_is_classified_as_constant_builtin() {
        let spans = highlight_toml(SAMPLE_TOML);
        assert_eq!(
            kind_at(&spans, SAMPLE_TOML, "true"),
            HighlightKind::ConstantBuiltin
        );
    }

    /// `(local_date) @string.special` - its own dedicated bucket since GitHub issue #183
    /// (previously fell through to the plain `"string"` entry).
    #[test]
    fn toml_date_literal_is_classified_as_string_special() {
        let spans = highlight_toml(SAMPLE_TOML);
        assert_eq!(
            kind_at(&spans, SAMPLE_TOML, "1979-05-27"),
            HighlightKind::StringSpecial
        );
    }

    const SAMPLE_GO: &str =
        "package main\n\nfunc add(x int) int {\n\treturn len(fmt.Sprint(x))\n}\n";

    #[test]
    fn go_func_keyword_is_classified_as_keyword() {
        let spans = highlight_go(SAMPLE_GO);
        assert_eq!(kind_at(&spans, SAMPLE_GO, "func"), HighlightKind::Keyword);
    }

    #[test]
    fn go_function_definition_name_is_classified_as_function() {
        let spans = highlight_go(SAMPLE_GO);
        assert_eq!(
            kind_at(&spans, SAMPLE_GO, "add"),
            HighlightKind::FunctionDefinition
        );
    }

    /// `@function.builtin` (`len`) - its own dedicated bucket since GitHub issue #183 (previously
    /// fell through to the plain `"function"` entry's subset match, same as a call to `add`).
    #[test]
    fn go_builtin_function_call_is_classified_as_function_builtin() {
        let spans = highlight_go(SAMPLE_GO);
        assert_eq!(
            kind_at(&spans, SAMPLE_GO, "len("),
            HighlightKind::FunctionBuiltin
        );
    }

    const SAMPLE_JSON: &str = "{\n  \"name\": \"jerry\",\n  \"count\": 3\n}\n";

    #[test]
    fn json_object_key_is_classified_as_property_not_string() {
        let spans = highlight_json(SAMPLE_JSON);
        assert_eq!(
            kind_at(&spans, SAMPLE_JSON, "\"name\""),
            HighlightKind::Property,
            "a real JSON key must win the more-specific string.special.key registration over \
             the plain string entry"
        );
    }

    #[test]
    fn json_string_value_is_classified_as_string_not_property() {
        let spans = highlight_json(SAMPLE_JSON);
        assert_eq!(
            kind_at(&spans, SAMPLE_JSON, "\"jerry\""),
            HighlightKind::String
        );
    }

    #[test]
    fn json_number_value_is_classified_as_number() {
        let spans = highlight_json(SAMPLE_JSON);
        assert_eq!(kind_at(&spans, SAMPLE_JSON, "3"), HighlightKind::Number);
    }

    const SAMPLE_YAML: &str = "anchor: &base\n  enabled: true\nalias: *base\ncount: 3\n";

    #[test]
    fn yaml_key_is_classified_as_property() {
        let spans = highlight_yaml(SAMPLE_YAML);
        assert_eq!(
            kind_at(&spans, SAMPLE_YAML, "count"),
            HighlightKind::Property
        );
    }

    #[test]
    fn yaml_boolean_is_classified_as_constant_builtin() {
        let spans = highlight_yaml(SAMPLE_YAML);
        assert_eq!(
            kind_at(&spans, SAMPLE_YAML, "true"),
            HighlightKind::ConstantBuiltin
        );
    }

    /// `(anchor_name) @label` - the cross-language `"label"` registration's YAML half, its own
    /// dedicated bucket since GitHub issue #183 (its C half is a goto target - see
    /// `c_goto_label_is_classified_as_label` below; previously both fell through to
    /// `Variable`).
    #[test]
    fn yaml_anchor_name_is_classified_as_label() {
        let spans = highlight_yaml(SAMPLE_YAML);
        assert_eq!(kind_at(&spans, SAMPLE_YAML, "base\n"), HighlightKind::Label);
    }

    /// `"&"/"*" @punctuation.special` - its own dedicated bucket since GitHub issue #183
    /// (previously fell through to `Operator`).
    #[test]
    fn yaml_anchor_and_alias_sigils_are_classified_as_punctuation_special() {
        let spans = highlight_yaml(SAMPLE_YAML);
        assert_eq!(
            kind_at(&spans, SAMPLE_YAML, "&base"),
            HighlightKind::PunctuationSpecial
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_YAML, "*base"),
            HighlightKind::PunctuationSpecial
        );
    }

    const SAMPLE_C: &str =
        "int add(int x) {\n  int total = 0;\n  goto done;\n done:\n  return total;\n}\n";

    #[test]
    fn c_keyword_is_classified_as_keyword() {
        let spans = highlight_c(SAMPLE_C);
        assert_eq!(kind_at(&spans, SAMPLE_C, "return"), HighlightKind::Keyword);
    }

    #[test]
    fn c_function_definition_name_is_classified_as_function() {
        let spans = highlight_c(SAMPLE_C);
        assert_eq!(
            kind_at(&spans, SAMPLE_C, "add"),
            HighlightKind::FunctionDefinition
        );
    }

    /// `(statement_identifier) @label` - the `"label"` registration's C half (a goto target, not
    /// a lifetime - see [`HighlightKind::Label`]'s own docs on the cross-language unification).
    #[test]
    fn c_goto_label_is_classified_as_label() {
        let spans = highlight_c(SAMPLE_C);
        assert_eq!(kind_at(&spans, SAMPLE_C, "done:"), HighlightKind::Label);
    }

    /// `";" @delimiter` - the new, plain `"delimiter"` registration, distinct from the
    /// already-registered `"punctuation.delimiter"`.
    #[test]
    fn c_semicolon_is_classified_as_punctuation_delimiter() {
        let spans = highlight_c(SAMPLE_C);
        assert_eq!(
            kind_at(&spans, SAMPLE_C, ";"),
            HighlightKind::PunctuationDelimiter
        );
    }

    const SAMPLE_MARKDOWN: &str = "# Title\n\nSome **bold** and *italic* text with `inline code` and a [link](https://example.com).\n\n```rust\nfn main() {}\n```\n";

    /// GitHub issue #104's own core premise: without a real `injection_callback` resolving
    /// `"markdown_inline"`, the block grammar alone never parses prose content at all, so
    /// everything inside a paragraph collapses to one flat, unclassified `Text` span. This proves
    /// the injection actually fires - `text_strong_...`/`text_emphasis_...` below prove the
    /// specific captures it unlocks.
    #[test]
    fn inline_content_is_never_left_as_a_single_flat_text_region() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert!(
            spans.iter().any(|span| span.kind != HighlightKind::Text),
            "a real markdown document must produce at least one non-Text span - if this fails, \
             the inline grammar injection isn't firing at all"
        );
    }

    #[test]
    fn markdown_heading_text_is_classified_as_heading() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN, "Title"),
            HighlightKind::Heading
        );
    }

    #[test]
    fn markdown_bold_text_is_classified_as_strong() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN, "bold"),
            HighlightKind::Strong
        );
    }

    #[test]
    fn markdown_italic_text_is_classified_as_emphasis() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN, "italic"),
            HighlightKind::Emphasis
        );
    }

    #[test]
    fn markdown_inline_code_is_classified_as_literal_string() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN, "inline code"),
            HighlightKind::String
        );
    }

    #[test]
    fn markdown_link_destination_and_label_are_classified_as_link() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN, "https://example.com"),
            HighlightKind::Link,
            "the link destination"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN, "link"),
            HighlightKind::Link,
            "the link's own visible label text"
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

    /// Every real capture `tree-sitter-html`'s own bundled `queries/highlights.scm` emits, in one
    /// test - the whole file is seven patterns long (read directly), so this is genuinely complete
    /// coverage of it rather than a sample. Note that not one of them needed a new
    /// [`HIGHLIGHT_NAMES`] entry: `tag`/`attribute`/`string`/`comment`/`constant`/
    /// `punctuation.bracket` were all already registered for other languages, and the one name
    /// that was not (`tag.error`, an unmatched closing tag) resolves through the plain `"tag"`
    /// entry by the subset rule this module's own docs describe.
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

    /// `(erroneous_end_tag_name) @tag.error` - its own dedicated bucket since GitHub issue #183
    /// (previously fell through to the plain `"tag"` entry, reading a mismatched closing tag as
    /// an ordinary, correct one). `</spam>` never had a matching `<spam>` opener - only `<div>`
    /// did - so `tree-sitter-html` itself flags it as erroneous, not this app's own logic.
    #[test]
    fn html_mismatched_closing_tag_is_classified_as_tag_error() {
        let source = "<div></spam>\n";
        let spans = highlight_html(source);
        assert_eq!(kind_at(&spans, source, "div"), HighlightKind::Tag);
        assert_eq!(kind_at(&spans, source, "spam"), HighlightKind::TagError);
    }

    /// The real reason `tree-sitter-html`'s own `INJECTIONS_QUERY` is wired rather than dropped:
    /// a `<style>` element's body is genuinely CSS and a `<script>` element's body is genuinely
    /// JavaScript, and both now reach those grammars for real. `#ff0000` inside `<style>` is a
    /// `(color_value) @string.special` that only the CSS grammar has any rule for at all - HTML's
    /// own query would leave the whole `<style>` body as one unclassified `(raw_text)` run.
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

    /// `(color_value) @string.special` gets its own real bucket since GitHub issue #183
    /// (previously fell through to the plain `"string"` entry), and must **not** resolve through
    /// the more specific `"string.special.key"` one that JSON registers: the recognized-name rule
    /// requires every one of a recognized name's own dot-parts to be present in the capture, and
    /// `key` is not present in `string.special`.
    #[test]
    fn css_color_literal_is_a_string_special_not_a_json_style_key() {
        let spans = highlight_css(SAMPLE_CSS);
        assert_eq!(
            kind_at(&spans, SAMPLE_CSS, "ff0000"),
            HighlightKind::StringSpecial
        );
    }

    const SAMPLE_MARKDOWN_FENCES: &str = "# Fences\n\n```html\n<div class=\"card\">hi</div>\n```\n\n```css\n.card { color: red; }\n```\n\n```rust\nfn main() {}\n```\n\n```zig\nconst x = 1;\n```\n\n```\nplain fence\n```\n";

    /// GitHub issue #154's own headline ask - "including in the markdown files". A ` ```html `
    /// fence's *content* really is reparsed by `tree-sitter-html`: `div` comes out
    /// [`Tag`](HighlightKind::Tag), which no markdown query has any rule capable of producing.
    #[test]
    fn a_markdown_html_fence_really_highlights_its_content_as_html() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "div class"),
            HighlightKind::Tag
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "class="),
            HighlightKind::Attribute
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "card\">"),
            HighlightKind::String
        );
    }

    #[test]
    fn a_markdown_css_fence_really_highlights_its_content_as_css() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "card { "),
            HighlightKind::Property,
            "the class selector inside the ```css fence"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "color: red"),
            HighlightKind::Property
        );
    }

    /// The proof that this is a real, general mechanism rather than an html/css special case: the
    /// exact same injection query resolves ` ```rust ` too, with no Rust-specific code anywhere in
    /// [`MARKDOWN_INJECTION_QUERY`] or [`Grammar::for_injection_name`].
    #[test]
    fn a_markdown_rust_fence_really_highlights_its_content_as_rust() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "fn main"),
            HighlightKind::Keyword,
            "the `fn` keyword - the first token of the fence's content, which is exactly the one \
             a parent highlight left open over the injected range would silently steal"
        );
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "main()"),
            HighlightKind::FunctionDefinition
        );
    }

    /// The honest fallback, both halves of it: a fence tagged with a language this app has no
    /// grammar for, and a fence with no tag at all, are plain [`Text`](HighlightKind::Text) - not
    /// a panic, and not some other language's colouring applied speculatively.
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

    /// The fence's own info string kept its colour through
    /// [`MARKDOWN_BLOCK_HIGHLIGHTS_SUPPLEMENT`]'s first line cancelling `@text.literal` for the
    /// fenced block - that is what the supplement's second line is for, and this is the guard on
    /// it.
    #[test]
    fn a_fence_info_string_is_still_colored_like_a_literal() {
        let spans = highlight_markdown(SAMPLE_MARKDOWN_FENCES);
        assert_eq!(
            kind_at(&spans, SAMPLE_MARKDOWN_FENCES, "html\n"),
            HighlightKind::String
        );
    }

    /// The other half of that same supplement's blast radius: it cancels `@text.literal` for
    /// *fenced* blocks only, and the bundled query's `[(link_title) (indented_code_block)
    /// (fenced_code_block)] @text.literal` still covers the other two.
    #[test]
    fn an_indented_code_block_is_still_colored_like_a_literal() {
        let source = "Text.\n\n    indented code\n";
        let spans = highlight_markdown(source);
        assert_eq!(
            kind_at(&spans, source, "indented code"),
            HighlightKind::String
        );
    }

    /// The block-level HTML half of "including in the markdown files": a raw `<div>` block written
    /// straight into a markdown document, which `tree-sitter-md` parses as one opaque
    /// `(html_block)` and never looks inside.
    #[test]
    fn a_raw_html_block_in_markdown_is_injected_into_the_html_grammar() {
        let source = "Before.\n\n<div class=\"card\">block</div>\n\nAfter.\n";
        let spans = highlight_markdown(source);
        assert_eq!(kind_at(&spans, source, "div class"), HighlightKind::Tag);
        assert_eq!(kind_at(&spans, source, "class="), HighlightKind::Attribute);
    }

    /// The inline half: a raw tag inside a paragraph is an `(html_tag)` node of the *inline*
    /// grammar, so this only works because [`MARKDOWN_INLINE_INJECTION_QUERY`] exists and is
    /// reached through a second level of injection (block -> inline -> html).
    #[test]
    fn a_raw_html_tag_inside_a_markdown_paragraph_is_injected_into_the_html_grammar() {
        let source = "Inline <span class=\"i\">tag</span> here.\n";
        let spans = highlight_markdown(source);
        assert_eq!(kind_at(&spans, source, "span class"), HighlightKind::Tag);
        assert_eq!(kind_at(&spans, source, "class="), HighlightKind::Attribute);
    }

    /// YAML frontmatter, a third real injection the same mechanism unlocked for free - the block
    /// grammar's own `(minus_metadata)` node, which is plain `Text` without an injection.
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

    /// A fence nested inside a list item or a block quote, which is the case
    /// [`MARKDOWN_INJECTION_QUERY`]'s `injection.include-children` decision most plausibly puts at
    /// risk: including children means the `(block_continuation)` nodes carrying each line's `  `
    /// or `> ` prefix are now inside the injected range too. Both really work - the content
    /// highlights as Rust, and the block quote's own `> ` markers keep their own colour rather
    /// than being swallowed by the injected layer.
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

    /// [`Grammar::for_injection_name`]'s first lookup: a `#set! injection.language` predicate
    /// naming a grammar directly, which is how `tree-sitter-md` requests `"markdown_inline"` and
    /// `"html"` and how `tree-sitter-html` requests `"css"`.
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

    /// The real drift guard over the second lookup. Every extension the registry claims a
    /// highlighter for must be reachable as a fence language *and* land on a real grammar -
    /// otherwise a ` ```yml ` fence would silently render as plain text while `.yml` files
    /// highlight fine, with nothing to catch the gap.
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

    /// The opposite direction: [`Grammar::for_extension`] must not claim an extension the registry
    /// has no highlighter for, which would mean a fence highlighting in a colour the same file on
    /// disk never gets.
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

    /// A fence tag nobody has a grammar for resolves to nothing at all, which is what makes
    /// `an_unknown_or_absent_fence_language_falls_back_to_plain_text` a real fallback rather than
    /// an accident.
    #[test]
    fn an_unknown_injection_name_resolves_to_no_grammar() {
        assert_eq!(Grammar::for_injection_name("zig"), None);
        assert_eq!(Grammar::for_injection_name("latex"), None);
        assert_eq!(Grammar::for_injection_name(""), None);
    }

    /// Both halves of the TypeScript query composition, which is the single most breakable part of
    /// this migration: `tree-sitter-typescript`'s own query file defines none of these, so if the
    /// JavaScript query ever stopped being concatenated in, every one of them would silently
    /// collapse to plain text while the TypeScript-only assertions elsewhere kept passing.
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

    /// The real TypeScript regressions [`TYPESCRIPT_HIGHLIGHTS_SUPPLEMENT`] exists to repair, all
    /// found by diffing old against new over real source rather than by inspection.
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
        // The sibling predefined type it has to stay consistent with.
        assert_eq!(
            kind_at(&spans, source, "string"),
            HighlightKind::TypeBuiltin
        );
    }

    /// Real TSX. The JSX query is only composed in for the TSX grammar (it references node kinds
    /// the plain TypeScript grammar does not have), so this is the only place element-name
    /// classification is exercised at all.
    ///
    /// `div` and `Badge` used to both assert `HighlightKind::Type` (a lowercase JSX element name
    /// arriving via `@tag` was folded into the same bucket as a capitalised one arriving via
    /// TypeScript's own `@type` heuristic - see `theme::syntax::TAG`'s own docs). GitHub issue #31
    /// gave `@tag` its own dedicated [`HighlightKind::Tag`] bucket, so the two are now correctly
    /// told apart at the classification level even though `theme::syntax::TAG` still aliases
    /// `theme::syntax::TYPE` and so the two still *render* identically.
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

    /// Interpolated code inside a template literal / f-string is now classified as the real code
    /// it is, instead of being swallowed whole by the enclosing string literal. Verified against
    /// real occurrences in this repository's own vendored sources before being pinned here.
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

    /// The span list [`highlight_with`] produces must be sorted, non-overlapping and gapless, with
    /// no empty spans - `build_lines` clips against it positionally and would mis-render if any of
    /// that were violated. The replaced implementation emitted leaf-only spans with real gaps, so
    /// this is a genuinely new invariant worth pinning rather than an inherited one.
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

    /// Before GitHub issue #31, `string` and `escape` both resolved to the same unsplit `Literal`
    /// bucket, so adjacent same-kind coalescing made a string containing escape sequences render
    /// as one continuous run with no visible distinction between the string body and its own
    /// escapes. That was a deliberate simplification at the time, and issue #31's whole point is
    /// to stop making it: `string.escape` is one of its checklist scopes by name. This test now
    /// pins the *opposite* invariant - each escape sequence gets its own real, separately
    /// classified `StringEscape` span, distinct from the surrounding `String` one - while still
    /// confirming the historical guarantee that mattered (no byte of the string, escapes
    /// included, ever falls back to plain `Text`).
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

    /// Regression: a genuinely empty side (zero real lines - e.g. a merge conflict hunk where
    /// one side deletes everything) must produce zero `RenderedLine`s, not one fabricated blank
    /// line via `build_lines`' own "an empty file still has one empty line" convention. A caller
    /// driving row/gutter-number count off this `Vec`'s length must never render a phantom row
    /// for content that was never real.
    #[test]
    fn highlight_block_on_zero_input_lines_returns_zero_rendered_lines() {
        let rendered = highlight_block(std::iter::empty(), Some("rs"), HighlightOptions::default());
        assert!(
            rendered.is_empty(),
            "zero real input lines must produce zero RenderedLines, not build_lines' one-empty-\
             line-for-an-empty-file convention: got {rendered:?}"
        );
    }

    /// Distinct from the zero-lines case above: one real, genuinely blank line (e.g. a real
    /// blank line inside a conflict hunk's content) must still render as one real empty line -
    /// the fix must not overcorrect into dropping real blank lines too.
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
                    // This helper models a single-language file, which is exactly one scope.
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

    /// The headline case from the issue: three different bracket shapes nesting inside one
    /// another, each pair's two halves sharing one colour, each level a different one.
    #[test]
    fn mixed_bracket_shapes_nest_and_each_pair_shares_one_ring_colour() {
        assert_eq!(depth_map("foo([{}])", "([{}])"), "...123321");
    }

    /// Siblings are not nesting: two pairs at the same level both get depth 0, so a long flat
    /// argument list doesn't drift through the ring.
    #[test]
    fn sibling_pairs_at_the_same_level_all_get_the_same_ring_colour() {
        assert_eq!(depth_map("()()()", "()"), "111111");
        // Both inner pairs are siblings *inside* the outer one, so both are at depth 1 - a
        // sibling never advances the ring, only nesting does.
        assert_eq!(depth_map("(()())", "()"), "122221");
    }

    /// The ring is `% 6`, so the seventh level of nesting really does come back around to colour
    /// 1 - and the matching closer comes back around with it.
    #[test]
    fn the_seventh_nesting_level_wraps_back_to_the_first_ring_colour() {
        // Eight levels: depths 0..7 colour as 1,2,3,4,5,6 and then wrap to 1,2 - and every
        // closer comes back around with its own opener.
        assert_eq!(depth_map("(((((((())))))))", "()"), "1234561221654321");
    }

    /// A stray closer is left plain and, critically, does **not** pop: the `(` around it is still
    /// really open, so its own eventual `)` must still find it. Popping here would miscolour
    /// everything after the anomaly instead of just the anomaly itself.
    #[test]
    fn a_stray_closer_is_left_plain_and_does_not_consume_the_open_bracket() {
        assert_eq!(depth_map("(a)b)", "()"), "1.1.-");
        assert_eq!(depth_map("()) ()", "()"), "11-.11");
        // The load-bearing one: the stray `)` sits *inside* a real pair.
        assert_eq!(depth_map("( ) )", "()"), "1.1.-");
        assert_eq!(depth_map("[ ) ]", "[)]"), "1.-.1");
    }

    /// The mid-edit case every real editor has to tolerate: an opener with no partner yet keeps
    /// plain punctuation colouring rather than claiming a depth it can't prove.
    #[test]
    fn an_opener_that_never_closes_is_left_plain() {
        assert_eq!(depth_map("fn f() {", "(){}"), "....11.-");
        assert_eq!(depth_map("{[", "{["), "--");
    }

    /// Shapes have to agree, not just counts - a naive depth counter would happily colour `([)]`
    /// as two nested pairs. The real stack matcher pairs `[` with `]` and leaves both the `(` and
    /// the `)` plain, because neither ever met a partner of its own shape.
    ///
    /// `[`/`]` land at ring depth 0 (`"-1-1"`), not depth 1 - GitHub issue #182's own fix:
    /// the never-matched `(` is dropped from the depth stack retroactively, so it can't hold a
    /// level for the real pair that comes after it, the same way it wouldn't if it simply weren't
    /// there. Before that fix this asserted `"-2-2"` - `[`/`]` read as nested one level inside an
    /// opener that was never going to close.
    #[test]
    fn mismatched_shapes_do_not_pair_up() {
        assert_eq!(depth_map("([)]", "([)]"), "-1-1");
    }

    /// GitHub issue #182's own minimal case: one permanently-unmatched opener, followed by two
    /// completely real, well-formed pairs. Before the fix, `(x)` read as nested one level inside
    /// the leading `(` (ring 1, `"2"`) and `(y(z))` one level deeper again (rings "2"/"3") - both
    /// wrong, since that leading `(` was never actually their ancestor; it just never closed.
    /// With it dropped from the stack retroactively, `(x)` and the outer `(y(z))` both correctly
    /// read as their own real, independent depth-0 pairs, `(z)` one real level inside `(y...)`.
    #[test]
    fn an_unmatched_opener_no_longer_shifts_the_depth_of_real_pairs_that_follow_it() {
        assert_eq!(depth_map("( (x) (y(z))", "()"), "-.1.1.1.2.21");
    }

    /// Real regression coverage for GitHub issue #182, reproduced with the exact source the
    /// issue itself used: three "functions" in one file, the middle one deliberately never
    /// closing its own body - the ordinary state of a file being actively typed into. Before the
    /// fix, `fn c`'s own perfectly well-formed body brace and its nested `ok()` call rendered at
    /// ring depths 4/5 instead of 0/1 (verified by hand-tracing the pre-fix algorithm against
    /// this exact source - it matches the issue's own reported "fn c... renders at depths 4/5"
    /// exactly), because two earlier, permanently-unmatched openers (`fn a`'s own `{`, orphaned
    /// when its own `}` got consumed matching the wrong bracket inside `([)]`; and the stray `(`
    /// inside `([)]` itself) never left the depth stack.
    ///
    /// Rather than hand-transcribe the combined source's full ~90-character depth map (fragile,
    /// and no more informative than the property that actually matters), this asserts the
    /// property GitHub issue #182 is really about: `fn c`'s own brackets must colour *identically*
    /// whether or not those two earlier unmatched openers precede it in the same file. Comparing
    /// against a completely independent, isolated parse of the same `fn c` snippet is what proves
    /// that - if the earlier unmatched brackets were still leaking into depth, the two parses
    /// would disagree.
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

    /// Nothing panics or produces nonsense on input that is nothing but noise - the honest
    /// degradation the issue asks for. Everything unmatched, nothing coloured.
    #[test]
    fn thoroughly_unbalanced_input_degrades_to_plain_brackets_without_panicking() {
        assert_eq!(depth_map(")))", "()"), "---");
        assert_eq!(depth_map("}{", "{}"), "--");
        assert_eq!(depth_map(")}]([{", "()[]{}"), "------");
    }

    /// The reason this pass needs no string/comment awareness of its own: a `{` inside a string
    /// literal or a comment is never classified `PunctuationBracket` by the grammar's own query
    /// in the first place, so it is invisible here by construction. Modelled exactly as the real
    /// pipeline delivers it - the brackets inside the quotes carry `String`/`Comment` spans.
    #[test]
    fn brackets_inside_strings_and_comments_are_never_coloured() {
        // `f("(", /* ) */)` - only the outer `(` and the final `)` are real bracket tokens.
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

    /// Two independent ` ```rust ` fences in one markdown document must pair their brackets
    /// **separately**. This is the real bug `HighlightSpan::scope` and `injection_scopes` exist to
    /// fix, reproduced exactly as it was found: before the fix, `colorize_bracket_pairs` ran one
    /// global stack over the whole document, so the unclosed `{` in the first fence paired with the
    /// `}` that opens the second - two brackets in two different code blocks, painted as one
    /// matched pair, with every depth in the second fence shifted by the first fence's leftovers.
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

    /// The other half of the same fix: an *unbalanced* fence must not shift the depth ring for the
    /// fences after it. `fn b`'s own braces are the outermost pair in their own fence, so they must
    /// paint ring colour 0 - not 2, which is what the leaked global stack produced.
    #[test]
    fn an_unbalanced_fence_does_not_shift_the_ring_for_later_fences() {
        let source = "```rust\nfn a() { let x = (;\n```\n\n```rust\nfn b() { c([1]); }\n```\n";
        let spans = ring_spans(source, highlight_markdown);

        // `fn b`'s body brace - the second `{` in the document.
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

    /// A fence whose code is genuinely balanced is unaffected by this fix - both fences already
    /// started at 0 before it, and must still.
    #[test]
    fn two_balanced_fences_each_start_the_ring_at_zero() {
        let source = "```rust\nfn a() { b(); }\n```\n\n```rust\nfn c() { d(); }\n```\n";
        let spans = ring_spans(source, highlight_markdown);
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 1)), Some(0));
    }

    /// `<`/`>` really do arrive here as `PunctuationBracket` (Rust and TypeScript both capture a
    /// type-argument list's angle brackets that way, and `tree-sitter-html` captures a tag's) and
    /// are deliberately skipped - see `colorize_bracket_pairs`' own docs. They must not push, not
    /// pop, and not disturb the depth of the real brackets around them.
    #[test]
    fn angle_brackets_never_participate_and_never_disturb_the_stack() {
        assert_eq!(depth_map("<>", "<>"), "--");
        // A generic argument list wrapping a real paren pair: the parens are still depth 0.
        assert_eq!(depth_map("f::<A>(x)", "<>()"), "...-.-1.1");
        // HTML, the case that would be actively wrong if angle brackets were tracked.
        assert_eq!(depth_map("<p></p>", "<>"), "-.--..-");
    }

    /// `fold_highlight_events` coalesces adjacent same-bucket spans, so a run like `}})` really
    /// does reach this function as *one* span. Each bracket in it still has to be coloured
    /// individually.
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

    /// The flip side: a bracket run this pass leaves entirely plain must come back out as the
    /// single span it went in as, not as N one-character spans - otherwise every `<>` in a
    /// TypeScript file would inflate `build_lines`' per-line run count for nothing.
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

    /// Source with no bracket tokens at all is handed straight back, untouched - the early-out
    /// that keeps this pass free for prose, YAML, plain text and every other bracket-light file.
    #[test]
    fn source_with_no_bracket_tokens_is_returned_completely_unchanged() {
        let source = "let x = 1;";
        let spans = bracket_spans(source, "");
        let before = spans.clone();
        assert_eq!(colorize_bracket_pairs(source, spans), before);
        assert!(colorize_bracket_pairs("", Vec::new()).is_empty());
    }

    /// The output has to stay the gapless, ordered, non-overlapping span list every consumer
    /// downstream (`build_lines`, `minimap`, `edit_buffer`'s stale-span reuse) already relies on -
    /// splitting and re-coalescing must not break that invariant.
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

    /// Multi-byte characters between brackets must not shift any byte offset - the split loop
    /// walks `char_indices`, and this is what would catch it walking bytes instead.
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

    /// `for_bracket_depth` is the single home of the `% 6` wrap-around, and the ring it indexes is
    /// really six distinct buckets (not six aliases of one).
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

    /// Rust, through the real `tree-sitter-rust` grammar: three nesting levels, each a different
    /// ring colour, each pair's two halves sharing one. The string literal's own `(` is the
    /// control: it must stay part of the string.
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

        // `main()`'s own empty parameter list is a sibling of the body brace, not nested in it.
        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 0)), Some(0));

        // The control: the `(` inside the string literal is not a bracket token at all.
        let in_string = nth_offset(source, '(', 2);
        assert_eq!(
            kind_at_byte(&spans, in_string),
            HighlightKind::String,
            "a paren inside a string literal must never reach the bracket ring"
        );
        assert_eq!(ring_index_at(&spans, in_string), None);
    }

    /// TypeScript, through the real `tree-sitter-typescript` grammar. Also the real proof of the
    /// `<`/`>` decision: `Map<string, number>`'s angle brackets are genuinely captured as
    /// `punctuation.bracket` by that grammar, and must still come out plain.
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

    /// Python, through the real `tree-sitter-python` grammar - a third language, and the one whose
    /// comment syntax differs most from the other two.
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

    /// End of the real pipeline, not the middle of it: the `RenderedLine` runs the File view
    /// actually paints carry the ring buckets, and `color_for_kind` turns them into genuinely
    /// different colours on screen. This is what proves the feature is wired all the way through
    /// rather than merely computed - `build_lines` is the last step before rendering, and
    /// `color_for_kind` is what `file_view`/`minimap`/`diff_view`/`merge::render` all call.
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

        // ... and those buckets really paint different colours.
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

    /// The honest coverage matrix: which of this module's languages bracket-pair colouring really
    /// works in, asserted rather than assumed.
    ///
    /// Six of these needed no work - their grammars ship a `punctuation.bracket` capture. Four
    /// (Python, Go, JSON, C) shipped none at all and emitted *no* bracket span whatsoever before
    /// this; they work through the per-grammar bracket supplements (see
    /// [`PYTHON_BRACKET_SUPPLEMENT`]). This test is what would catch a grammar upgrade silently
    /// dropping either.
    ///
    /// Markdown and HTML are the two real, deliberate exclusions, and they are asserted as such
    /// rather than left ambiguous: Markdown prose has no bracket *tokens* (a `(` in a sentence is
    /// text, and its fenced code blocks reach the ring through their injected language instead -
    /// see the next test), and HTML's only bracketed tokens are the `<`/`>` of a tag, which
    /// `colorize_bracket_pairs` deliberately never tracks.
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

    /// `appearance.bracket_pair_colorization = false` really switches the feature off - the same
    /// spans, in the same languages, come back with the flat `PunctuationBracket` the grammar gave
    /// them and no ring bucket anywhere.
    ///
    /// Byte-for-byte identical to what this module produced before the feature existed, which is
    /// the honest meaning of "off": the pass genuinely does not run, so there is no recoloured
    /// imitation of the old behaviour to drift from it.
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

    /// Toggling back on restores the ring exactly, from the same input - the setting is a real
    /// switch, not a one-way downgrade.
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

    /// The whole-pipeline entry points honour the setting too, not just the raw `apply` seam -
    /// `load_file`'s own `_with_options` sibling and `highlight_block_with_options` are what the
    /// File view and the Diff/Merge views actually call.
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

    /// A markdown fenced code block's Rust reaches the ring too - the injection path really goes
    /// through the same `highlight_with` funnel, so this needed no markdown-specific wiring.
    #[test]
    fn brackets_inside_a_markdown_fenced_code_block_reach_the_ring() {
        let source = "# Title\n\n```rust\nfn f() { g(1); }\n```\n";
        let spans = ring_spans(source, highlight_markdown);
        assert_eq!(ring_index_at(&spans, nth_offset(source, '{', 0)), Some(0));
        assert_eq!(ring_index_at(&spans, nth_offset(source, '(', 1)), Some(1));
    }

    /// A diff hunk highlighted on its own (`highlight_block`) is a partial, hunk-local source by
    /// design, so its depths are hunk-relative and a bracket whose partner is outside the hunk
    /// stays plain. That is the honest degradation, and it is worth pinning rather than leaving as
    /// an undiscovered surprise.
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
///
/// This module is the honest substitute for "before/after screenshots of each fixture". The
/// `verify` skill (`.claude/skills/verify/`) can screenshot the running app, but has no scripted
/// way to open a chosen file in the editor, so per-fixture screenshots are not automatable. What is
/// automatable, and is arguably better evidence, is a dump of exactly what the pipeline classifies
/// each byte as and exactly what colour that resolves to - which is the thing a screenshot would
/// have been inspected *for*, minus the eyes.
///
/// Each test below renders one real fixture through the real pipeline (`HighlightOptions::default`
/// -> the real grammar -> the real bracket ring) and asserts the properties the redesign promises.
/// Run with `--nocapture` to read the full per-line dump for review.
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
        // The definition site is coloured; the call sites are not.
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
        // The brackets inside the string, char and raw string are never ring-coloured.
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
        // Every `<`/`>` stays the de-emphasized punctuation tone - never a ring colour.
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
        // Both fences are balanced, so each one's outermost pair starts the ring over at 0.
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

        // Ring on: twelve levels really do cycle, and adjacent levels really do differ.
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

        // The `(` inside the leading comment.
        let comment_paren = TORTURE.find("Unmatched (").expect("fixture") + 10;
        assert_eq!(kind_at_byte(&spans, comment_paren), HighlightKind::Comment);

        // The mismatched `([)]`: the stray `)` is left plain rather than consuming the `[`.
        let stray = TORTURE.find("([)]").expect("fixture") + 2;
        assert_eq!(
            kind_at_byte(&spans, stray),
            HighlightKind::PunctuationBracket,
            "a closer whose shape does not match the innermost opener must stay plain"
        );
    }
}
