//! Pure logic for Surface C's File view (`design_handoff_jerry_ade/README.md`'s "File view"
//! subsection): reads a file off disk, detects its line-ending style, picks a language label
//! from its extension, and - for a real subset of extensions - produces syntax-colored spans by
//! parsing with `tree-sitter` and walking the resulting AST. Deliberately `gpui`-window-free
//! (only [`gpui::Rgba`] is used, for plain colour data), mirroring this crate's split between
//! pure logic modules and `crate::root`'s `Div` construction.
//!
//! `.rs`/`.ts`/`.tsx`/`.js`/`.jsx`/`.py` get real syntax spans (Revision R8 added the latter
//! five, following [`highlight_rust`]'s own parse-then-walk shape exactly - see [`Lexicon`] for
//! how the shared walker is parameterized per language rather than duplicating the walk itself
//! three times); other extensions (including `.vue` - see `crate::language`'s docs for why this
//! phase doesn't spawn an LSP client for it, unrelated to highlighting) render as plain monospace
//! text. A further grammar would just add one more [`Lexicon`] plus a thin wrapper.
//!
//! ## `tree-sitter` API usage
//!
//! `tree_sitter::Parser::new()`, `set_language`, `Node::walk()`/`TreeCursor::goto_first_child`/
//! `goto_next_sibling`, `Parser::parse`/`Tree::root_node`, and `TreeCursor::field_name` are all
//! used below in their ordinary, documented shapes. Verified against
//! `vendor/zed/crates/language/src/language.rs:135,1376,1673` and
//! `vendor/zed/crates/language/src/outline.rs:102` (same `tree-sitter`/`tree-sitter-rust`
//! version pair as this crate's `Cargo.toml`). The TypeScript/TSX and Python node-kind names
//! [`Lexicon`]'s three table instances below use were verified for real by parsing real sample
//! source with each grammar and inspecting the actual emitted node kinds (not guessed/invented -
//! see this crate's Revision R8 step report for the probe), the same "verify the real API before
//! using it" discipline this project applies to `vendor/zed`.

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
/// ([`crate::language::ExtensionEntry::highlighter`] itself `None` - TOML/Markdown/SQL/Vue/Go).
pub fn highlighter_for_extension(
    extension: Option<&str>,
) -> Option<crate::language::HighlighterFn> {
    crate::language::entry_for_extension(extension)?.highlighter
}

/// A syntax span's classification - `design_handoff_jerry_ade/README.md`'s File view
/// syntax-colour table ("keyword ... function ... type ... literal/self ... comment ...
/// punctuation/text").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
        HighlightKind::Keyword => theme::syntax::KEYWORD.into(),
        HighlightKind::Function => theme::syntax::FUNCTION.into(),
        HighlightKind::Type => theme::syntax::TYPE.into(),
        HighlightKind::Literal => theme::syntax::LITERAL.into(),
        HighlightKind::Comment => theme::syntax::COMMENT.into(),
        HighlightKind::Text => theme::syntax::TEXT.into(),
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

