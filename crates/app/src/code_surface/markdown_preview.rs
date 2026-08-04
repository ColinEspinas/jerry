//! GitHub issue #115's Markdown preview: a real `tree-sitter-md` CST walk (headings, paragraphs,
//! lists, block quotes, fenced/indented code, tables, thematic breaks; inline emphasis/strong/
//! code spans/links/images) into a small owned tree ([`MdBlock`]/[`MdInline`]), rendered as
//! nested styled GPUI elements by `super::render`. Distinct from
//! `super::code_view::highlight_markdown`, which flattens the same grammar into one-dimensional
//! colored *source-text* runs for the Source view - this module keeps real block/inline
//! structure (heading level, list nesting, table shape) that a flat run list has already thrown
//! away.
//!
//! ## Scope (deliberately v1)
//!
//! - Reference-style (`[text][ref]`) and shortcut (`[text]`) links are not resolved against their
//!   `link_reference_definition` - only inline links (`` [text](url) ``) render as real, styled
//!   links. A reference/shortcut link's visible text still renders (as plain text via the generic
//!   gap-capture in [`build_inline_run`]), just without link styling or a destination - not
//!   dropped, just not specially recognized.
//! - Table cells render as plain trimmed text, not further inline-parsed - a cell containing
//!   `**bold**` shows the literal asterisks rather than real bold text.
//! - Images render as a small `[image: alt]` placeholder, not the fetched picture - this is a
//!   text preview, not a browser; no destination is even loaded, so a missing/broken image
//!   destination can never break the preview.
//! - GitHub task-list checkboxes (`- [ ] foo`) are not specially recognized - a task item renders
//!   as an ordinary bullet, marker text included in its own paragraph text like any other list
//!   item content.
//!
//! None of this is a silent gap: every real CommonMark block kind either renders for real or is
//! silently skipped as a *block* (HTML blocks, link reference definitions - neither has a
//! sensible visual form in a preview), never partially rendered as raw leftover markup syntax.

use tree_sitter::{Node, Parser};

/// One block-level markdown element.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MdBlock {
    Heading {
        level: u8,
        inline: Vec<MdInline>,
    },
    Paragraph(Vec<MdInline>),
    CodeBlock {
        language: Option<String>,
        text: String,
    },
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<MdBlock>>,
    },
    BlockQuote(Vec<MdBlock>),
    ThematicBreak,
    /// `header`/each `rows` entry are already-trimmed cell text - see this module's own
    /// "Scope" docs for why cells are not further inline-parsed.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

/// One inline (within-paragraph) markdown span.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MdInline {
    Text(String),
    Emphasis(Vec<MdInline>),
    Strong(Vec<MdInline>),
    Code(String),
    Link {
        text: Vec<MdInline>,
        destination: String,
    },
    /// Alt text only - see this module's own "Scope" docs for why the destination is never even
    /// read.
    Image {
        alt: String,
    },
    LineBreak,
}

/// Parses `source` as a real CommonMark document and returns its top-level blocks. Returns an
/// empty `Vec` (rather than panicking) if the grammar failed to load or the parse produced no
/// tree - neither expected in practice, matching `code_view::highlight_with`'s own posture for
/// the identical "should never happen, but degrade instead of panic" case.
pub(crate) fn parse_markdown(source: &str) -> Vec<MdBlock> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    parse_blocks(tree.root_node(), source)
}

