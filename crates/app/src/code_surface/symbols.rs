//! The File view breadcrumb's **symbol path** - the real chain of enclosing declarations around
//! wherever the caret currently sits (`design_handoff_jerry_ade/README.md`: "Breadcrumb 26
//! (`src › db › query_builder.rs › impl QueryBuilder › build`, ...)").
//!
//! GitHub issue #178: the breadcrumb band used to render only `code_view::breadcrumb_segments`'
//! path components, which the Surface C toolbar directly above it already shows - a literal
//! duplicate. This module supplies the half that was missing.
//!
//! ## Why a flat span list rather than a retained `tree_sitter::Tree`
//!
//! The obvious implementation is to keep the parsed tree alive and, on every caret move, call
//! `Node::descendant_for_byte_range(offset, offset)` and walk `parent()` upwards. That is
//! genuinely cheaper per lookup, and it is deliberately not what this does. A retained tree is
//! only valid against the exact source text it was parsed from, so it would have to be kept in
//! lockstep with `crate::code_surface::edit_buffer::EditBuffer::content` through every splice -
//! and `EditBuffer` is cloned, snapshotted and compared in several places that have no business
//! knowing about grammar lifetimes.
//!
//! [`symbol_outline`] instead flattens the tree **once per parse**, on the same background
//! executor that already re-highlights the file, into plain owned data: a preorder list of
//! `(byte range, label)`. Looking a caret up in it ([`symbol_path_at`]) is a linear scan over a
//! few hundred entries - far below a frame budget, measured against nothing more exotic than the
//! fact that this file's own outline has 40-odd entries. Plain data also means the outline can be
//! unit-tested against real parses with no GPUI window and no `EditBuffer` at all, which is what
//! this module's own tests do.
//!
//! ## Staleness while typing
//!
//! The outline is recomputed by `crate::code_surface::editing::AdeApp::schedule_rehighlight`,
//! i.e. 150ms after the last keystroke, exactly like the syntax highlighting it rides along with.
//! Between an edit and that refresh, the stored byte ranges describe the *pre-edit* text: spans
//! that start after the caret are shifted by the edit's own delta. This is deliberate and is not
//! hidden - the enclosing chain at the caret is built from spans whose `start` is *before* the
//! edit (so still correct) and whose `end` may be off by the edit's length, which can only matter
//! in the narrow window where the caret sits within that many bytes of a symbol's closing
//! boundary. The alternative - clearing the outline on every keystroke - would make the
//! breadcrumb blink empty while typing, which is a worse lie than being 150ms behind.
//!
//! ## Which languages are covered, and why not all of them
//!
//! Rust, TypeScript/TSX (and therefore `.js`/`.jsx`, which this app parses with the TypeScript
//! grammars - see `code_view::Grammar::for_extension`), Python and Go. Those are the languages
//! whose "what is an enclosing declaration" node kinds were each read out of the grammar's own
//! bundled `src/node-types.json` in the pinned crate on disk, field names included, and are
//! asserted by real parses in this module's tests.
//!
//! C is deliberately absent despite being a supported highlight grammar: `tree-sitter-c`'s
//! `function_definition` has no `name` field at all (its fields are `declarator`/`type`, checked
//! in `tree-sitter-c-0.24.2/src/node-types.json`), so producing `foo` from `static int *foo(void)`
//! means walking down through `pointer_declarator`/`function_declarator` by hand - real work with
//! its own failure modes, and no shared abstraction with the four languages here. TOML, JSON,
//! YAML, Markdown, HTML and CSS have no enclosing *symbol* concept worth a breadcrumb crumb at
//! all. For any of those, [`symbol_outline`] returns an empty outline and the breadcrumb honestly
//! shows the path alone.

use std::ops::Range;

use tree_sitter::{Node, Parser};

use crate::code_surface::code_view::Grammar;

/// One enclosing declaration found in a real parse: the byte range it spans in the source it was
/// parsed from, and the crumb text the breadcrumb renders for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSpan {
    /// Half-open, exactly `tree_sitter::Node::byte_range()` - see [`symbol_path_at`] for why the
    /// half-open end matters for a caret parked on a closing brace.
    pub byte_range: Range<usize>,
    /// What the breadcrumb shows: `build`, `impl QueryBuilder`, `class Foo`, `mod db`, ...
    pub label: String,
}