/// One language's real node-kind vocabulary, feeding the single shared [`walk_node`] walker -
/// see this module's top-level docs. Keeping one generic walker parameterized by a small table
/// per language (rather than three near-identical copies of the walk itself) is the same
/// "consolidate genuinely repeated shape, don't force an abstraction where fields would differ"
/// call this codebase's Revision R5.5 already established elsewhere; the fields themselves are
/// still just plain string-slice tables, no trait machinery.
struct Lexicon {
    /// Leaf-token kinds (`node.child_count() == 0`) whose literal text is a keyword.
    keywords: &'static [&'static str],
    /// Whole-node kinds classified as [`HighlightKind::Literal`] without descending into
    /// children (a string node's inner quote/content/escape sub-nodes render as one span, not
    /// three).
    literal_kinds: &'static [&'static str],
    /// Leaf texts, checked only when the leaf's own `kind()` is in [`Self::identifier_kinds`],
    /// that should still be classified [`HighlightKind::Literal`] - exists for Python's `self`
    /// specifically: unlike Rust (a dedicated `self`/`self_parameter` grammar node, already
    /// covered by [`Self::literal_kinds`] without needing this) or TypeScript (`this` is a real
    /// keyword token there), `tree-sitter-python`'s grammar has *no* distinct node kind for
    /// `self` at all - it parses as a perfectly ordinary `identifier`, indistinguishable by kind
    /// alone from any other name. Matching by literal text is the only way to give Python's
    /// `self` the same Literal treatment Rust's own `self` gets, rather than leaving the two
    /// languages inconsistently rendered for what plays the same syntactic role in both.
    literal_identifier_texts: &'static [&'static str],
    comment_kinds: &'static [&'static str],
    /// Whole-node kinds classified as [`HighlightKind::Type`] without descending into children.
    type_kinds: &'static [&'static str],
    /// Leaf kinds eligible for [`HighlightKind::Function`]/[`HighlightKind::Type`] when
    /// [`Self::declared_name_fields`] also matches that leaf's field name and its immediate
    /// parent's real node kind matches [`Self::function_name_parent_kinds`]/
    /// [`Self::type_name_parent_kinds`] respectively (Rust: `["identifier"]`; TypeScript
    /// additionally needs `"property_identifier"` for a class method's own name).
    identifier_kinds: &'static [&'static str],
    /// Field names under which an [`Self::identifier_kinds`] leaf *might* be a declared/called
    /// name, not a use - genuinely ambiguous on the field name alone (e.g. TypeScript's
    /// `variable_declarator`, `function_declaration`, `interface_body` member, and JSX tag all
    /// reuse the same `"name"` field for very different things), so [`walk_node`] also
    /// requires the leaf's immediate *parent* node kind to appear in
    /// [`Self::function_name_parent_kinds`]/[`Self::type_name_parent_kinds`] before actually
    /// classifying it as [`HighlightKind::Function`]/[`HighlightKind::Type`] - see those fields'
    /// own docs. Still the same narrow "declaration `name` field, or a plain-identifier call's
    /// `function` field" heuristic [`highlight_rust`] originally established (still doesn't cover
    /// `obj.foo()`'s `field_identifier`/`property_identifier` callee - an intentionally narrow,
    /// documented gap carried over unchanged into TypeScript/Python, not widened here).
    declared_name_fields: &'static [&'static str],
    /// Real parent node kinds under which an [`Self::identifier_kinds`] leaf whose field name is
    /// in [`Self::declared_name_fields`] is genuinely a function/method declaration's or a call
    /// expression's own name - e.g. Rust's `function_item`/`function_signature_item`/
    /// `call_expression`, not `variable_declarator`/`let_declaration`-shaped parents (Rust's own
    /// `let` binding uses a `pattern` field, never `name`, so it was never at risk - see
    /// [`RUST_LEXICON`]'s own docs; TypeScript/Python's `variable_declarator`/`class_definition`
    /// *do* reuse `"name"`, which is exactly the real, live-verified collision this field exists
    /// to rule out).
    function_name_parent_kinds: &'static [&'static str],
    /// Real parent node kinds under which an [`Self::identifier_kinds`] leaf whose field name is
    /// in [`Self::declared_name_fields`] is genuinely a type's own declared name rather than a
    /// function - exists specifically for Python's `class_definition`, whose `name` field is a
    /// plain `identifier` (unlike Rust/TypeScript, where a class/struct/enum name is already a
    /// distinct `type_identifier` node kind, caught by [`Self::type_kinds`] before this check is
    /// ever reached - see [`PYTHON_LEXICON`]'s own docs). Empty for languages that don't need it.
    type_name_parent_kinds: &'static [&'static str],
}