/// Walks `node`'s direct children into real [`MdBlock`]s. `"section"` (the block grammar's own
/// heading-scoping wrapper - see this module's own scratch findings, not guessed) is transparent:
/// its children are spliced straight into the parent's block list, so a document's real top-level
/// block order survives regardless of how many heading levels wrap it.
fn parse_blocks(node: Node, source: &str) -> Vec<MdBlock> {
    let mut blocks = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "section" => blocks.extend(parse_blocks(child, source)),
            "atx_heading" => blocks.push(MdBlock::Heading {
                level: atx_heading_level(child),
                inline: child
                    .child_by_field_name("heading_content")
                    .map(|inline| parse_inline_node(inline, source))
                    .unwrap_or_default(),
            }),
            "setext_heading" => blocks.push(MdBlock::Heading {
                level: setext_heading_level(child),
                inline: child
                    .child_by_field_name("heading_content")
                    .and_then(|paragraph| first_child_of_kind(paragraph, "inline"))
                    .map(|inline| parse_inline_node(inline, source))
                    .unwrap_or_default(),
            }),
            "paragraph" => {
                let inline = first_child_of_kind(child, "inline")
                    .map(|inline| parse_inline_node(inline, source))
                    .unwrap_or_default();
                if !inline.is_empty() {
                    blocks.push(MdBlock::Paragraph(inline));
                }
            }
            "fenced_code_block" => blocks.push(parse_fenced_code_block(child, source)),
            "indented_code_block" => blocks.push(MdBlock::CodeBlock {
                language: None,
                text: strip_indented_code_prefix(&source[child.byte_range()]),
            }),
            "list" => blocks.push(parse_list(child, source)),
            "block_quote" => blocks.push(MdBlock::BlockQuote(parse_blocks(child, source))),
            "thematic_break" => blocks.push(MdBlock::ThematicBreak),
            "pipe_table" => blocks.push(parse_pipe_table(child, source)),
            // Markers/continuation noise inside a containing block (list item bullets, block
            // quote `>` markers, multi-line continuation markers) - real content, already
            // consumed by the block kinds above; nothing further to render for the marker itself.
            // Every other unmatched kind (html_block, link_reference_definition, task list
            // markers - see this module's own "Scope" docs) is likewise silently dropped as a
            // *block*, never rendered as raw leftover syntax.
            _ => {}
        }
    }
    blocks
}

fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.child_count()).find_map(|i| {
        let child = node.child(i as u32)?;
        (child.kind() == kind).then_some(child)
    })
}

/// `"atx_h3_marker"` -> `3`, matching this grammar's own real marker-kind naming (verified via a
/// real parse's `to_sexp()`, not assumed) - falls back to `1` only if the grammar ever changes
/// shape under us, never panics.
fn atx_heading_level(atx_heading: Node) -> u8 {
    atx_heading
        .child(0)
        .and_then(|marker| marker.kind().strip_prefix("atx_h"))
        .and_then(|rest| rest.strip_suffix("_marker"))
        .and_then(|digits| digits.parse::<u8>().ok())
        .unwrap_or(1)
}

fn setext_heading_level(setext_heading: Node) -> u8 {
    let is_h1 = (0..setext_heading.child_count()).any(|i| {
        setext_heading
            .child(i as u32)
            .is_some_and(|child| child.kind() == "setext_h1_underline")
    });
    if is_h1 {
        1
    } else {
        2
    }
}

fn parse_fenced_code_block(node: Node, source: &str) -> MdBlock {
    let language = first_child_of_kind(node, "info_string")
        .and_then(|info| first_child_of_kind(info, "language"))
        .map(|lang| source[lang.byte_range()].to_string());
    let text = first_child_of_kind(node, "code_fence_content")
        .map(|content| source[content.byte_range()].to_string())
        .unwrap_or_default();
    MdBlock::CodeBlock { language, text }
}