/// Which of the four languages with real enclosing-declaration support a [`Grammar`] maps to.
/// Separate from [`Grammar`] itself so the "is there a symbol outline for this file" answer is a
/// single exhaustive `match` that a newly added grammar cannot silently fall through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolLanguage {
    Rust,
    /// Both `tree-sitter-typescript` grammars (`typescript` and `tsx`). Their declaration node
    /// kinds are the same set - the TSX grammar is the TypeScript one plus JSX expressions - so
    /// one label extractor serves both, which is why `Grammar::TypeScript` and `Grammar::Tsx`
    /// both land here.
    TypeScript,
    Python,
    Go,
}

impl SymbolLanguage {
    /// `None` for every grammar this module deliberately does not cover - see this module's own
    /// docs for the per-language reasoning. Exhaustive on purpose (no `_` arm): adding a grammar
    /// to [`Grammar`] must force a decision here rather than silently defaulting to "no symbols".
    fn for_grammar(grammar: Grammar) -> Option<Self> {
        match grammar {
            Grammar::Rust => Some(SymbolLanguage::Rust),
            Grammar::TypeScript | Grammar::Tsx => Some(SymbolLanguage::TypeScript),
            Grammar::Python => Some(SymbolLanguage::Python),
            Grammar::Go => Some(SymbolLanguage::Go),
            Grammar::C
            | Grammar::Toml
            | Grammar::Json
            | Grammar::Yaml
            | Grammar::Markdown
            | Grammar::MarkdownInline
            | Grammar::Html
            | Grammar::Css => None,
        }
    }
}

/// Parses `source` with the grammar `extension` maps to and flattens every enclosing declaration
/// in it into a preorder (outermost first) [`SymbolSpan`] list.
///
/// Empty - never a fabricated placeholder - when `extension` has no grammar at all, has a grammar
/// this module doesn't cover, or the parse itself fails. The empty case is what the breadcrumb
/// renders as "path segments only", which is the honest rendering for a file whose language has
/// no symbol nesting to report.
///
/// Runs a real `tree_sitter::Parser::parse` (~16ms on this repository's largest file, measured in
/// `code_view`'s own module docs), so every production caller runs it on
/// `cx.background_executor()`, never in a render pass.
pub fn symbol_outline(source: &str, extension: Option<&str>) -> Vec<SymbolSpan> {
    let Some(grammar) = extension.and_then(Grammar::for_extension) else {
        return Vec::new();
    };
    let Some(language) = SymbolLanguage::for_grammar(grammar) else {
        return Vec::new();
    };

    let mut parser = Parser::new();
    if parser.set_language(&grammar.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    collect(tree.root_node(), source, language, &mut spans);
    spans
}

/// Preorder walk: a node's own crumb (if it has one) is pushed before its children's, so the
/// resulting list is already outermost-first for any nested chain - which is exactly the order
/// [`symbol_path_at`] hands to the breadcrumb, with no sorting pass.
fn collect(node: Node, source: &str, language: SymbolLanguage, spans: &mut Vec<SymbolSpan>) {
    if let Some(label) = label_for(node, source, language) {
        spans.push(SymbolSpan {
            byte_range: node.byte_range(),
            label,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, language, spans);
    }
}

/// The real crumb chain enclosing `offset`, outermost first - `["impl QueryBuilder", "build"]` for
/// a caret inside `QueryBuilder::build`.
///
/// Containment is **half-open** (`start <= offset < end`), matching `tree_sitter`'s own
/// `descendant_for_byte_range` convention: a caret parked immediately *after* a function's closing
/// brace is genuinely outside that function, and reporting it as still inside would be a visible
/// lie the moment the user arrows past the brace.
pub fn symbol_path_at(spans: &[SymbolSpan], offset: usize) -> Vec<&str> {
    spans
        .iter()
        .filter(|span| span.byte_range.start <= offset && offset < span.byte_range.end)
        .map(|span| span.label.as_str())
        .collect()
}

/// The crumb for `node`, or `None` if it isn't an enclosing declaration in `language`.
fn label_for(node: Node, source: &str, language: SymbolLanguage) -> Option<String> {
    if !node.is_named() {
        return None;
    }
    match language {
        SymbolLanguage::Rust => rust_label(node, source),
        SymbolLanguage::TypeScript => typescript_label(node, source),
        SymbolLanguage::Python => python_label(node, source),
        SymbolLanguage::Go => go_label(node, source),
    }
}

/// `node`'s `field`-named child rendered as source text, or `None` when the field is absent (every
/// one of these fields is optional in at least one real shape - `impl_item` has no `trait` for an
/// inherent impl, `function_expression` has no `name` for an anonymous one).
fn field_text<'a>(node: Node, source: &'a str, field: &str) -> Option<&'a str> {
    node.child_by_field_name(field)?
        .utf8_text(source.as_bytes())
        .ok()
}

