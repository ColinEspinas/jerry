//! The sidebar strip, drawn (GitHub issue #291): the rail column's own 36px view switcher, the
//! real switch behind it, and the Problems view it switches to.

use super::*;
use std::path::Path;

use crate::icons::{IconRow, IconSize};
use crate::lsp::client::LspClientState;
use crate::lsp::diagnostics as diagnostics_view;
use crate::rail::strip::{self, Problem, ProblemTally, SidebarView, StripCell, StripMarker};
use crate::root::scrollbar;
use crate::root::widgets::text_tooltip;

/// Memo behind [`AdeApp::worktree_problems`] - see `AdeApp::problems_cache`'s own field docs.
/// Holds the derived list plus exactly the inputs whose movement invalidates it.
#[derive(Default)]
pub(crate) struct ProblemsCache {
    root: Option<std::path::PathBuf>,
    generations: Vec<(std::path::PathBuf, &'static str, u64)>,
    problems: std::rc::Rc<Vec<Problem>>,
}

impl AdeApp {
    /// Switches the sidebar to `view`.
    pub(crate) fn set_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        if self.sidebar_view == view {
            return;
        }
        self.sidebar_view = view;
        let _ = self.close_menu_surfaces_except(None);
        cx.notify();
    }

    /// The view the sidebar body is really showing, after [`strip::effective_view`]'s empty gate.
    /// Read by both the strip (to decide which slab fills) and the body (to decide what to paint),
    /// so the lit cell and the panel under it can never disagree.
    pub(in crate::rail) fn effective_sidebar_view(&self, has_worktrees: bool) -> SidebarView {
        strip::effective_view(has_worktrees, self.sidebar_view)
    }