/// Strips CommonMark's own indented-code-block indent (4 spaces, or one leading tab) from every
/// line - that indent is the block's own marker, not part of the code it names.
fn strip_indented_code_prefix(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            line.strip_prefix("    ")
                .or_else(|| line.strip_prefix('\t'))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_list(list_node: Node, source: &str) -> MdBlock {
    let mut items = Vec::new();
    let mut ordered = false;
    let mut start = 1u64;
    let mut cursor = list_node.walk();
    for (index, item) in list_node
        .children(&mut cursor)
        .filter(|child| child.kind() == "list_item")
        .enumerate()
    {
        if let Some(marker) = item.child(0) {
            if matches!(marker.kind(), "list_marker_dot" | "list_marker_parenthesis") {
                ordered = true;
                if index == 0 {
                    // The marker's own text includes its trailing space (`"3. "`, not `"3."`) -
                    // trim whitespace *before* stripping the `.`/`)` punctuation, not after,
                    // or the punctuation is never reached and the digits fail to parse.
                    start = source[marker.byte_range()]
                        .trim()
                        .trim_end_matches(['.', ')'])
                        .parse()
                        .unwrap_or(1);
                }
            }
        }
        items.push(parse_blocks(item, source));
    }
    MdBlock::List {
        ordered,
        start,
        items,
    }
}

fn parse_pipe_table(node: Node, source: &str) -> MdBlock {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    let mut cursor = node.walk();
    for row in node.children(&mut cursor) {
        match row.kind() {
            "pipe_table_header" => header = table_row_cells(row, source),
            "pipe_table_row" => rows.push(table_row_cells(row, source)),
            _ => {}
        }
    }
    MdBlock::Table { header, rows }
}

fn table_row_cells(row: Node, source: &str) -> Vec<String> {
    let mut cursor = row.walk();
    row.children(&mut cursor)
        .filter(|cell| cell.kind() == "pipe_table_cell")
        .map(|cell| source[cell.byte_range()].trim().to_string())
        .collect()
}

/// Reconstructs the real, clean source text an `"inline"` node covers - stripping any nested
/// `"block_continuation"` marker byte ranges (e.g. a block quote's own `"> "` prefix on a
/// continuation line) rather than taking the node's raw `byte_range()` verbatim, which would
/// otherwise splice that marker text into the middle of real prose (verified directly against a
/// real multi-line block quote parse: `"a quote\n> second line"` raw vs. this function's
/// `"a quote\nsecond line"` - not assumed).
fn inline_source(node: Node, source: &str) -> String {
    let continuations: Vec<Node> = (0..node.child_count())
        .filter_map(|i| node.child(i as u32))
        .filter(|child| child.kind() == "block_continuation")
        .collect();
    if continuations.is_empty() {
        return source[node.byte_range()].to_string();
    }
    let mut out = String::new();
    let mut cursor_pos = node.start_byte();
    for continuation in continuations {
        out.push_str(&source[cursor_pos..continuation.start_byte()]);
        cursor_pos = continuation.end_byte();
    }
    out.push_str(&source[cursor_pos..node.end_byte()]);
    out
}

/// Re-parses one block grammar `"inline"` node's real text with `tree-sitter-md`'s separate
/// inline grammar (the block grammar deliberately never parses prose itself - see this crate's
/// `code_view::highlight_markdown` for the identical, established two-grammar split) and walks
/// the result into real [`MdInline`]s.
fn parse_inline_node(inline_node: Node, source: &str) -> Vec<MdInline> {
    let text = inline_source(inline_node, source);
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .is_err()
    {
        return vec![MdInline::Text(text)];
    }
    let Some(tree) = parser.parse(&text, None) else {
        return vec![MdInline::Text(text)];
    };
    build_inline_run(tree.root_node(), &text)
}

/// Walks one inline-grammar node's children into real [`MdInline`]s. Generic by construction,
/// not by per-kind special-casing: `tree-sitter-md`'s inline grammar represents `**bold**` as
/// `(strong_emphasis (emphasis_delimiter) (emphasis_delimiter) (emphasis_delimiter)
/// (emphasis_delimiter))` - four punctuation-only delimiter children with *no* child node for
/// `"bold"` itself (verified via a real parse, not assumed) - so the real text is always the
/// *gap* between recognized children, never a child's own text. This loop captures exactly that
/// gap unconditionally, before dispatching on the child's own kind, which is what makes
/// recursing into `code_span`'s own two delimiter children (with nothing else recognized inside)
/// already produce its own inner text correctly, with no separate extraction path needed.
fn build_inline_run(node: Node, text: &str) -> Vec<MdInline> {
    let mut out = Vec::new();
    let mut cursor_pos = node.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() > cursor_pos {
            push_text(&mut out, &text[cursor_pos..child.start_byte()]);
        }
        match child.kind() {
            "strong_emphasis" => out.push(MdInline::Strong(build_inline_run(child, text))),
            "emphasis" => out.push(MdInline::Emphasis(build_inline_run(child, text))),
            "code_span" => push_code(&mut out, &flatten_text(&build_inline_run(child, text))),
            "inline_link" => out.push(build_link(child, text)),
            "image" => out.push(build_image(child, text)),
            "hard_line_break" => out.push(MdInline::LineBreak),
            _ => {}
        }
        cursor_pos = child.end_byte();
    }
    if cursor_pos < node.end_byte() {
        push_text(&mut out, &text[cursor_pos..node.end_byte()]);
    }
    out
}

/// A soft line break (a bare `\n` inside a paragraph, as opposed to a real `hard_line_break`
/// node) collapses to a single space - CommonMark's own rule, and the same thing a browser does
/// with unstyled whitespace - rather than rendering as a literal embedded newline GPUI has no
/// reason to treat specially.
fn push_text(out: &mut Vec<MdInline>, raw: &str) {
    let collapsed = raw.replace('\n', " ");
    if !collapsed.is_empty() {
        out.push(MdInline::Text(collapsed));
    }
}

fn push_code(out: &mut Vec<MdInline>, raw: &str) {
    if !raw.is_empty() {
        out.push(MdInline::Code(raw.to_string()));
    }
}

fn flatten_text(inlines: &[MdInline]) -> String {
    inlines
        .iter()
        .map(|inline| match inline {
            MdInline::Text(text) | MdInline::Code(text) => text.as_str(),
            _ => "",
        })
        .collect()
}