/// Node kinds and field names read from `tree-sitter-rust-0.24.2/src/node-types.json`:
/// `function_item`/`function_signature_item`/`mod_item`/`struct_item`/`enum_item`/`union_item`/
/// `trait_item`/`macro_definition` all carry a `name` field; `impl_item` carries `type` and an
/// optional `trait` (and no `name` at all), which is why it is the one kind assembled by hand.
fn rust_label(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "function_item" | "function_signature_item" => {
            field_text(node, source, "name").map(str::to_string)
        }
        "impl_item" => {
            let type_name = field_text(node, source, "type")?;
            Some(match field_text(node, source, "trait") {
                Some(trait_name) => format!("impl {trait_name} for {type_name}"),
                None => format!("impl {type_name}"),
            })
        }
        "trait_item" => field_text(node, source, "name").map(|name| format!("trait {name}")),
        "mod_item" => field_text(node, source, "name").map(|name| format!("mod {name}")),
        "struct_item" => field_text(node, source, "name").map(|name| format!("struct {name}")),
        "enum_item" => field_text(node, source, "name").map(|name| format!("enum {name}")),
        "union_item" => field_text(node, source, "name").map(|name| format!("union {name}")),
        "macro_definition" => field_text(node, source, "name").map(|name| format!("{name}!")),
        _ => None,
    }
}

/// Node kinds and field names read from `tree-sitter-typescript-0.23.2/typescript/src/
/// node-types.json`. Note `class` and `class_declaration` are two genuinely different kinds there
/// (a class *expression* versus a statement), both with a `name` field, and both are covered.
///
/// `arrow_function` has no `name` field at all, so `const handler = () => {}` and a class's
/// `handler = () => {}` field are resolved through the parent that *does* name them -
/// `variable_declarator` (fields `name`/`type`/`value`) and `public_field_definition` (fields
/// `decorator`/`name`/`type`/`value`) respectively. An arrow function with neither parent (an
/// inline callback) genuinely has no name and contributes no crumb, rather than an invented one.
fn typescript_label(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" | "class" => {
            field_text(node, source, "name").map(|name| format!("class {name}"))
        }
        "interface_declaration" => {
            field_text(node, source, "name").map(|name| format!("interface {name}"))
        }
        "enum_declaration" => field_text(node, source, "name").map(|name| format!("enum {name}")),
        "internal_module" => {
            field_text(node, source, "name").map(|name| format!("namespace {name}"))
        }
        "module" => field_text(node, source, "name").map(|name| format!("module {name}")),
        "function_declaration" | "generator_function_declaration" | "method_definition" => {
            field_text(node, source, "name").map(str::to_string)
        }
        "function_expression" | "arrow_function" => field_text(node, source, "name")
            .map(str::to_string)
            .or_else(|| named_by_enclosing_binding(node, source)),
        _ => None,
    }
}

/// The name a `variable_declarator`/`public_field_definition` parent gives an otherwise-anonymous
/// function value. Only the *direct* parent is consulted: `const a = foo(() => {})` must not
/// borrow `a`'s name for the inner callback, and it doesn't, because that callback's parent is an
/// `arguments` node, not a declarator.
fn named_by_enclosing_binding(node: Node, source: &str) -> Option<String> {
    let parent = node.parent()?;
    match parent.kind() {
        "variable_declarator" | "public_field_definition" => {
            field_text(parent, source, "name").map(str::to_string)
        }
        _ => None,
    }
}