    /// Every diagnostic the app really knows about in the currently selected worktree, worst
    /// first, then by file and position — memoized on `AdeApp::problems_cache`, recomputed only
    /// when the worktree or some Ready client's diagnostics generation moved. This runs on
    /// every render, and the uncached version cloned every diagnostic and `stat`ed every
    /// diagnosed file per frame (GitHub issue #471).
    pub(in crate::rail) fn worktree_problems(&self) -> std::rc::Rc<Vec<Problem>> {
        let Some(root) = self.current_worktree_path() else {
            return std::rc::Rc::default();
        };
        let mut generations: Vec<(PathBuf, &'static str, u64)> = self
            .lsp_clients
            .iter()
            .filter_map(|((client_root, server), state)| match state {
                LspClientState::Ready(client) if client_root == &root => Some((
                    client_root.clone(),
                    *server,
                    client.diagnostics_generation(),
                )),
                _ => None,
            })
            .collect();
        // `lsp_clients` is a `HashMap`; a stable order is what makes generation equality mean
        // "nothing changed" rather than "same clients, different iteration order".
        generations.sort();
        {
            let cache = self.problems_cache.borrow();
            if cache.root.as_ref() == Some(&root) && cache.generations == generations {
                return std::rc::Rc::clone(&cache.problems);
            }
        }
        let problems = std::rc::Rc::new(self.compute_worktree_problems(&root));
        *self.problems_cache.borrow_mut() = ProblemsCache {
            root: Some(root),
            generations,
            problems: std::rc::Rc::clone(&problems),
        };
        problems
    }

    /// Drops the memo so the next [`Self::worktree_problems`] call recomputes. Called by the
    /// file-tree watcher's event arm: diagnostics generations can't see *worktree* changes, and
    /// the existence filter below must re-run once a published-about file may have been
    /// deleted out from under its server (the watcher is how the real app notices that).
    pub(crate) fn invalidate_problems_cache(&self) {
        self.problems_cache.borrow_mut().root = None;
    }

    /// The real recomputation behind [`Self::worktree_problems`] — the only place the
    /// per-diagnostic clones and the per-file existence `stat`s happen.
    fn compute_worktree_problems(&self, root: &Path) -> Vec<Problem> {
        let root = root.to_path_buf();
        // A diagnostic's uri is whatever real path the *server* opened, and `lsp_core`'s own
        // `path_to_uri` canonicalises on the way in - so a checkout reached through a symlink
        // (macOS' `/var` -> `/private/var`, or a worktree directory someone symlinked) yields
        // paths that `strip_prefix(&root)` can never match. Resolved once per pass, not per row,
        // and falling back to `root` itself if the checkout has since gone away. `dunce`, because
        // `lsp_core` canonicalises with `dunce` too - std's Windows verbatim `\\?\` spelling
        // would never prefix-match the server's paths (GitHub issue #467).
        let canonical_root = dunce::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let mut problems: Vec<Problem> = Vec::new();
        for ((client_root, _server), state) in &self.lsp_clients {
            let LspClientState::Ready(client) = state else {
                continue;
            };
            if client_root != &root {
                continue;
            }
            for (path, diagnostics) in client.published_diagnostics() {
                let Ok(relative) = path
                    .strip_prefix(&root)
                    .or_else(|_| path.strip_prefix(&canonical_root))
                else {
                    continue;
                };
                if !path.is_file() {
                    continue;
                }
                let file = relative.to_string_lossy().into_owned();
                for diagnostic in diagnostics {
                    problems.push(Problem {
                        severity: diagnostics_view::Severity::from_lsp(diagnostic.severity),
                        message: diagnostic.message.trim().to_string(),
                        file: file.clone(),
                        path: path.clone(),
                        // LSP positions are 0-based; every compiler, editor and other row in this
                        // app prints them 1-based.
                        line: diagnostic.range.start.line + 1,
                        column: diagnostic.range.start.character + 1,
                        source: diagnostic.source.filter(|source| !source.is_empty()),
                    });
                }
            }
        }
        problems.sort_by(Problem::worst_first);
        problems
    }

    /// The whole sidebar strip: the view cells, the flex spacer, the `+` new-agent cell and the
    /// `⋯` overflow - §4v's final cell list, with §4u's "Settings lives in the overflow" already
    /// applied, so there is no Settings cell.
    pub(in crate::rail) fn render_sidebar_strip(
        &self,
        cells: &[StripCell],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut band = div()
            .id("sidebar-strip")
            .debug_selector(|| "sidebar-strip".to_string())
            .flex()
            .flex_none()
            .items_stretch()
            .h(theme::band::CHROME_HEADER)
            .bg(theme::surface::SIDEBAR_STRIP);

        for cell in cells {
            band = band.child(self.render_sidebar_strip_cell(*cell, cx));
        }

        band.child(
            // The spacer is a child of the strip, so it carries the column rule too.
            div()
                .flex_1()
                .min_w(px(6.0))
                .border_b_1()
                .border_color(theme::border::RAIL_INNER),
        )
        .child(self.render_new_agent_cell(cx))
        .child(self.render_rail_overflow_button(cx))
    }

    /// One view cell: a full-height 38px slab, its glyph, and its state marker.
    fn render_sidebar_strip_cell(
        &self,
        cell: StripCell,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cell.view;
        let element_id = match view {
            SidebarView::Worktrees => "sidebar-strip-worktrees",
            SidebarView::Problems => "sidebar-strip-problems",
            // Unreachable: cells come from `strip::strip_view_cells`, which maps over
            // `SidebarView::ALL`, and History is deliberately not in it (§4t). Named anyway
            // rather than left to a catch-all, so this match keeps asking the question.
            SidebarView::History => "sidebar-strip-history",
        };
        let glyph = cell.glyph_color();

        let hover = strip_glyph_hover(cell.selected);

        strip_cell(
            div()
                .id(element_id)
                .debug_selector(move || element_id.to_string()),
            element_id,
            cell.rule_color(),
            glyph,
            // The hover lives on the glyph element rather than on the cell because
            // `IconRow::draw` pins the SVG's own `text_color` (an untinted `gpui::svg` paints
            // nothing at all), so an inherited colour from the cell would never reach it. A
            // `group_hover` keyed on the cell keeps the *trigger* the whole 38px slab - hovering
            // the cell's corner lights the glyph, exactly as `style-hover` on the cell does in the
            // mock - while the style still lands on the element that really owns the colour.
            IconRow::new(&self.settings.icon_pack, IconSize::Strip)
                .draw(view.icon(), glyph)
                .group_hover(element_id, move |el| el.text_color(hover)),
        )
        // The rule the tabs use between them - and the reason the strip reads as segments rather
        // than as icons on a dark band (§4v).
        .border_r_1()
        .border_color(theme::border::INNER)
        .when(cell.selected, |el| el.bg(theme::surface::RAIL))
        .children(cell.marker.map(|marker| render_strip_marker(view, marker)))
        .tooltip(text_tooltip(view.tooltip(cell.marker)))
        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
            this.set_sidebar_view(view, cx);
        }))
    }

    /// The strip's `+` cell - the rail's own new-agent action, in the strip's own shape.
    fn render_new_agent_cell(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let combo = keymap::resolve_combo("mod+N", self.window_controls_style().is_macos());
        strip_cell(
            div()
                .id("rail-new-agent")
                .debug_selector(|| "rail-new-agent".to_string()),
            "rail-new-agent",
            theme::border::RAIL_INNER,
            theme::text::FAINTER,
            div()
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(13.0))
                .group_hover("rail-new-agent", |el| {
                    el.text_color(strip_glyph_hover(false))
                })
                .child("+"),
        )
        .border_l_1()
        .border_color(theme::border::INNER)
        .tooltip(text_tooltip(format!(
            "New agent \u{2014} a shell in this worktree ({})",
            combo.join(" ")
        )))
        .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
            this.new_agent(ProcessKind::Shell, window, cx);
        }))
    }

    /// The sidebar's body for `view` - the rail's own tree, or the Problems list.
    pub(in crate::rail) fn render_sidebar_body(
        &mut self,
        view: SidebarView,
        groups: &std::rc::Rc<Vec<RepoGroup>>,
        problems: &[Problem],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match view {
            SidebarView::Worktrees => self.render_rail_list(groups, cx),
            SidebarView::Problems => {
                let body = self.render_problems_view(problems, cx);
                self.scrolled_sidebar_body(body, cx)
            }
            // GitHub issue #227. `&mut self` exists for this arm alone: History's body is the one
            // that needs a real background load before it can answer
            // (`crate::run_history::flow::AdeApp::load_run_drift` - one `git rev-list` per
            // checkout with history). Idempotent and single-flight, and asked for here rather
            // than at startup so a window whose user never opens History runs none - the same
            // shape `crate::lsp::client::AdeApp::ensure_lsp_client` already has on the code
            // surface's own render path.
            //
            // It shares the Problems view's plain scroller rather than the Worktrees view's
            // virtualized `gpui::list` for the same reason Problems does: the rows are
            // genuinely few (`crate::hooks::store::MAX_RECORDED_AGENTS` caps the whole
            // window's history), and the tree is a two-level structure of headers and rows
            // rather than the flat, uniform row list a `list` measures.
            SidebarView::History => {
                let body = self.render_history_view(cx);
                self.scrolled_sidebar_body(body, cx)
            }
        }
    }

    /// The plain scroller the two non-virtualized sidebar views share - Problems and History.
    fn scrolled_sidebar_body(
        &self,
        body: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("agent-rail-list")
                    // Lets a real test measure the scroller's own painted box - the rail menus'
                    // "rendered outside the scrolling list" guarantee (GitHub issue #290) is only
                    // checkable against it.
                    .debug_selector(|| "agent-rail-list".to_string())
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.rail_scroll_handle)
                    .child(body),
            )
            .children(scrollbar::render_vertical_scrollbar(
                "rail-scrollbar",
                &self.rail_scroll_handle,
                &[],
                cx,
            ))
            .into_any_element()
    }

    /// The Problems view: the real count line over the real rows, or the real empty note.
    fn render_problems_view(&self, all: &[Problem], cx: &mut Context<Self>) -> gpui::AnyElement {
        // Filtered here, not in `Self::worktree_problems`: the strip's own marker reads that
        // same list, and a marker that shrank as the filter box was typed into would break the
        // same promise the repo headers' counts keep (`crate::rail::state::RepoWorktrees::
        // all_rows`) - the strip reports what is really in the worktree, the list reports what
        // you asked to see.
        let query = self.filter_query.as_str().to_string();
        let problems: Vec<Problem> = all
            .iter()
            .filter(|problem| problem.matches_filter(&query))
            .cloned()
            .collect();
        let mut list = div()
            .id("sidebar-problems")
            .debug_selector(|| "sidebar-problems".to_string())
            .flex()
            .flex_col();

        if problems.is_empty() {
            // "Clean checkout" and "your filter hid them all" are two different facts, and the
            // rail's own list already distinguishes them for exactly this reason
            // (`Self::render_rail_list`). Saying `No diagnostics in <branch>.` over five hidden
            // diagnostics would be the same class of claim §4g removed elsewhere.
            let note = if all.is_empty() {
                let branch = self
                    .selected
                    .and_then(|index| self.worktrees.get(index))
                    .and_then(|item| item.branch.as_deref());
                strip::problems_empty_note(branch)
            } else {
                strip::problems_filtered_away_note(all.len())
            };
            return list
                .px(px(14.0))
                .py(px(18.0))
                .child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(self.ui_text_size(11.0))
                        .text_color(theme::text::GHOST)
                        .debug_selector(|| "sidebar-problems-note".to_string())
                        .child(note),
                )
                .into_any_element();
        }

        if let Some(line) = ProblemTally::over(&problems).count_line() {
            list = list.child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .pt(px(8.0))
                    .pb(px(6.0))
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::FAINTER)
                    .child(line),
            );
        }
        for (index, problem) in problems.iter().enumerate() {
            list = list.child(self.render_problem_row(index, problem, cx));
        }
        list.into_any_element()
    }

    /// One Problems row: a 5px severity square, the message, and the file/position/source line
    /// under it, and the click that opens it.
    fn render_problem_row(
        &self,
        index: usize,
        problem: &Problem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut meta = div()
            .flex()
            .items_baseline()
            .gap(px(6.0))
            .mt(px(3.0))
            .font(font(theme::font::MONO))
            .child(self.render_problem_path(&problem.file))
            .child(
                div()
                    .flex_none()
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOST)
                    .child(problem.position()),
            )
            .child(div().flex_1());
        if let Some(source) = problem.source.clone() {
            meta = meta.child(
                div()
                    .flex_none()
                    .text_size(self.ui_text_size(9.0))
                    .text_color(theme::text::GHOSTER)
                    .child(source),
            );
        }

        let target = problem.path.clone();
        let line = problem.line as usize;
        let element_id = gpui::SharedString::from(format!("sidebar-problems-row-{index}"));
        let selector = element_id.clone();
        div()
            .id(element_id)
            .debug_selector(move || selector.to_string())
            .flex()
            .gap(px(7.0))
            .border_l_2()
            .border_color(gpui::transparent_black())
            .pl(px(12.0))
            .pr(px(10.0))
            .pt(px(6.0))
            .pb(px(7.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .tooltip(text_tooltip(format!(
                "{}:{} \u{2014} open in the editor",
                problem.file, problem.line
            )))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.open_file_at_line(target.clone(), line, window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .mt(px(5.0))
                    .rounded(theme::radius::MARK_SM)
                    .bg(severity_color(problem.severity)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(self.ui_text_size(11.0))
                            .text_color(theme::text::STRONG)
                            .child(problem.message.clone()),
                    )
                    .child(meta),
            )
    }

    /// The row's `file` cell: the directory prefix dimmed, the file name at the mock's own
    /// `#7d848b` ([`theme::text::DIMMER`]).
    fn render_problem_path(&self, file: &str) -> impl IntoElement {
        // Both separators, because this string comes from a real `Path` and Windows' is `\`.
        let (directory, name) = match file.rfind(['/', '\\']) {
            Some(cut) => file.split_at(cut + 1),
            None => ("", file),
        };
        div()
            .flex()
            .min_w_0()
            .flex_shrink_1()
            .items_baseline()
            .text_size(self.ui_text_size(9.5))
            .when(!directory.is_empty(), |el| {
                el.child(
                    // The one shrinkable span in the row, and deliberately this one: a 260px rail
                    // cannot hold `src/db/orm/select.rs` beside a position and a source, and
                    // eliding the *directory*'s tail leaves the file name - the half a click is
                    // about - fully readable, where eliding the cell as a whole would eat it.
                    div()
                        .min_w_0()
                        .flex_shrink_1()
                        .truncate()
                        .text_color(theme::text::GHOSTER)
                        .child(directory.to_string()),
                )
            })
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text::DIMMER)
                    .child(name.to_string()),
            )
    }
}