fn build_link(inline_link: Node, text: &str) -> MdInline {
    let link_text = first_child_of_kind(inline_link, "link_text")
        .map(|node| build_inline_run(node, text))
        .unwrap_or_default();
    let destination = first_child_of_kind(inline_link, "link_destination")
        .map(|node| text[node.byte_range()].to_string())
        .unwrap_or_default();
    MdInline::Link {
        text: link_text,
        destination,
    }
}

fn build_image(image: Node, text: &str) -> MdInline {
    let alt = first_child_of_kind(image, "image_description")
        .map(|node| flatten_text(&build_inline_run(node, text)))
        .unwrap_or_default();
    MdInline::Image { alt }
}

// ---------------------------------------------------------------------------------------------
// Rendering: `Vec<MdBlock>` -> nested styled GPUI elements.
// ---------------------------------------------------------------------------------------------

use gpui::prelude::FluentBuilder;
use gpui::{
    div, font, px, rems, AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, StyledText, TextRun,
};

use super::code_view;
use super::zoom::zoom_scoped;
use crate::root::AdeApp;
use crate::theme;

/// Surface C's `Source | Preview` toggle for a `.md` file - one shared field reset on tab
/// activation, mirroring [`code_view::CodeView`]'s own shape (see `root::AdeApp::markdown_view`'s
/// own docs for why this app has no per-tab state mechanism to do otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MarkdownView {
    #[default]
    Source,
    Preview,
}

const INDENT_PX: f32 = 20.0;

fn hsla(token: theme::ColorToken) -> Hsla {
    token.into()
}

fn prose_font() -> gpui::Font {
    font(theme::font::SANS)
}

fn mono_font() -> gpui::Font {
    font(theme::font::MONO)
}

impl AdeApp {
    /// GitHub issue #115: the real rendered Markdown preview for `source` - a real
    /// `tree-sitter-md` parse ([`parse_markdown`]) walked into nested styled elements, not a
    /// second copy of [`code_view::highlight_markdown`]'s flat source-text coloring. Zoom-scoped
    /// through the same [`zoom_scoped`]/[`Self::effective_code_rem_px`] the File view uses, so
    /// prose size follows the user's editor zoom setting exactly like code does.
    pub(in crate::code_surface) fn render_markdown_preview(
        &self,
        source: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let blocks = parse_markdown(source);
        let content = div()
            .id("markdown-preview-content")
            // Test-only lookup key for `VisualTestContext::debug_bounds` - matches
            // `crate::settings::widgets::AdeApp::render_choice_control`'s own identical
            // `.debug_selector` convention for exactly the same reason (a plain `.id()` alone is
            // not queryable through `debug_bounds`). No-op in release builds.
            .debug_selector(|| "markdown-preview-content".to_string())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.markdown_preview_scroll_handle)
            .px(px(28.0))
            .py(px(20.0))
            .gap(px(10.0))
            .children(blocks.iter().map(render_block));
        let scrolled = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .bg(theme::surface::CENTER)
            .child(content)
            .children(self.render_vertical_scrollbar(
                "markdown-preview-scrollbar",
                &self.markdown_preview_scroll_handle,
                &[],
                cx,
            ));
        zoom_scoped(self.effective_code_rem_px(), scrolled)
    }
}

fn render_block(block: &MdBlock) -> AnyElement {
    match block {
        MdBlock::Heading { level, inline } => render_heading(*level, inline),
        MdBlock::Paragraph(inline) => render_prose(inline, theme::text::BODY, prose_font()),
        MdBlock::CodeBlock { language, text } => render_code_block(language.as_deref(), text),
        MdBlock::List {
            ordered,
            start,
            items,
        } => render_list(*ordered, *start, items),
        MdBlock::BlockQuote(blocks) => render_block_quote(blocks),
        MdBlock::ThematicBreak => render_thematic_break(),
        MdBlock::Table { header, rows } => render_table(header, rows),
    }
}

fn heading_size_rems(level: u8) -> f32 {
    match level {
        1 => 1.7,
        2 => 1.4,
        3 => 1.2,
        4 => 1.05,
        5 => 0.95,
        _ => 0.9,
    }
}