/// Rust keyword tokens - tree-sitter-rust's grammar represents each as an unnamed leaf node
/// whose `kind()` is the literal keyword text (see this module's tests). `self` is deliberately
/// not here - it's real, but classified as [`HighlightKind::Literal`] instead (see
/// [`RUST_LEXICON`]'s `literal_kinds`), matching how a self-reference reads visually closer to a
/// value than a keyword.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "super", "trait", "type", "union", "unsafe", "use", "where",
    "while", "yield",
];

const RUST_LEXICON: Lexicon = Lexicon {
    keywords: RUST_KEYWORDS,
    literal_kinds: &[
        "string_literal",
        "raw_string_literal",
        "char_literal",
        "integer_literal",
        "float_literal",
        "boolean_literal",
        "self",
    ],
    literal_identifier_texts: &[],
    comment_kinds: &["line_comment", "block_comment"],
    type_kinds: &["type_identifier", "primitive_type"],
    identifier_kinds: &["identifier"],
    declared_name_fields: &["name", "function"],
    // Verified for real by parsing real sample source with `tree-sitter-rust` and inspecting the
    // actual emitted tree while building this fix: a `fn` item's/trait method signature's own name
    // is `name: identifier` under `function_item`/`function_signature_item`; a plain-identifier
    // call's callee is `function: identifier` under `call_expression`. Rust's own `let` binding
    // uses a `pattern` field (never `name`), so it was never at risk of this collision in the
    // first place - see `declared_name_fields`' own docs.
    function_name_parent_kinds: &[
        "function_item",
        "function_signature_item",
        "call_expression",
    ],
    // Not needed for Rust: a struct/enum/trait name is already a distinct `type_identifier` node
    // kind, caught by `type_kinds` above before this would ever be reached.
    type_name_parent_kinds: &[],
};

/// TypeScript/TSX keyword tokens - verified for real against `tree-sitter-typescript@0.23.2`'s
/// own bundled `queries/highlights.scm` (its real keyword list) plus a direct parse probe of
/// sample source (see this module's top-level docs), not guessed. `this`/`super` are included
/// here rather than treated like Rust's `self` (a [`Lexicon::literal_kinds`] entry) purely for
/// simplicity - no test or real-world hover/highlight distinction in this app depends on which
/// bucket they land in.
const TYPESCRIPT_KEYWORDS: &[&str] = &[
    "abstract",
    "declare",
    "enum",
    "export",
    "implements",
    "interface",
    "keyof",
    "namespace",
    "private",
    "protected",
    "public",
    "type",
    "readonly",
    "override",
    "satisfies",
    "function",
    "const",
    "let",
    "var",
    "return",
    "class",
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "new",
    "delete",
    "typeof",
    "instanceof",
    "in",
    "of",
    "try",
    "catch",
    "finally",
    "throw",
    "yield",
    "async",
    "await",
    "import",
    "from",
    "extends",
    "as",
    "void",
    "get",
    "set",
    "this",
    "super",
    "static",
];