/// Every strip cell's shared shape: 38 wide, full height, centred, no radius, no gap, `content`
/// in the middle, and the column rule painted as this cell's own last 1px row in `rule`.
pub(in crate::rail) fn strip_cell(
    cell: gpui::Stateful<gpui::Div>,
    group: &'static str,
    rule: theme::ColorToken,
    glyph: theme::ColorToken,
    content: impl IntoElement,
) -> gpui::Stateful<gpui::Div> {
    cell.group(group)
        .relative()
        .flex_none()
        .flex()
        .flex_col()
        .w(theme::zone::SIDEBAR_STRIP_CELL)
        .cursor_pointer()
        .text_color(glyph)
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(content),
        )
        .child(div().flex_none().w_full().h(px(1.0)).bg(rule))
}

/// What a strip cell's glyph lifts to when the pointer is on the cell.
fn strip_glyph_hover(selected: bool) -> theme::ColorToken {
    if selected {
        theme::text::SELECTED
    } else {
        theme::text::GLYPH_HOVER
    }
}

/// A cell's state marker: the tabs' own 5px square dot, in the marker's hue, inset at
/// `right:7 top:8`. On a square cell a corner-anchored badge reads as clipped, so it sits in.
fn render_strip_marker(view: SidebarView, marker: StripMarker) -> impl IntoElement {
    let selector = match view {
        SidebarView::Worktrees => "sidebar-strip-worktrees-marker",
        SidebarView::Problems => "sidebar-strip-problems-marker",
        // Unreachable - History has no cell, so it has no marker either. See `SidebarView`.
        SidebarView::History => "sidebar-strip-history-marker",
    };
    div()
        // Lets a real test prove the marker is really absent on a window with nothing waiting -
        // §1's "an empty day cannot claim agents needing a human" is only checkable as an absence.
        .debug_selector(move || selector.to_string())
        .absolute()
        .right(px(7.0))
        .top(px(8.0))
        .w(px(5.0))
        .h(px(5.0))
        .rounded(theme::radius::MARK_SM)
        .bg(marker.tone.color())
}