/// Node kinds and field names read from `tree-sitter-python-0.25.0/src/node-types.json`:
/// `function_definition` and `class_definition` both carry a `name` field. `decorated_definition`
/// wraps them rather than replacing them, so a decorated function is still reached as its own
/// `function_definition` child by the walk in [`collect`] and needs no special case.
fn python_label(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "function_definition" => field_text(node, source, "name").map(str::to_string),
        "class_definition" => field_text(node, source, "name").map(|name| format!("class {name}")),
        _ => None,
    }
}

/// Node kinds and field names read from `tree-sitter-go-0.25.0/src/node-types.json`:
/// `function_declaration` (fields include `name`), `method_declaration` (also `receiver`) and
/// `type_spec` (field `name`; it is the child of `type_declaration`, which carries no fields of
/// its own, so the crumb hangs off the spec).
///
/// A method's crumb is its bare name, not `(*T) Name`: the receiver field's own source text is
/// the whole parameter list (`(b *QueryBuilder)`), and picking the type out of it means parsing a
/// second declarator shape for one cosmetic gain. Go has no lexical nesting of declarations
/// anyway, so a method crumb never appears beside a type crumb the way Rust's `impl` does.
fn go_label(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            field_text(node, source, "name").map(str::to_string)
        }
        "type_spec" => field_text(node, source, "name").map(|name| format!("type {name}")),
        _ => None,
    }
}

#[cfg(test)]
mod symbol_outline_tests {
    use super::*;

    /// The byte offset just after the first occurrence of `needle` in `source` - how every test
    /// below places a caret at a real position in real source text rather than a magic number.
    fn offset_after(source: &str, needle: &str) -> usize {
        source.find(needle).expect("needle present in source") + needle.len()
    }