fn render_heading(level: u8, inline: &[MdInline]) -> AnyElement {
    let mut el = div()
        .flex()
        .flex_col()
        .text_size(rems(heading_size_rems(level)))
        .line_height(rems(heading_size_rems(level) * 1.3))
        .child(render_prose_font(
            inline,
            theme::text::HEADING,
            prose_font().bold(),
        ));
    if level <= 2 {
        el = el
            .pb(px(6.0))
            .border_b_1()
            .border_color(theme::border::DIVIDER);
    }
    el.into_any_element()
}

/// A plain prose block (paragraph body, list item text) at the ambient body text size.
fn render_prose(inline: &[MdInline], color: theme::ColorToken, font: gpui::Font) -> AnyElement {
    div()
        .text_size(rems(0.95))
        .line_height(rems(1.55))
        .child(render_prose_font(inline, color, font))
        .into_any_element()
}

/// Builds one real [`StyledText`] run from `inline` - real mixed-style word-wrapping prose
/// (bold/italic/code/link spans flowing together on the same line), not a `flex()` row of
/// separate divs, which GPUI does not word-wrap the way inline HTML does. `base_color`/`base_font`
/// are this run's own default styling before any nested emphasis/strong/link/code override.
fn render_prose_font(
    inline: &[MdInline],
    base_color: theme::ColorToken,
    base_font: gpui::Font,
) -> AnyElement {
    let mut text = String::new();
    let mut runs = Vec::new();
    build_text_runs(inline, hsla(base_color), &base_font, &mut text, &mut runs);
    if text.is_empty() {
        return div().into_any_element();
    }
    StyledText::new(text).with_runs(runs).into_any_element()
}

fn push_run(
    text: &mut String,
    runs: &mut Vec<TextRun>,
    run_text: &str,
    font: &gpui::Font,
    color: Hsla,
    background: Option<Hsla>,
) {
    if run_text.is_empty() {
        return;
    }
    text.push_str(run_text);
    runs.push(TextRun {
        len: run_text.len(),
        font: font.clone(),
        color,
        background_color: background,
        underline: None,
        strikethrough: None,
    });
}

fn build_text_runs(
    inlines: &[MdInline],
    color: Hsla,
    font: &gpui::Font,
    text: &mut String,
    runs: &mut Vec<TextRun>,
) {
    for inline in inlines {
        match inline {
            MdInline::Text(run_text) => push_run(text, runs, run_text, font, color, None),
            MdInline::Strong(children) => build_text_runs(
                children,
                hsla(theme::syntax::STRONG),
                &font.clone().bold(),
                text,
                runs,
            ),
            MdInline::Emphasis(children) => build_text_runs(
                children,
                hsla(theme::syntax::EMPHASIS),
                &font.clone().italic(),
                text,
                runs,
            ),
            MdInline::Code(code_text) => push_run(
                text,
                runs,
                code_text,
                &mono_font(),
                hsla(theme::text::PATH),
                Some(hsla(theme::surface::ROW_HOVER_ALT)),
            ),
            MdInline::Link {
                text: link_text,
                destination: _,
            } => build_text_runs(link_text, hsla(theme::syntax::LINK), font, text, runs),
            MdInline::Image { alt } => push_run(
                text,
                runs,
                &format!("[image: {alt}]"),
                font,
                hsla(theme::text::GHOST),
                None,
            ),
            MdInline::LineBreak => push_run(text, runs, "\n", font, color, None),
        }
    }
}

/// A preview code card's real, coloured lines - split out of [`render_code_block`] purely so the
/// colouring itself is testable without building a GPUI element (`AnyElement` exposes nothing to
/// assert on).
///
/// The fence-tag alias table it resolves through used to live here as a private copy; GitHub issue
/// #154 lifted it into [`crate::language::extension_for_fence_language`] because the *source*
/// view's own tree-sitter injection callback needs the identical mapping, and two copies could
/// disagree about what ` ```py ` means. Sharing it is also what gave preview mode real HTML/CSS
/// fence colouring with no further change here - see that function's own docs.
fn highlighted_code_block_lines(
    language: Option<&str>,
    text: &str,
) -> Vec<code_view::RenderedLine> {
    let extension = language.and_then(crate::language::extension_for_fence_language);
    let lines: Vec<&str> = text.lines().collect();
    code_view::highlight_block(lines, extension)
}

fn render_code_block(language: Option<&str>, text: &str) -> AnyElement {
    let rendered = highlighted_code_block_lines(language, text);
    let mut card = div()
        .flex()
        .flex_col()
        .rounded(theme::radius::CARD_SM)
        .bg(theme::surface::FOOTER)
        .border_1()
        .border_color(theme::border::INNER)
        .px(px(12.0))
        .py(px(8.0))
        .font(mono_font())
        .text_size(rems(0.85));
    for line in &rendered {
        card = card.child(render_code_line(line));
    }
    card.into_any_element()
}