/// Real node kinds verified by parsing sample TypeScript source with
/// `tree_sitter_typescript::LANGUAGE_TYPESCRIPT` and inspecting the actual tree (see this
/// module's top-level docs) - `predefined_type` (the `number`/`string`/`boolean`/`void`/...
/// built-in type keywords) is a composite wrapper node, classified whole here rather than
/// descended into, which is exactly what keeps its one anonymous leaf child (also, confusingly,
/// often kind `"number"`/`"string"`/etc, colliding with the *literal* kinds below) from ever
/// being reached as a separate leaf - the two never collide in practice because this whole-node
/// check always wins first.
const TYPESCRIPT_LEXICON: Lexicon = Lexicon {
    keywords: TYPESCRIPT_KEYWORDS,
    literal_kinds: &[
        "string",
        "template_string",
        "number",
        "true",
        "false",
        "null",
        "undefined",
        "regex",
    ],
    literal_identifier_texts: &[],
    comment_kinds: &["comment"],
    type_kinds: &["type_identifier", "predefined_type"],
    identifier_kinds: &["identifier", "property_identifier"],
    declared_name_fields: &["name", "function"],
    // Verified for real by parsing real sample source with `tree-sitter-typescript` and inspecting
    // the actual emitted tree while building this fix: a real, live-verified bug this narrows away -
    // `variable_declarator`, `interface_body`'s `property_signature`, and a JSX tag
    // (`jsx_self_closing_element`/`jsx_opening_element`/`jsx_closing_element`) all *also* reuse
    // the `"name"` field, which is exactly why the old, parent-kind-unaware version misclassified
    // `const s: string = ...`'s `s`, every interface member name, and every TSX tag name as
    // Function. Only `function_declaration` (a real `fn`), `method_definition` (a real class
    // method's own name), and `call_expression` (a real plain-identifier call's callee) actually
    // are one.
    function_name_parent_kinds: &[
        "function_declaration",
        "method_definition",
        "call_expression",
    ],
    // Not needed for TypeScript: a class/interface name is already a distinct `type_identifier`
    // node kind, caught by `type_kinds` above before this would ever be reached.
    type_name_parent_kinds: &[],
};

/// Real node kinds verified by parsing sample Python source with `tree_sitter_python::LANGUAGE`
/// and inspecting the actual tree (see this module's top-level docs). `"type"` is the composite
/// wrapper both a parameter's and a return type's real annotation uses (`def f(x: int) -> None:`
/// puts both `int` and `None` inside one `type` node), classified whole the same way
/// TypeScript's `predefined_type` is above.
const PYTHON_KEYWORDS: &[&str] = &[
    "def", "class", "return", "if", "elif", "else", "for", "while", "in", "not", "and", "or", "is",
    "pass", "break", "continue", "import", "from", "as", "with", "try", "except", "finally",
    "raise", "lambda", "yield", "global", "nonlocal", "del", "assert", "async", "await",
];

const PYTHON_LEXICON: Lexicon = Lexicon {
    keywords: PYTHON_KEYWORDS,
    literal_kinds: &["string", "integer", "float", "true", "false", "none"],
    // Python's `self` has no dedicated grammar node the way Rust's does - see
    // `Lexicon::literal_identifier_texts`' own docs for why matching by leaf text is the only way
    // to give it the same Literal treatment Rust's own `self` gets (a deliberate, documented
    // choice to match Rust's convention, not an oversight - `self`/`this` play the same syntactic
    // role in both languages).
    literal_identifier_texts: &["self"],
    comment_kinds: &["comment"],
    type_kinds: &["type"],
    identifier_kinds: &["identifier"],
    declared_name_fields: &["name", "function"],
    // Verified for real by parsing real sample source with `tree-sitter-python` and inspecting the
    // actual emitted tree while building this fix: a `def`'s own name is `name: identifier` under
    // `function_definition`; a plain-identifier call's callee is `function: identifier` under
    // `call` (Python's call node kind - not `call_expression`, unlike Rust/TypeScript).
    function_name_parent_kinds: &["function_definition", "call"],
    // The real, live-verified bug this narrows away: `class_definition`'s own `name` field is a
    // plain `identifier` (unlike Rust/TypeScript, where a class/struct name is already a distinct
    // `type_identifier` node kind caught by `type_kinds` above) - without this, `class Foo:`
    // misclassified `Foo` as a Function instead of a Type.
    type_name_parent_kinds: &["class_definition"],
};

/// Parses `source` with `tree-sitter-rust` and walks the resulting AST into classified
/// [`HighlightSpan`]s. Returns an empty `Vec` (rather than panicking) if the grammar fails to
/// load or the parse produces no tree - neither expected in practice, but not assumed away.
pub fn highlight_rust(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, tree_sitter_rust::LANGUAGE.into(), &RUST_LEXICON)
}