/// The row dot's colour for `severity`.
fn severity_color(severity: diagnostics_view::Severity) -> theme::ColorToken {
    match severity {
        diagnostics_view::Severity::Error => theme::status::FAIL,
        diagnostics_view::Severity::Warning => theme::status::ASK,
        // The two least severe levels are the ones the File view already draws with no row tint at
        // all (`crate::code_surface::lsp_ui::diagnostic_row_bg`); a neutral mark keeps them
        // visibly less alarming than a warning here too.
        diagnostics_view::Severity::Information | diagnostics_view::Severity::Hint => {
            theme::text::DIM
        }
    }
}

/// Real-window, real-git coverage for the sidebar strip (GitHub issue #291): the band as it is
/// really painted, the real switch a click on a cell performs, and the window-chrome invariant the
/// design derived while building it.
#[cfg(test)]
mod sidebar_strip_tests {
    use super::*;
    use gpui::TestAppContext;

    /// The four bounds every geometry assertion below reads: the two view cells, the `+` and the
    /// `⋯`, in the order the strip paints them.
    fn strip_cell_bounds(
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<(&'static str, gpui::Bounds<gpui::Pixels>)> {
        [
            "sidebar-strip-worktrees",
            "sidebar-strip-problems",
            "rail-new-agent",
            "rail-overflow",
        ]
        .into_iter()
        .map(|selector| {
            (
                selector,
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} must paint")),
            )
        })
        .collect()
    }

    fn right(bounds: gpui::Bounds<gpui::Pixels>) -> f32 {
        f32::from(bounds.origin.x + bounds.size.width)
    }

    fn bottom(bounds: gpui::Bounds<gpui::Pixels>) -> f32 {
        f32::from(bounds.origin.y + bounds.size.height)
    }