fn render_code_line(line: &code_view::RenderedLine) -> AnyElement {
    let mut text = String::new();
    let mut runs = Vec::new();
    for (run_text, kind) in &line.runs {
        push_run(
            &mut text,
            &mut runs,
            run_text,
            &mono_font(),
            code_view::color_for_kind(*kind).into(),
            None,
        );
    }
    if text.is_empty() {
        // A real blank line must still take up a real row of vertical space.
        text.push(' ');
        runs.push(TextRun {
            len: 1,
            font: mono_font(),
            color: hsla(theme::text::GHOST),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }
    StyledText::new(text).with_runs(runs).into_any_element()
}

fn render_list(ordered: bool, start: u64, items: &[Vec<MdBlock>]) -> AnyElement {
    let mut list = div().flex().flex_col().gap(px(4.0));
    for (index, item_blocks) in items.iter().enumerate() {
        let marker: SharedString = if ordered {
            format!("{}.", start + index as u64).into()
        } else {
            "\u{2022}".into()
        };
        let row = div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .w(px(18.0))
                    .text_size(rems(0.95))
                    .text_color(theme::text::SECONDARY)
                    .child(marker),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.0))
                    .children(item_blocks.iter().map(render_block)),
            );
        list = list.child(row);
    }
    div().pl(px(INDENT_PX * 0.0)).child(list).into_any_element()
}

fn render_block_quote(blocks: &[MdBlock]) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .pl(px(14.0))
        .py(px(2.0))
        .border_l_2()
        .border_color(theme::border::DIVIDER)
        .text_color(theme::text::SECONDARY)
        .children(blocks.iter().map(render_block))
        .into_any_element()
}

fn render_thematic_break() -> AnyElement {
    div()
        .flex_none()
        .h(px(1.0))
        .my(px(4.0))
        .bg(theme::border::DIVIDER)
        .into_any_element()
}

fn render_table(header: &[String], rows: &[Vec<String>]) -> AnyElement {
    let cell = |text: &str, is_header: bool| {
        div()
            .flex_1()
            .min_w_0()
            .px(px(8.0))
            .py(px(5.0))
            .when(is_header, |el| el.font_weight(FontWeight::BOLD))
            .text_size(rems(0.9))
            .text_color(if is_header {
                theme::text::HEADING
            } else {
                theme::text::BODY
            })
            .child(text.to_string())
    };
    let row = |cells: &[String], is_header: bool| {
        div()
            .flex()
            .flex_row()
            .when(!is_header, |el| {
                el.border_t_1().border_color(theme::border::DIVIDER)
            })
            .children(cells.iter().map(|text| cell(text, is_header)))
    };
    div()
        .flex()
        .flex_col()
        .rounded(theme::radius::CARD_SM)
        .border_1()
        .border_color(theme::border::INNER)
        .overflow_hidden()
        .child(row(header, true))
        .children(rows.iter().map(|cells| row(cells, false)))
        .into_any_element()
}

#[cfg(test)]
mod code_block_color_tests {
    use super::*;
    use crate::code_surface::code_view::HighlightKind;

    /// The real kind covering the first occurrence of `needle` across a rendered code card's
    /// lines - the same question `render_code_line` asks when it picks each run's colour.
    fn kind_of(lines: &[code_view::RenderedLine], needle: &str) -> Option<HighlightKind> {
        lines
            .iter()
            .flat_map(|line| &line.runs)
            .find(|(text, _)| text.contains(needle))
            .map(|(_, kind)| *kind)
    }

    /// GitHub issue #154 in *preview* mode, which is a genuinely separate rendering path from the
    /// source view's own highlighting: a ` ```html ` fence's card is really coloured by
    /// `tree-sitter-html`, not left flat.
    #[test]
    fn a_preview_html_fence_is_really_colored_by_the_html_grammar() {
        let lines = highlighted_code_block_lines(Some("html"), "<div class=\"card\">hi</div>\n");
        assert_eq!(kind_of(&lines, "div"), Some(HighlightKind::Tag));
        assert_eq!(kind_of(&lines, "class"), Some(HighlightKind::Attribute));
    }

    #[test]
    fn a_preview_css_fence_is_really_colored_by_the_css_grammar() {
        let lines = highlighted_code_block_lines(Some("css"), ".card { color: red; }\n");
        assert_eq!(kind_of(&lines, "card"), Some(HighlightKind::Property));
    }