/// Parses `source` with `tree-sitter-typescript` and walks the resulting AST into classified
/// [`HighlightSpan`]s, following [`highlight_rust`]'s exact shape. `is_tsx` selects the real TSX
/// grammar variant (used for `.tsx`/`.jsx` - TSX's grammar is a superset that also parses plain
/// JSX-free TypeScript/JavaScript correctly, and there is no separate JSX-only grammar in
/// `tree-sitter-typescript`) over the plain TypeScript one (used for `.ts`/`.js` - TypeScript's
/// grammar is itself a real syntactic superset of JavaScript, so `.js` deliberately reuses it
/// rather than adding a third grammar dependency for plain JavaScript).
pub fn highlight_typescript(source: &str, is_tsx: bool) -> Vec<HighlightSpan> {
    let language: tree_sitter::Language = if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    highlight_with(source, language, &TYPESCRIPT_LEXICON)
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

/// Parses `source` with `tree-sitter-python` and walks the resulting AST into classified
/// [`HighlightSpan`]s, following [`highlight_rust`]'s exact shape.
pub fn highlight_python(source: &str) -> Vec<HighlightSpan> {
    highlight_with(source, tree_sitter_python::LANGUAGE.into(), &PYTHON_LEXICON)
}

fn highlight_with(
    source: &str,
    language: tree_sitter::Language,
    lexicon: &Lexicon,
) -> Vec<HighlightSpan> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    walk_node(tree.root_node(), None, None, source, lexicon, &mut spans);
    spans
}