    #[gpui::test]
    fn every_strip_cell_is_a_full_height_38px_slab_with_no_gap(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (_app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let band = cx
            .debug_bounds("sidebar-strip")
            .expect("the strip must paint");
        assert_eq!(
            f32::from(band.size.height),
            f32::from(theme::band::CHROME_HEADER),
            "the strip is one of the window's three column headers"
        );

        let cells = strip_cell_bounds(cx);
        for (selector, bounds) in &cells {
            assert_eq!(
                f32::from(bounds.size.width),
                f32::from(theme::zone::SIDEBAR_STRIP_CELL),
                "{selector} must be a 38px cell"
            );
            assert_eq!(
                f32::from(bounds.size.height),
                f32::from(band.size.height),
                "{selector} must be a full-height slab, not a pill inset in the band"
            );
            assert_eq!(
                f32::from(bounds.origin.y),
                f32::from(band.origin.y),
                "{selector} must start at the band's own top edge"
            );
        }

        assert_eq!(
            right(cells[0].1),
            f32::from(cells[1].1.origin.x),
            "no gap between the two view cells"
        );
        assert_eq!(
            right(cells[2].1),
            f32::from(cells[3].1.origin.x),
            "no gap between the `+` and the `\u{22ef}`"
        );
        assert!(
            f32::from(cells[2].1.origin.x) > right(cells[1].1),
            "the flex spacer really separates the view cells from the trailing pair"
        );
        assert_eq!(
            right(cells[3].1),
            right(band),
            "the `\u{22ef}` is the strip's last cell and ends on the column's own edge"
        );
    }

    #[gpui::test]
    fn all_three_column_headers_end_on_one_line(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (_app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let rail = cx
            .debug_bounds("sidebar-strip")
            .expect("the rail's column header");
        let centre = cx
            .debug_bounds("tab-strip")
            .expect("the centre column's header");
        let panel = cx
            .debug_bounds("right-panel-header")
            .expect("the right panel's header");

        assert_eq!(
            (bottom(rail), bottom(centre), bottom(panel)),
            (bottom(rail), bottom(rail), bottom(rail)),
            "\u{a7}4v: three headers on one y, or the rule steps at every column boundary"
        );
        for (name, bounds) in [("rail", rail), ("centre", centre), ("panel", panel)] {
            assert_eq!(
                f32::from(bounds.size.height),
                f32::from(theme::band::CHROME_HEADER),
                "{name}'s header must be the one shared height"
            );
        }
    }

    #[gpui::test]
    fn the_centre_columns_bottom_edge_is_carried_all_the_way_across(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (_app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let centre = cx.debug_bounds("tab-strip").expect("the tab strip");
        let panel = cx
            .debug_bounds("right-panel-header")
            .expect("the right panel's header");
        assert!(
            right(centre) <= f32::from(panel.origin.x) + 1.0
                && right(centre) >= f32::from(panel.origin.x) - 2.0,
            "premise: the centre column really does run right up to the panel column, so a rule \
             that stopped short of its own right edge would be a visible gap - centre ends at {}, \
             panel starts at {}",
            right(centre),
            f32::from(panel.origin.x)
        );
        assert!(
            crate::work_surface::state::tab_colors(false).underline
                == gpui::Rgba::from(theme::border::RAIL_INNER),
            "\u{a7}4v: an inactive tab draws the window's column rule, in the colour all three \
             headers share - not `border::ZONE`, which read as one rule changing shade mid-span"
        );
        assert!(
            crate::work_surface::state::tab_colors(true).underline
                == crate::work_surface::state::tab_colors(true).bg,
            "and the active tab still cuts it with its own background"
        );
    }

    #[gpui::test]
    fn clicking_a_cell_really_switches_the_panel_under_it(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("rail-repo-groups").is_some(),
            "premise: the window opens on the Worktrees view"
        );
        assert!(cx.debug_bounds("sidebar-problems").is_none());

        let problems = cx
            .debug_bounds("sidebar-strip-problems")
            .expect("the Problems cell");
        cx.simulate_click(problems.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.sidebar_view, SidebarView::Problems);
        });
        assert!(
            cx.debug_bounds("sidebar-problems").is_some(),
            "the Problems view must really paint - a cell that lit up over an unchanged panel \
             would be exactly the dead button \u{a7}7 rule 1 forbids"
        );
        assert!(
            cx.debug_bounds("rail-repo-groups").is_none(),
            "and the worktree tree must really be gone, not merely covered"
        );

        let worktrees = cx
            .debug_bounds("sidebar-strip-worktrees")
            .expect("the Worktrees cell");
        cx.simulate_click(worktrees.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.sidebar_view, SidebarView::Worktrees);
        });
        assert!(cx.debug_bounds("rail-repo-groups").is_some());
        assert!(cx.debug_bounds("sidebar-problems").is_none());
    }

    #[gpui::test]
    fn both_view_glyphs_are_phosphor_assets_in_one_shared_optical_box(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (_app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let box_size = f32::from(IconSize::Strip.box_size());
        for icon in ["icon-tree-structure", "icon-warning"] {
            let bounds = cx
                .debug_bounds(icon)
                .unwrap_or_else(|| panic!("{icon} must paint - it is #282's own mapped glyph"));
            assert_eq!(
                (f32::from(bounds.size.width), f32::from(bounds.size.height)),
                (box_size, box_size),
                "{icon} must be drawn inside the strip's one shared optical box"
            );
        }

        // And really centred in their cells - the defect §4v records for the version before it
        // was "the padding pushed every glyph 2px off-centre".
        for (cell, icon) in [
            ("sidebar-strip-worktrees", "icon-tree-structure"),
            ("sidebar-strip-problems", "icon-warning"),
        ] {
            let cell = cx.debug_bounds(cell).expect("cell");
            let glyph = cx.debug_bounds(icon).expect("glyph");
            assert!(
                (f32::from(cell.center().x) - f32::from(glyph.center().x)).abs() <= 0.5,
                "{icon} is not horizontally centred in its cell"
            );
        }
    }

    #[gpui::test]
    fn a_window_with_nothing_waiting_paints_no_state_marker(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("sidebar-strip-worktrees").is_some(),
            "premise: the cells really are painted, so a missing marker is a real absence rather \
             than a missing strip"
        );
        assert!(
            cx.debug_bounds("sidebar-strip-worktrees-marker").is_none(),
            "no agent is waiting on a human, so the Worktrees cell must carry no marker"
        );
        assert!(
            cx.debug_bounds("sidebar-strip-problems-marker").is_none(),
            "no language server has reported anything, so the Problems cell must carry no marker"
        );
        app.read_with(cx, |app, _| {
            assert!(app.worktree_problems().is_empty());
            assert_eq!(
                ProblemTally::over(&app.worktree_problems()),
                ProblemTally::default()
            );
        });
    }

    #[gpui::test]
    fn an_empty_day_drops_the_view_cells_and_keeps_the_trailing_pair(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-strip-worktrees").is_some(),
            "premise: a repo with a real worktree does offer the switcher"
        );

        app.update(cx, |app, cx| {
            app.worktrees.clear();
            for repo in &mut app.repos {
                repo.worktrees.clear();
            }
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("sidebar-strip-worktrees").is_none()
                && cx.debug_bounds("sidebar-strip-problems").is_none(),
            "\u{a7}1: with no worktrees there are no views to offer, and a switcher with dead \
             views is worse than no switcher"
        );
        assert!(
            cx.debug_bounds("rail-new-agent").is_some()
                && cx.debug_bounds("rail-overflow").is_some(),
            "the `+` and the overflow are not view cells and must survive the gate"
        );
        assert!(
            cx.debug_bounds("sidebar-strip").is_some(),
            "the band itself stays - it is still one of the window's three column headers"
        );
    }

    #[gpui::test]
    fn the_filter_rows_placeholder_follows_the_selected_view(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("rail-filter-text").is_some(),
            "premise: the filter row survives the strip - it is chrome both views sit under"
        );

        let problems = cx
            .debug_bounds("sidebar-strip-problems")
            .expect("the Problems cell");
        cx.simulate_click(problems.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("rail-filter-text").is_some(),
            "\u{a7}1: the filter row stays across a view switch"
        );
        assert!(
            cx.debug_bounds("sidebar-problems-note").is_some(),
            "a clean checkout gets its own note"
        );
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.effective_sidebar_view(true),
                SidebarView::Problems,
                "premise: the switch really happened, so the placeholder read below is the \
                 Problems one"
            );
        });
    }

    #[gpui::test]
    fn the_plus_cell_still_spawns_a_real_agent(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let before = app.read_with(cx, |app, _| app.agents.iter().count());
        let plus = cx.debug_bounds("rail-new-agent").expect("the `+` cell");
        cx.simulate_click(plus.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                before + 1,
                "the `+` cell must really spawn, exactly as the rail header's `+` did"
            );
        });
    }
}