    /// The pre-existing languages must be unaffected by the alias table moving out of this module.
    #[test]
    fn a_preview_rust_fence_is_still_colored_by_the_rust_grammar() {
        let lines = highlighted_code_block_lines(Some("rust"), "fn main() {}\n");
        assert_eq!(kind_of(&lines, "fn"), Some(HighlightKind::Keyword));
    }

    /// A real gain the shared table brought with it: this module's own private copy mapped a
    /// ` ```jsx ` fence to the `"js"` extension, which wires the *plain* TypeScript grammar - one
    /// that cannot parse JSX at all. It now maps to `"jsx"`, which wires the real TSX grammar.
    #[test]
    fn a_preview_jsx_fence_reaches_a_grammar_that_can_actually_parse_jsx() {
        let lines = highlighted_code_block_lines(Some("jsx"), "const a = <div id=\"x\" />;\n");
        assert_eq!(kind_of(&lines, "div"), Some(HighlightKind::Tag));
        assert_eq!(kind_of(&lines, "id"), Some(HighlightKind::Attribute));
    }

    /// An unrecognized fence tag, and a fence with no tag at all, render as one flat unclassified
    /// card rather than being force-fitted into some other language.
    #[test]
    fn an_unknown_or_absent_preview_fence_language_stays_plain_text() {
        for language in [Some("zig"), None] {
            let lines = highlighted_code_block_lines(language, "const x = 1;\n");
            let kinds: Vec<HighlightKind> = lines
                .iter()
                .flat_map(|line| &line.runs)
                .map(|(_, kind)| *kind)
                .collect();
            assert_eq!(kinds, vec![HighlightKind::Text], "{language:?}");
        }
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn a_plain_paragraph_round_trips_as_a_single_text_run() {
        let blocks = parse_markdown("hello world\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Paragraph(vec![MdInline::Text(
                "hello world".to_string()
            )])]
        );
    }

    #[test]
    fn atx_headings_report_their_real_level() {
        for (markup, level) in [
            ("# one", 1),
            ("## two", 2),
            ("### three", 3),
            ("#### four", 4),
            ("##### five", 5),
            ("###### six", 6),
        ] {
            // A trailing newline matters here: without one, this grammar doesn't recognize the
            // line as a real `atx_heading` at all (verified directly - not assumed).
            let blocks = parse_markdown(&format!("{markup}\n"));
            assert_eq!(
                blocks,
                vec![MdBlock::Heading {
                    level,
                    inline: vec![MdInline::Text(
                        markup.trim_start_matches('#').trim().to_string()
                    )]
                }],
                "markup {markup:?}"
            );
        }
    }

    #[test]
    fn setext_headings_report_h1_and_h2() {
        assert_eq!(
            parse_markdown("Title\n=====\n"),
            vec![MdBlock::Heading {
                level: 1,
                inline: vec![MdInline::Text("Title".to_string())]
            }]
        );
        assert_eq!(
            parse_markdown("Title\n-----\n"),
            vec![MdBlock::Heading {
                level: 2,
                inline: vec![MdInline::Text("Title".to_string())]
            }]
        );
    }

    #[test]
    fn strong_and_emphasis_and_code_span_are_real_nested_inlines_not_flattened_text() {
        let blocks = parse_markdown("a **bold** b *it* c `code` d\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Paragraph(vec![
                MdInline::Text("a ".to_string()),
                MdInline::Strong(vec![MdInline::Text("bold".to_string())]),
                MdInline::Text(" b ".to_string()),
                MdInline::Emphasis(vec![MdInline::Text("it".to_string())]),
                MdInline::Text(" c ".to_string()),
                MdInline::Code("code".to_string()),
                MdInline::Text(" d".to_string()),
            ])]
        );
    }

    #[test]
    fn an_inline_link_carries_its_real_text_and_destination() {
        let blocks = parse_markdown("see [the docs](https://example.com/x) now\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Paragraph(vec![
                MdInline::Text("see ".to_string()),
                MdInline::Link {
                    text: vec![MdInline::Text("the docs".to_string())],
                    destination: "https://example.com/x".to_string(),
                },
                MdInline::Text(" now".to_string()),
            ])]
        );
    }

    #[test]
    fn an_image_carries_its_real_alt_text_and_nothing_else() {
        let blocks = parse_markdown("![a cat](cat.png)\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Paragraph(vec![MdInline::Image {
                alt: "a cat".to_string()
            }])]
        );
    }

    #[test]
    fn a_fenced_code_block_carries_its_real_language_and_body_verbatim() {
        let blocks = parse_markdown("```rust\nfn main() {\n    1;\n}\n```\n");
        assert_eq!(
            blocks,
            vec![MdBlock::CodeBlock {
                language: Some("rust".to_string()),
                text: "fn main() {\n    1;\n}\n".to_string(),
            }]
        );
    }

    #[test]
    fn a_fenced_code_block_with_no_language_reports_none() {
        let blocks = parse_markdown("```\nplain\n```\n");
        assert_eq!(
            blocks,
            vec![MdBlock::CodeBlock {
                language: None,
                text: "plain\n".to_string(),
            }]
        );
    }

    #[test]
    fn an_indented_code_block_strips_the_real_four_space_marker_not_the_codes_own_indentation() {
        let blocks = parse_markdown("    fn main() {\n        1;\n    }\n");
        assert_eq!(
            blocks,
            vec![MdBlock::CodeBlock {
                language: None,
                // The code's own inner 4-space indent (on `1;`) must survive - only the block's
                // own leading marker indent is stripped.
                text: "fn main() {\n    1;\n}".to_string(),
            }]
        );
    }

    #[test]
    fn a_bullet_list_reports_unordered_with_each_items_real_paragraph() {
        let blocks = parse_markdown("- one\n- two\n");
        assert_eq!(
            blocks,
            vec![MdBlock::List {
                ordered: false,
                start: 1,
                items: vec![
                    vec![MdBlock::Paragraph(vec![MdInline::Text("one".to_string())])],
                    vec![MdBlock::Paragraph(vec![MdInline::Text("two".to_string())])],
                ],
            }]
        );
    }

    #[test]
    fn an_ordered_list_reports_its_real_start_number() {
        let blocks = parse_markdown("3. three\n4. four\n");
        assert_eq!(
            blocks,
            vec![MdBlock::List {
                ordered: true,
                start: 3,
                items: vec![
                    vec![MdBlock::Paragraph(vec![MdInline::Text(
                        "three".to_string()
                    )])],
                    vec![MdBlock::Paragraph(vec![MdInline::Text("four".to_string())])],
                ],
            }]
        );
    }

    #[test]
    fn a_nested_list_is_a_real_nested_block_not_flattened_into_the_parent_item() {
        let blocks = parse_markdown("- outer\n  - inner\n");
        let MdBlock::List { items, .. } = &blocks[0] else {
            panic!("expected a list, got {blocks:?}");
        };
        assert_eq!(items.len(), 1, "one real top-level item");
        let outer_item = &items[0];
        assert!(
            outer_item
                .iter()
                .any(|block| matches!(block, MdBlock::List { .. })),
            "the outer item's own blocks must include a real nested List - got {outer_item:?}"
        );
    }

    #[test]
    fn a_multiline_block_quote_strips_the_real_prefix_marker_from_every_continuation_line() {
        let blocks = parse_markdown("> first\n> second\n");
        assert_eq!(
            blocks,
            vec![MdBlock::BlockQuote(vec![MdBlock::Paragraph(vec![
                MdInline::Text("first second".to_string())
            ])])]
        );
    }

    #[test]
    fn thematic_break_is_its_own_real_block() {
        assert_eq!(parse_markdown("---\n"), vec![MdBlock::ThematicBreak]);
    }

    #[test]
    fn a_pipe_table_reports_its_real_header_and_row_cells() {
        let blocks = parse_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Table {
                header: vec!["a".to_string(), "b".to_string()],
                rows: vec![vec!["1".to_string(), "2".to_string()]],
            }]
        );
    }

    #[test]
    fn a_document_with_multiple_heading_levels_keeps_every_block_in_real_top_level_order() {
        let blocks = parse_markdown("# h1\n\npara one\n\n## h2\n\npara two\n");
        assert_eq!(
            blocks,
            vec![
                MdBlock::Heading {
                    level: 1,
                    inline: vec![MdInline::Text("h1".to_string())]
                },
                MdBlock::Paragraph(vec![MdInline::Text("para one".to_string())]),
                MdBlock::Heading {
                    level: 2,
                    inline: vec![MdInline::Text("h2".to_string())]
                },
                MdBlock::Paragraph(vec![MdInline::Text("para two".to_string())]),
            ],
            "section nesting must never reorder or drop a real top-level block"
        );
    }

    #[test]
    fn an_empty_document_parses_to_no_blocks() {
        assert_eq!(parse_markdown(""), Vec::new());
    }
}