    fn path_at(source: &str, extension: &str, needle: &str) -> Vec<String> {
        let spans = symbol_outline(source, Some(extension));
        let offset = offset_after(source, needle);
        symbol_path_at(&spans, offset)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    const RUST_SOURCE: &str = r#"
mod db {
    pub struct QueryBuilder {
        projection: Vec<String>,
    }

    impl QueryBuilder {
        pub fn select(mut self, cols: &[&str]) -> Self {
            self.projection.extend(cols.iter().map(|c| c.to_string()));
            self
        }

        pub fn build(&self) -> String {
            let marker_inside_build = 1;
            String::new()
        }
    }

    impl Default for QueryBuilder {
        fn default() -> Self {
            Self { projection: Vec::new() }
        }
    }
}

fn free_standing() {
    let marker_inside_free = 2;
}
"#;

    #[test]
    fn a_caret_inside_a_real_rust_method_reports_the_designs_own_mod_impl_fn_chain() {
        assert_eq!(
            path_at(RUST_SOURCE, "rs", "marker_inside_build"),
            vec!["mod db", "impl QueryBuilder", "build"],
        );
    }

    #[test]
    fn moving_the_caret_to_a_sibling_method_really_changes_the_last_crumb() {
        let spans = symbol_outline(RUST_SOURCE, Some("rs"));
        let in_build = offset_after(RUST_SOURCE, "marker_inside_build");
        let in_select = offset_after(RUST_SOURCE, "self.projection.extend");
        assert_eq!(
            symbol_path_at(&spans, in_build),
            vec!["mod db", "impl QueryBuilder", "build"],
        );
        assert_eq!(
            symbol_path_at(&spans, in_select),
            vec!["mod db", "impl QueryBuilder", "select"],
        );
    }

    #[test]
    fn moving_the_caret_out_of_every_declaration_really_empties_the_symbol_path() {
        let spans = symbol_outline(RUST_SOURCE, Some("rs"));
        // The very first byte of the file: before `mod db` starts, so genuinely inside nothing.
        assert!(symbol_path_at(&spans, 0).is_empty());
        // Inside a free function at the file's top level: exactly one crumb, no `mod`.
        assert_eq!(
            symbol_path_at(&spans, offset_after(RUST_SOURCE, "marker_inside_free")),
            vec!["free_standing"],
        );
    }

    #[test]
    fn a_rust_trait_impl_names_both_the_trait_and_the_type() {
        assert_eq!(
            path_at(RUST_SOURCE, "rs", "Self { projection"),
            vec!["mod db", "impl Default for QueryBuilder", "default"],
        );
    }

    #[test]
    fn a_caret_inside_a_rust_struct_body_reports_the_struct_crumb() {
        assert_eq!(
            path_at(RUST_SOURCE, "rs", "projection: Vec<String>"),
            vec!["mod db", "struct QueryBuilder"],
        );
    }

    #[test]
    fn a_caret_immediately_after_a_functions_closing_brace_is_outside_it() {
        let source = "fn a() {\n    let x = 1;\n}\nfn b() {}\n";
        let spans = symbol_outline(source, Some("rs"));
        let inside = offset_after(source, "let x = 1;");
        assert_eq!(symbol_path_at(&spans, inside), vec!["a"]);
        // `fn a`'s node ends exactly at its closing brace; one byte past it is the newline
        // between the two functions, which belongs to neither.
        let after_brace = source.find("}\nfn b").expect("closing brace") + 1;
        assert!(
            symbol_path_at(&spans, after_brace).is_empty(),
            "a caret past the closing brace must report no enclosing symbol"
        );
    }

    #[test]
    fn a_rust_trait_and_its_default_method_both_appear() {
        let source = "trait Query {\n    fn run(&self) -> usize {\n        7\n    }\n}\n";
        assert_eq!(
            path_at(source, "rs", "        7"),
            vec!["trait Query", "run"]
        );
    }

    const TYPESCRIPT_SOURCE: &str = r#"
namespace db {
  export class QueryBuilder {
    private projection: string[] = [];

    build(): string {
      const markerInsideBuild = 1;
      return "";
    }

    reset = () => {
      const markerInsideReset = 2;
    };
  }

  export interface Query {
    run(): number;
  }
}

const handler = (event: string) => {
  const markerInsideHandler = 3;
};

function freeStanding() {
  const markerInsideFree = 4;
}
"#;

    #[test]
    fn a_caret_inside_a_real_typescript_method_reports_namespace_class_and_method() {
        assert_eq!(
            path_at(TYPESCRIPT_SOURCE, "ts", "markerInsideBuild"),
            vec!["namespace db", "class QueryBuilder", "build"],
        );
    }

    #[test]
    fn a_typescript_arrow_function_borrows_the_name_of_whatever_binds_it() {
        assert_eq!(
            path_at(TYPESCRIPT_SOURCE, "ts", "markerInsideHandler"),
            vec!["handler"],
        );
        assert_eq!(
            path_at(TYPESCRIPT_SOURCE, "ts", "markerInsideReset"),
            vec!["namespace db", "class QueryBuilder", "reset"],
        );
    }

    #[test]
    fn a_typescript_interface_is_a_real_crumb() {
        assert_eq!(
            path_at(TYPESCRIPT_SOURCE, "ts", "run(): number;"),
            vec!["namespace db", "interface Query"],
        );
    }

    #[test]
    fn an_anonymous_inline_callback_contributes_no_invented_crumb() {
        // `rows` binds a *call expression*, not a function, so it is not a declaration at all -
        // and the arrow function's own parent is the call's `arguments` node, not a declarator.
        // Both halves must stay silent: borrowing `rows` for this callback would put a caret
        // inside an inline lambda under a crumb that names something it isn't.
        let source = "const rows = items.map((item) => {\n  return item.id;\n});\n";
        assert!(path_at(source, "ts", "return item.id;").is_empty());
        // The contrast case, proving the silence above is about *this* shape and not about arrow
        // functions generally: bind the same arrow directly and it does get its binding's name.
        let bound = "const pick = (item) => {\n  return item.id;\n};\n";
        assert_eq!(path_at(bound, "ts", "return item.id;"), vec!["pick"]);
    }

    #[test]
    fn tsx_uses_the_same_declaration_kinds_as_plain_typescript() {
        let source = "export function Panel() {\n  const label = 1;\n  return <div />;\n}\n";
        assert_eq!(path_at(source, "tsx", "const label = 1;"), vec!["Panel"]);
    }

    #[test]
    fn a_javascript_file_is_parsed_with_the_typescript_grammar_and_still_gets_crumbs() {
        let source = "class Widget {\n  render() {\n    const inner = 1;\n  }\n}\n";
        assert_eq!(
            path_at(source, "js", "const inner = 1;"),
            vec!["class Widget", "render"],
        );
    }

    const PYTHON_SOURCE: &str = r#"
class QueryBuilder:
    def build(self):
        marker_inside_build = 1
        return ""

    @staticmethod
    def decorated():
        marker_inside_decorated = 2


def free_standing():
    def nested():
        marker_inside_nested = 3
    return nested
"#;

    #[test]
    fn a_caret_inside_a_real_python_method_reports_class_and_method() {
        assert_eq!(
            path_at(PYTHON_SOURCE, "py", "marker_inside_build"),
            vec!["class QueryBuilder", "build"],
        );
    }

    #[test]
    fn a_decorated_python_method_is_still_reached_through_its_own_definition_node() {
        assert_eq!(
            path_at(PYTHON_SOURCE, "py", "marker_inside_decorated"),
            vec!["class QueryBuilder", "decorated"],
        );
    }

    #[test]
    fn a_python_function_nested_in_another_function_reports_both() {
        assert_eq!(
            path_at(PYTHON_SOURCE, "py", "marker_inside_nested"),
            vec!["free_standing", "nested"],
        );
    }

    const GO_SOURCE: &str = r#"
package db

type QueryBuilder struct {
	projection []string
}

func (b *QueryBuilder) Build() string {
	markerInsideBuild := 1
	return ""
}

func FreeStanding() {
	markerInsideFree := 2
}
"#;

    #[test]
    fn a_caret_inside_a_real_go_method_reports_the_method_name() {
        assert_eq!(path_at(GO_SOURCE, "go", "markerInsideBuild"), vec!["Build"]);
        assert_eq!(
            path_at(GO_SOURCE, "go", "markerInsideFree"),
            vec!["FreeStanding"],
        );
    }

    #[test]
    fn a_go_struct_type_is_a_real_crumb() {
        assert_eq!(
            path_at(GO_SOURCE, "go", "projection []string"),
            vec!["type QueryBuilder"],
        );
    }

    #[test]
    fn a_language_without_symbol_support_yields_an_empty_outline_rather_than_a_guess() {
        // Real TOML/JSON/Markdown sources, all of which this app really does highlight - they
        // just have no enclosing-declaration concept, so the breadcrumb shows the path alone.
        assert!(symbol_outline("[package]\nname = \"ade\"\n", Some("toml")).is_empty());
        assert!(symbol_outline("{\"a\": 1}\n", Some("json")).is_empty());
        assert!(symbol_outline("# Heading\n\ntext\n", Some("md")).is_empty());
    }

    #[test]
    fn an_unknown_or_absent_extension_yields_an_empty_outline() {
        assert!(symbol_outline("SELECT 1;\n", Some("sql")).is_empty());
        assert!(symbol_outline("fn main() {}\n", None).is_empty());
    }

    #[test]
    fn malformed_source_still_produces_whatever_really_parsed_rather_than_panicking() {
        // Tree-sitter always yields a best-effort tree; the complete function before the broken
        // tail is genuinely there, so it is genuinely reported.
        let source = "fn good() {\n    let x = 1;\n}\n\nfn bad( {{{\n";
        let spans = symbol_outline(source, Some("rs"));
        assert_eq!(
            symbol_path_at(&spans, offset_after(source, "let x = 1;")),
            vec!["good"],
        );
    }

    #[test]
    fn the_outline_is_preorder_so_the_reported_chain_is_outermost_first() {
        let spans = symbol_outline(RUST_SOURCE, Some("rs"));
        let labels: Vec<&str> = spans.iter().map(|span| span.label.as_str()).collect();
        let mod_index = labels.iter().position(|l| *l == "mod db").expect("mod db");
        let impl_index = labels
            .iter()
            .position(|l| *l == "impl QueryBuilder")
            .expect("impl QueryBuilder");
        let fn_index = labels.iter().position(|l| *l == "build").expect("build");
        assert!(
            mod_index < impl_index && impl_index < fn_index,
            "preorder walk must emit enclosing declarations before enclosed ones, got {labels:?}"
        );
    }
}