/// Real coverage for the Problems view itself (GitHub issue #292), driven against a **genuinely
/// spawned** language server pushing real `textDocument/publishDiagnostics` notifications over the
/// real wire protocol (`crate::lsp::client::lsp_connection_facade_tests`' own fake server - see
/// that module's docs for why a real, tiny server rather than the real `rust-analyzer`).
#[cfg(test)]
mod problems_view_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::{
        publish_full_and_wait, spawn_fake_server,
    };
    use crate::rail::worktrees::WorktreeItem;
    use gpui::TestAppContext;
    use std::path::Path;
    use std::sync::Arc;

    /// LSP severity numbers, as a real server sends them.
    const ERROR: u8 = 1;
    const WARNING: u8 = 2;
    const HINT: u8 = 4;

    /// Writes a real file under `root` and hands back its absolute path and its `file://` uri -
    /// the two things a real publish and a real click each need.
    fn real_file(root: &Path, relative: &str, contents: &str) -> (PathBuf, String) {
        let absolute = root.join(relative);
        std::fs::create_dir_all(absolute.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&absolute, contents).expect("write");
        let uri = lsp_core::LspClient::uri_for_path(&absolute)
            .expect("a real file:// uri")
            .to_string();
        (absolute, uri)
    }

    fn worktree_item(path: PathBuf, branch: &str) -> WorktreeItem {
        WorktreeItem {
            path,
            label: branch.to_string(),
            branch: Some(branch.to_string()),
            is_main: false,
            is_bare: false,
            is_detached: false,
            short_sha: None,
            is_locked: false,
            lock_reason: None,
            is_broken: false,
            broken_reason: None,
            error: None,
        }
    }

    /// Clicks the strip's Problems cell for real, the way a user reaches this view.
    fn show_problems(cx: &mut gpui::VisualTestContext) {
        let cell = cx
            .debug_bounds("sidebar-strip-problems")
            .expect("the Problems cell");
        cx.simulate_click(cell.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// The list the view is really showing, as `(file, line, severity)` triples.
    fn rows(app: &AdeApp) -> Vec<(String, u32, diagnostics_view::Severity)> {
        app.worktree_problems()
            .iter()
            .map(|problem| (problem.file.clone(), problem.line, problem.severity))
            .collect()
    }

    #[gpui::test]
    fn a_diagnostic_from_another_checkout_never_renders_under_the_active_one(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let other = crate::test_support::temp_root();
        let here = repo.path().to_path_buf();
        let there = other.path().to_path_buf();

        let (_, here_uri) = real_file(&here, "src/here.rs", "fn here() {}\n");
        let (_, there_uri) = real_file(&there, "src/there.rs", "fn there() {}\n");

        let (app, cx) = crate::test_support::open_test_app(cx, here.clone());
        cx.run_until_parked();

        let here_server = spawn_fake_server(&here, "rust-analyzer", "normal");
        let there_server = spawn_fake_server(&there, "rust-analyzer", "normal");
        publish_full_and_wait(
            &here_server,
            &here_uri,
            "mismatched types",
            17,
            ERROR,
            "rustc",
        );
        publish_full_and_wait(
            &there_server,
            &there_uri,
            "unused variable: `barrier`",
            43,
            WARNING,
            "rustc",
        );

        app.update_in(cx, |app, window, cx| {
            app.worktrees = vec![
                worktree_item(here.clone(), "main"),
                worktree_item(there.clone(), "fix/auth"),
            ];
            app.select_worktree(0, window, cx);
            app.lsp_clients.insert(
                (here.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&here_server)),
            );
            app.lsp_clients.insert(
                (there.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&there_server)),
            );
        });
        cx.run_until_parked();
        show_problems(cx);

        app.read_with(cx, |app, _| {
            assert_eq!(
                rows(app),
                vec![(
                    "src/here.rs".to_string(),
                    18,
                    diagnostics_view::Severity::Error
                )],
                "only the active checkout's diagnostic may render - the other client's is live in \
                 the same map and must be filtered out by the key, not by luck"
            );
        });
        assert!(
            cx.debug_bounds("sidebar-problems-row-0").is_some(),
            "and it must really paint as a row"
        );
        assert!(
            cx.debug_bounds("sidebar-problems-row-1").is_none(),
            "one diagnostic is one row"
        );

        // Now switch, for real. The list must swap - and this is also the path
        // `evict_stale_lsp_clients` runs on, so the old checkout's client is genuinely gone
        // afterwards rather than merely filtered.
        app.update_in(cx, |app, window, cx| app.select_worktree(1, window, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                rows(app),
                vec![(
                    "src/there.rs".to_string(),
                    44,
                    diagnostics_view::Severity::Warning
                )],
                "\u{a7}2: switching worktrees swaps the list"
            );
        });
        assert!(
            cx.debug_bounds("sidebar-problems-row-0").is_some(),
            "the Problems view stays selected across a worktree switch, showing the new \
             checkout's rows"
        );
    }

    #[gpui::test]
    fn a_row_for_a_never_opened_file_really_opens_it_at_the_diagnostics_line(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let root = repo.path().to_path_buf();
        let (buried, buried_uri) =
            real_file(&root, "src/db/orm/select.rs", &"// filler\n".repeat(60));

        let (app, cx) = crate::test_support::open_test_app(cx, root.clone());
        cx.run_until_parked();

        let server = spawn_fake_server(&root, "rust-analyzer", "normal");
        publish_full_and_wait(
            &server,
            &buried_uri,
            "no method named `order_by_nulls_last` on `QueryBuilder`",
            41,
            ERROR,
            "rust-analyzer",
        );
        app.update(cx, |app, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&server)),
            );
            cx.notify();
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.open_files().is_empty() && app.file_view_cache.is_none(),
                "premise: no editor has ever opened this file, or any other"
            );
            assert_eq!(
                rows(app),
                vec![(
                    "src/db/orm/select.rs".to_string(),
                    42,
                    diagnostics_view::Severity::Error
                )],
                "a file no buffer has ever been opened for still gets its row - the store is the \
                 server's whole published map, not the open document"
            );
        });

        show_problems(cx);
        let row = cx
            .debug_bounds("sidebar-problems-row-0")
            .expect("the row must paint");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.code_view,
                crate::code_surface::code_view::CodeView::File,
                "the click must really open the File view"
            );
            assert_eq!(
                app.file_view_cache
                    .as_ref()
                    .map(|cached| cached.path.clone()),
                Some(buried.clone()),
                "and it must really load the row's own file"
            );
            assert_eq!(
                app.code_cursor,
                Some(42),
                "landing on the diagnostic's own 1-based line, not on line 1 - the whole point of \
                 going through `open_file_at_line` rather than `open_file_view`"
            );
        });
    }

    #[gpui::test]
    fn the_header_and_the_marker_are_tallied_over_the_real_published_list(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let root = repo.path().to_path_buf();
        let (_, broken) = real_file(&root, "src/db/orm/select.rs", "fn a() {}\n");
        let (_, noisy) = real_file(&root, "src/db/query_builder.rs", "fn b() {}\n");
        let (_, chatty) = real_file(&root, "src/api/orders.rs", "fn c() {}\n");

        let (app, cx) = crate::test_support::open_test_app(cx, root.clone());
        cx.run_until_parked();

        let server = spawn_fake_server(&root, "rust-analyzer", "normal");
        publish_full_and_wait(&server, &broken, "unused import", 23, ERROR, "rustc");
        publish_full_and_wait(
            &server,
            &noisy,
            "this function has too many arguments (9/7)",
            65,
            WARNING,
            "clippy",
        );
        publish_full_and_wait(
            &server,
            &chatty,
            "consider using `let-else` here",
            202,
            HINT,
            "clippy",
        );
        app.update(cx, |app, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&server)),
            );
            cx.notify();
        });
        cx.run_until_parked();
        show_problems(cx);

        app.read_with(cx, |app, _| {
            let problems = app.worktree_problems();
            let tally = ProblemTally::over(&problems);
            assert_eq!(
                tally,
                ProblemTally {
                    errors: 1,
                    warnings: 1,
                    hints: 1
                }
            );
            assert_eq!(
                tally.count_line().as_deref(),
                Some("1 error \u{b7} 1 warning \u{b7} 1 hint"),
                "\u{a7}2: the header names every severity the list is showing - a pair that left \
                 the hint unnamed is the defect the rule was written for"
            );
            assert_eq!(
                tally.total(),
                problems.len(),
                "and the header's own total is the number of rows under it"
            );
            assert_eq!(
                tally.marker().expect("three diagnostics").tone,
                crate::rail::strip::MarkerTone::Failure,
                "red, because one of them is a real error"
            );
            assert_eq!(
                problems
                    .first()
                    .map(|problem| problem.severity)
                    .expect("a first row"),
                diagnostics_view::Severity::Error,
                "worst first"
            );
        });

        for selector in [
            "sidebar-problems-row-0",
            "sidebar-problems-row-1",
            "sidebar-problems-row-2",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "all three rows must really paint - {selector} did not"
            );
        }
        assert!(
            cx.debug_bounds("sidebar-strip-problems-marker").is_some(),
            "and the strip's own marker must really be there once the store is non-empty"
        );
    }

    #[gpui::test]
    fn a_diagnostic_about_a_file_outside_the_checkout_is_the_only_one_dropped(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let registry = crate::test_support::temp_root();
        let root = repo.path().to_path_buf();
        let (_, dependency) = real_file(
            registry.path(),
            "serde-1.0.0/src/lib.rs",
            "pub fn parse() {}\n",
        );
        // A second, in-checkout diagnostic from the *same* server, so an empty list cannot pass
        // this test for the wrong reason (a client that never went live, a publish that never
        // landed): the dependency row must be the only one dropped, not everything.
        let (_, mine) = real_file(&root, "src/lib.rs", "pub fn mine() {}\n");

        let (app, cx) = crate::test_support::open_test_app(cx, root.clone());
        cx.run_until_parked();

        let server = spawn_fake_server(&root, "rust-analyzer", "normal");
        publish_full_and_wait(
            &server,
            &dependency,
            "unresolved import",
            8,
            ERROR,
            "rust-analyzer",
        );
        publish_full_and_wait(&server, &mine, "unused variable", 3, WARNING, "rustc");
        app.update(cx, |app, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&server)),
            );
            cx.notify();
        });
        cx.run_until_parked();
        show_problems(cx);

        app.read_with(cx, |app, _| {
            assert_eq!(
                rows(app),
                vec![(
                    "src/lib.rs".to_string(),
                    4,
                    diagnostics_view::Severity::Warning
                )],
                "a dependency's own source is not a file in this checkout - and the checkout's \
                 own diagnostic proves the client and the publish both really worked"
            );
        });
        assert!(
            cx.debug_bounds("sidebar-problems-row-1").is_none(),
            "one row, not two"
        );
        let marker = cx
            .debug_bounds("sidebar-strip-problems-marker")
            .expect("the marker stands for the one real in-checkout diagnostic");
        assert_eq!(f32::from(marker.size.width), 5.0);
    }

    #[gpui::test]
    fn a_row_for_a_deleted_file_disappears_from_the_list_and_the_tally(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let root = repo.path().to_path_buf();
        let (doomed, doomed_uri) = real_file(&root, "src/scratch.rs", "fn tmp() {}\n");

        // A real settings path, not `open_test_app`'s `None`: `start_file_tree_watch` refuses to
        // arm without one, and this test's whole subject is that watcher noticing the deletion
        // (see the problems memo, GitHub issue #471).
        let settings_dir = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app_with_settings(
            cx,
            root.clone(),
            crate::settings::store::Settings::default(),
            Some(settings_dir.path().join("settings.toml")),
        );
        cx.run_until_parked();

        let server = spawn_fake_server(&root, "rust-analyzer", "normal");
        publish_full_and_wait(&server, &doomed_uri, "mismatched types", 0, ERROR, "rustc");
        app.update(cx, |app, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&server)),
            );
            cx.notify();
        });
        cx.run_until_parked();
        show_problems(cx);

        assert!(
            cx.debug_bounds("sidebar-problems-row-0").is_some(),
            "premise: while the file exists, its diagnostic really is a row"
        );

        // The server is never told. This is the real condition: an agent (or a `git checkout`)
        // removes a file, and the server's own published map still names it. The problems memo
        // (GitHub issue #471) only re-checks existence once the real file-tree watcher notices
        // the worktree changed, so this drives that whole path: the deletion reaches the OS
        // watcher in real time (`wait_until`'s bounded real-time loop), and `advance_clock`
        // ticks the watch loop's own timer arm.
        std::fs::remove_file(&doomed).expect("remove the file");
        let gone = test_support::wait_until(std::time::Duration::from_secs(5), || {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(600));
            cx.run_until_parked();
            app.read_with(cx, |app, _| app.worktree_problems().is_empty())
        });
        assert!(
            gone,
            "the server still publishes it; the checkout no longer contains it - the row must \
             drop once the file-tree watcher reports the deletion"
        );
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-problems-row-0").is_none(),
            "so the row must be gone"
        );
        assert!(
            cx.debug_bounds("sidebar-strip-problems-marker").is_none(),
            "and so must the marker it was the whole reason for"
        );
    }

    #[gpui::test]
    fn restarting_the_language_server_empties_the_list_rather_than_stranding_it(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let root = repo.path().to_path_buf();
        let (_, uri) = real_file(&root, "src/main.rs", "fn main() {}\n");

        let (app, cx) = crate::test_support::open_test_app(cx, root.clone());
        cx.run_until_parked();

        let server = spawn_fake_server(&root, "rust-analyzer", "normal");
        publish_full_and_wait(&server, &uri, "mismatched types", 0, ERROR, "rustc");
        app.update(cx, |app, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(Arc::clone(&server)),
            );
            cx.notify();
        });
        cx.run_until_parked();
        show_problems(cx);
        assert!(
            cx.debug_bounds("sidebar-problems-row-0").is_some(),
            "premise: the row is really there before the restart"
        );

        app.update(cx, |app, cx| app.restart_lsp_clients(cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.worktree_problems().is_empty(),
                "a restarted server has published nothing yet, and the view must say so rather \
                 than keep reporting what the old process said"
            );
        });
        assert!(
            cx.debug_bounds("sidebar-problems-note").is_some(),
            "the clean note takes the list's place"
        );
    }
}