/// `field_name` is the field this node is held under on its immediate parent, if any (`None` for
/// the root, or an unnamed child) - `parent_kind` is that immediate parent's own real node kind
/// (also `None` for the root). Both are needed, together, to correctly classify a leaf whose
/// field name alone is ambiguous across several different real parent shapes - see
/// [`Lexicon::function_name_parent_kinds`]/[`Lexicon::type_name_parent_kinds`]'s own docs for why
/// the field name by itself (this walker's original, Rust-only design) isn't enough once
/// TypeScript/Python's grammars are in the mix. `source` is only consulted for
/// [`Lexicon::literal_identifier_texts`]'s leaf-text check (Python's `self`).
fn walk_node(
    node: tree_sitter::Node<'_>,
    field_name: Option<&str>,
    parent_kind: Option<&str>,
    source: &str,
    lexicon: &Lexicon,
    spans: &mut Vec<HighlightSpan>,
) {
    let kind = node.kind();

    if lexicon.comment_kinds.contains(&kind) {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: HighlightKind::Comment,
        });
        return;
    }
    if lexicon.literal_kinds.contains(&kind) {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: HighlightKind::Literal,
        });
        return;
    }
    if lexicon.type_kinds.contains(&kind) {
        spans.push(HighlightSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: HighlightKind::Type,
        });
        return;
    }

    if node.child_count() == 0 {
        let is_declared_name = lexicon.identifier_kinds.contains(&kind)
            && field_name.is_some_and(|field| lexicon.declared_name_fields.contains(&field));
        let classified = if lexicon.keywords.contains(&kind) {
            HighlightKind::Keyword
        } else if is_declared_name
            && parent_kind.is_some_and(|parent| lexicon.type_name_parent_kinds.contains(&parent))
        {
            HighlightKind::Type
        } else if is_declared_name
            && parent_kind
                .is_some_and(|parent| lexicon.function_name_parent_kinds.contains(&parent))
        {
            HighlightKind::Function
        } else if lexicon.identifier_kinds.contains(&kind)
            && node
                .utf8_text(source.as_bytes())
                .is_ok_and(|text| lexicon.literal_identifier_texts.contains(&text))
        {
            HighlightKind::Literal
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
            walk_node(child, child_field, Some(kind), source, lexicon, spans);
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

/// Real line-boundary byte ranges within `source`, excluding line-ending bytes (`\n`, and a
/// preceding `\r` for a CRLF line) - the same ranges [`build_lines`] slices `RenderedLine::text`
/// from. `pub(crate)` (not private) so `crate::edit_buffer::EditBuffer` can derive its own
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
/// `crate::root::code_surface`'s Diff view and `crate::root::merge_flow_render`'s Merge view so
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
pub(crate) fn highlight_block<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    extension: Option<&str>,
) -> Vec<RenderedLine> {
    let lines: Vec<&str> = lines.into_iter().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let source = lines.join("\n");
    let spans = match highlighter_for_extension(extension) {
        Some(highlighter) => highlighter(&source),
        None => Vec::new(),
    };
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

/// [`load_file`]'s real implementation, also handing back the decoded source text - used by
/// `crate::root::AdeApp::spawn_file_load` to lazily seed a `crate::edit_buffer::EditBuffer` from
/// the exact same background read/decode this already does, rather than a second, independent
/// disk read of the same file. `load_file` itself is now a thin wrapper that discards the source,
/// kept as the public entry point every other existing caller (and this module's own tests)
/// already uses unchanged.
pub fn load_file_with_source(path: &Path) -> io::Result<(ParsedFile, String)> {
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
    let spans = match highlighter_for_extension(extension) {
        Some(highlighter) => highlighter(&source),
        None => Vec::new(),
    };
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
    fn typescript_string_literal_is_classified_as_literal() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span = find_span(&spans, SAMPLE_TYPESCRIPT, "\"x\"").expect("string literal span");
        assert_eq!(span.kind, HighlightKind::Literal);
    }

    #[test]
    fn typescript_function_declaration_name_is_classified_as_function() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span = find_span(&spans, SAMPLE_TYPESCRIPT, "add").expect("function name span");
        assert_eq!(span.kind, HighlightKind::Function);
    }

    #[test]
    fn typescript_predefined_type_is_classified_as_type() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let type_spans: Vec<_> = spans
            .iter()
            .filter(|span| SAMPLE_TYPESCRIPT[span.start..span.end] == *"number")
            .collect();
        assert!(!type_spans.is_empty());
        assert!(type_spans
            .iter()
            .all(|span| span.kind == HighlightKind::Type));
    }

    #[test]
    fn typescript_doc_comment_is_classified_as_comment() {
        let spans = highlight_typescript(SAMPLE_TYPESCRIPT, false);
        let span =
            find_span(&spans, SAMPLE_TYPESCRIPT, "/** Adds one. */").expect("doc comment span");
        assert_eq!(span.kind, HighlightKind::Comment);
    }

    /// The real, live-verified regression this fix addresses: a `variable_declarator`'s own
    /// `name` field collides with `function_declaration`'s `name` field in
    /// `tree-sitter-typescript`'s real grammar, and the old, parent-kind-unaware matching
    /// misclassified every `const`/`let`/`var` binding's name as a Function. `s` here must be
    /// plain `Text` (a use/declaration of a variable, not a function).
    #[test]
    fn typescript_const_variable_name_is_not_misclassified_as_a_function() {
        // The audit's exact reproduction. `find_span`'s plain substring search would otherwise
        // match the embedded "s" inside "const" itself, so the real declared variable's own byte
        // offset (right after "const ") is computed explicitly instead.
        let source = "const s: string = \"hi\";\n";
        let variable_start = source.find("const ").expect("const") + "const ".len();
        let spans = highlight_typescript(source, false);
        let span = spans
            .iter()
            .find(|span| span.start == variable_start && span.end == variable_start + 1)
            .expect("variable name span");
        assert_ne!(
            span.kind,
            HighlightKind::Function,
            "a const/let/var binding's own name must never be classified as a function"
        );
    }

    /// The same real collision, for an `interface` member name (`property_signature`'s `name`
    /// field) - must not be classified as a function either.
    #[test]
    fn typescript_interface_member_name_is_not_misclassified_as_a_function() {
        let source = "interface Point { x: number }\n";
        let spans = highlight_typescript(source, false);
        let span = find_span(&spans, source, "x").expect("interface member name span");
        assert_ne!(span.kind, HighlightKind::Function);
    }

    /// The same real collision, for a class method's own name (`method_definition`'s `name`
    /// field, a `property_identifier`) - this one, unlike the two above, genuinely *should* be
    /// classified as a function.
    #[test]
    fn typescript_class_method_name_is_classified_as_a_function() {
        let source = "class Point {\n    length() {\n        return 0;\n    }\n}\n";
        let spans = highlight_typescript(source, false);
        let span = find_span(&spans, source, "length").expect("method name span");
        assert_eq!(span.kind, HighlightKind::Function);
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
        assert!(function_spans
            .iter()
            .all(|span| span.kind == HighlightKind::Function));
    }

    /// A real TSX tag name (`jsx_self_closing_element`'s own `name` field) is the same real
    /// collision one more time - must not render as a Function either.
    #[test]
    fn tsx_tag_name_is_not_misclassified_as_a_function() {
        let source = "const el = <div />;\n";
        let spans = highlight_typescript(source, true);
        let span = find_span(&spans, source, "div").expect("tag name span");
        assert_ne!(span.kind, HighlightKind::Function);
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
    fn python_string_literal_is_classified_as_literal() {
        let spans = highlight_python(SAMPLE_PYTHON);
        let span = find_span(&spans, SAMPLE_PYTHON, "\"x\"").expect("string literal span");
        assert_eq!(span.kind, HighlightKind::Literal);
    }

    #[test]
    fn python_function_definition_name_is_classified_as_function() {
        let spans = highlight_python(SAMPLE_PYTHON);
        let span = find_span(&spans, SAMPLE_PYTHON, "add").expect("function name span");
        assert_eq!(span.kind, HighlightKind::Function);
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

    /// Matches Rust's own `self_is_classified_as_literal_not_keyword` test - a deliberate,
    /// documented choice that Python's `self` gets the same Literal treatment Rust's does (see
    /// `PYTHON_LEXICON`'s own docs on why this needs a leaf-text check, unlike Rust's dedicated
    /// grammar node).
    #[test]
    fn python_self_is_classified_as_literal_not_a_plain_identifier() {
        let source = "class Foo:\n    def bar(self):\n        return self.value\n";
        let spans = highlight_python(source);
        let self_spans: Vec<_> = spans
            .iter()
            .filter(|span| source[span.start..span.end] == *"self")
            .collect();
        assert!(!self_spans.is_empty());
        assert!(self_spans
            .iter()
            .all(|span| span.kind == HighlightKind::Literal));
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
        assert_eq!(span.kind, HighlightKind::Function);
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
        assert!(function_spans
            .iter()
            .all(|span| span.kind == HighlightKind::Function));
    }

    #[test]
    fn highlighting_invalid_python_still_returns_a_real_non_empty_span_list() {
        let spans = highlight_python("def (((( broken");
        assert!(spans.iter().any(|span| span.kind == HighlightKind::Keyword));
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
        assert!(highlighter_for_extension(Some("toml")).is_none());
        assert!(highlighter_for_extension(Some("md")).is_none());
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
        let rendered = highlight_block(lines.iter().copied(), Some("rs"));
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
                text.as_ref() == "highlighter_for_extension" && *kind == HighlightKind::Function
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
        let rendered = highlight_block(lines.iter().copied(), Some("py"));
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
            .any(|(text, kind)| text.as_ref() == "\"left\"" && *kind == HighlightKind::Literal);
        assert!(
            has_string_literal,
            "the real string literal should be classified as Literal"
        );
    }

    #[test]
    fn highlight_block_on_an_unregistered_extension_is_all_plain_text() {
        let rendered = highlight_block(["key = \"value\"", "other = 1"], Some("toml"));
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
        let rendered = highlight_block(std::iter::empty(), Some("rs"));
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
        let rendered = highlight_block([""], Some("rs"));
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].text, "");
    }
}
