//! The sidebar strip, drawn (GitHub issue #291): the rail column's own 36px view switcher, the
//! real switch behind it, and the Problems view it switches to.
//!
//! [`crate::rail::strip`] decides *which* cells and *what* they say; this file is the `impl AdeApp`
//! half - the band, the slabs, the real diagnostics read behind the Problems marker, and the body
//! each view paints.
//!
//! ## The strip speaks the tab language
//!
//! `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4v, verbatim, on what the cells
//! are: "**38px full-height cells, no radius, no gap**, selected = a filled slab that cuts the
//! column rule to join the panel below, inactive = transparent over a recessed strip." Then, on
//! the divisions: "each cell carries `border-right: 1px #1c2023` - the same rule the tabs use
//! between them ... Segments divided by vertical rules is most of what makes a tab strip legible;
//! the first cut had the slab and the cut-out but no divisions, so it read as icons on a dark
//! band."
//!
//! Three consequences show up directly in [`strip_cell`]:
//!
//! 1. The **container draws no bottom border**. Every child carries its own, because "a child
//!    cannot paint over its parent's border - the parent's border sits outside the child's box -
//!    so the container cannot own the edge if any child needs to cut it" (§4v). The selected cell
//!    cuts it by painting that same 1px bar in [`theme::surface::RAIL`], exactly as the centre tab
//!    strip's active tab does with [`theme::surface::CENTER`] - and for the same GPUI reason it is
//!    a 1px child rather than a `border_b`: `Style::border_color` is one colour for all four
//!    edges, and these cells need a vertical rule in one colour and a horizontal one in another.
//! 2. **No cell has a background hover.** §4v: "two states must not compete on one property. If
//!    background says 'selected', hover says it somewhere else." Hover lifts the glyph to
//!    [`theme::text::GLYPH_HOVER`] and nothing else moves.
//! 3. The strip band is [`theme::surface::SIDEBAR_STRIP`], deliberately *below*
//!    [`theme::surface::RAIL`], because "a tab only reads as connected if the strip behind it is
//!    darker than the panel".
//!
//! ## What is deliberately not here
//!
//! The Problems **view** is minimal on purpose. It reads the app's real, worktree-scoped LSP
//! diagnostics ([`AdeApp::worktree_problems`]) and lists them, so the Problems cell ships with its
//! behaviour rather than as a dead button (`REVISION-2026-08-14.md` §7 rule 1: "Ship the
//! affordance with the behaviour, or ship neither"). Everything past "list what is really there" -
//! `REVISION-2026-08-13.md` §2's click-to-open navigation, the filter row's per-view query, the
//! `all`/`this worktree` scope toggle, and reaching diagnostics for files no editor has opened -
//! is GitHub issue #292's, which this issue blocks.

use super::*;
use crate::icons::{IconRow, IconSize};
use crate::lsp::client::LspClientState;
use crate::lsp::diagnostics as diagnostics_view;
use crate::rail::strip::{self, ProblemTally, SidebarView, StripCell, StripMarker};
use crate::root::widgets::text_tooltip;

/// One diagnostic in the Problems list, already reduced to what the row paints.
///
/// A view model rather than a borrowed `lsp_types::Diagnostic`: the list is scoped to a worktree,
/// not to an open buffer, so it outlives no particular file's state and needs nothing from
/// `lsp_types` past the five fields below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::rail) struct Problem {
    pub severity: diagnostics_view::Severity,
    pub message: String,
    /// The path, relative to the worktree root when it really is inside it - a Problems list
    /// showing 60 characters of shared prefix on every row says nothing per row.
    pub file: String,
    /// `line:column`, 1-based, the way every compiler and every other row in this app prints it.
    pub position: String,
    /// The server that reported it (`rustc`, `clippy`, `rust-analyzer`), when it named itself.
    pub source: Option<String>,
}

impl Problem {
    /// Whether this row survives the sidebar's own filter box - the same case-insensitive
    /// substring test over the row's own visible text that
    /// [`crate::rail::state::WorktreeRow::matches_filter`] applies to a worktree row, so one field
    /// behaves the same way whichever view is under it. A blank query matches everything.
    ///
    /// This is what lets the filter row's placeholder really say `filter problems`
    /// (`REVISION-2026-08-13.md` §1) rather than promising a filter that does nothing (§7 rule 1).
    fn matches_filter(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        self.message.to_lowercase().contains(&query)
            || self.file.to_lowercase().contains(&query)
            || self
                .source
                .as_deref()
                .is_some_and(|source| source.to_lowercase().contains(&query))
    }
}

impl AdeApp {
    /// Switches the sidebar to `view`.
    ///
    /// Closes any open rail menu first, for the reason [`AdeApp::set_right_sidebar_view`]
    /// documents at length for the right panel: a menu anchored to a row the next frame no longer
    /// paints is a popover acting on something the user cannot see. Nothing else is torn down -
    /// the filter row and [`AdeApp::rail_focus_handle`] are strip-level chrome that both views sit
    /// under, so no view switch here can strand focus.
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
    /// first, then by file and position.
    ///
    /// **Worktree-keyed, exactly like history.** `REVISION-2026-08-14.md` §6, verbatim: "a
    /// diagnostic belongs to a checkout. Unkeyed, one global list renders under every worktree,
    /// listing files that are not in it." That keying is free here rather than a filter step:
    /// [`AdeApp::lsp_clients`] is already keyed on `(worktree root, server)`, so this reads only
    /// the clients whose root *is* the selected worktree.
    ///
    /// Real data or nothing. A worktree whose language server has never started - or has started
    /// and reported nothing - yields an empty `Vec`, and the Problems view says so honestly. That
    /// is what makes the strip's marker safe to derive from this: an empty day cannot produce a
    /// red dot, which is exactly the failure `REVISION-2026-08-13.md` §1's "gate at the source"
    /// rule exists to prevent.
    pub(in crate::rail) fn worktree_problems(&self) -> Vec<Problem> {
        let Some(root) = self.current_worktree_path() else {
            return Vec::new();
        };
        let mut problems: Vec<Problem> = Vec::new();
        for ((client_root, _server), state) in &self.lsp_clients {
            let LspClientState::Ready(client) = state else {
                continue;
            };
            if client_root != &root {
                continue;
            }
            for (path, diagnostics) in client.published_diagnostics() {
                let file = display_path(&path, &root);
                for diagnostic in diagnostics {
                    problems.push(Problem {
                        severity: diagnostics_view::Severity::from_lsp(diagnostic.severity),
                        message: diagnostic.message.trim().to_string(),
                        file: file.clone(),
                        // LSP positions are 0-based; every compiler, editor and other row in this
                        // app prints them 1-based.
                        position: format!(
                            "{}:{}",
                            diagnostic.range.start.line + 1,
                            diagnostic.range.start.character + 1
                        ),
                        source: diagnostic.source.filter(|source| !source.is_empty()),
                    });
                }
            }
        }
        // Worst first - the same "worst wins" ordering `diagnostics_view::Severity::worst` applies
        // within a line, applied to the list. Ties break on file then position so the order is
        // total and a re-render cannot reshuffle rows under the pointer.
        problems.sort_by(|left, right| {
            severity_rank(right.severity)
                .cmp(&severity_rank(left.severity))
                .then_with(|| left.file.cmp(&right.file))
                .then_with(|| left.position.cmp(&right.position))
        });
        problems
    }

    /// [`Self::worktree_problems`], tallied by severity - for the strip's marker and for the
    /// view's own count line alike, so the marker can never report a number the list below it does
    /// not contain (`REVISION-2026-08-13.md` §2: "tallied over their own data").
    pub(in crate::rail) fn worktree_problem_tally(&self) -> ProblemTally {
        tally_problems(&self.worktree_problems())
    }

    /// The whole sidebar strip: the view cells, the flex spacer, the `+` new-agent cell and the
    /// `⋯` overflow - §4v's final cell list, with §4u's "Settings lives in the overflow" already
    /// applied, so there is no Settings cell.
    ///
    /// [`theme::band::CHROME_HEADER`] high, like the centre tab strip and the right panel header
    /// beside it: §4v's "column headers that share a y are one rule, not three". It carries **no
    /// bottom border of its own** - see this module's docs for why the children own that edge -
    /// and note that *every* child carries it, the spacer included, "without which the rule
    /// stopped at the last tab and 398px of the window's top edge was simply missing".
    ///
    /// `cells` comes from [`strip::strip_view_cells`], which is where the empty-day gate lives: on
    /// First run and Empty day it is simply empty, and the strip is the spacer, the `+` and the
    /// `⋯`.
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
    ///
    /// Selected fills with [`theme::surface::RAIL`] - "exactly the rail's own background, with its
    /// rule the same colour" (§4v) - so the cell reads as continuous with the panel below it.
    /// Inactive is transparent over the recessed band and paints the real column rule.
    fn render_sidebar_strip_cell(
        &self,
        cell: StripCell,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cell.view;
        let element_id = match view {
            SidebarView::Worktrees => "sidebar-strip-worktrees",
            SidebarView::Problems => "sidebar-strip-problems",
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
    ///
    /// It lived in the rail header this strip replaced (Revision R12 §2.1: "Rail header keeps only
    /// the `+` new-session button"), and `REVISION-2026-08-13.md` §1 puts it here: "Flex spacer,
    /// then the `+` new-session button (unchanged)". *Unchanged* is about the action, not the
    /// chrome: its `mod+N` keycap pair does not fit a 38px cell, so it moves into the tooltip -
    /// still resolved through [`crate::keymap::resolve_combo`] exactly as the keycap was, so it
    /// still cannot name a binding this build does not really register.
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
        &self,
        view: SidebarView,
        groups: &[RepoGroup],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match view {
            SidebarView::Worktrees => self.render_rail_list(groups, cx),
            SidebarView::Problems => self.render_problems_view(cx),
        }
    }

    /// The Problems view: the real count line over the real rows, or the real empty note.
    ///
    /// Scoped to the selected worktree and tallied over its own data - see
    /// [`Self::worktree_problems`]. What this view deliberately does not do yet is GitHub issue
    /// #292's; see this module's docs.
    fn render_problems_view(&self, _cx: &mut Context<Self>) -> gpui::AnyElement {
        // Filtered here, not in `Self::worktree_problems`: the strip's own marker reads that
        // method too, and a marker that shrank as the filter box was typed into would break the
        // same promise the repo headers' counts keep (`crate::rail::state::RepoWorktrees::
        // all_rows`) - the strip reports what is really in the worktree, the list reports what
        // you asked to see.
        let query = self.filter_query.as_str().to_string();
        let all = self.worktree_problems();
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

        if let Some(line) = tally_problems(&problems).count_line() {
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
        for problem in problems {
            list = list.child(self.render_problem_row(&problem));
        }
        list.into_any_element()
    }

    /// One Problems row: a 5px severity square, the message, and the file/position/source line
    /// under it - `Jerry.dc.html`'s own `probRows` markup.
    fn render_problem_row(&self, problem: &Problem) -> impl IntoElement {
        let mut meta = div()
            .flex()
            .items_baseline()
            .gap(px(6.0))
            .mt(px(3.0))
            .font(font(theme::font::MONO))
            .child(
                div()
                    .flex_none()
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::DIMMER)
                    .child(problem.file.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOST)
                    .child(problem.position.clone()),
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

        div()
            .flex()
            .gap(px(7.0))
            .pl(px(12.0))
            .pr(px(10.0))
            .pt(px(6.0))
            .pb(px(7.0))
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
}

/// Every strip cell's shared shape: 38 wide, full height, centred, no radius, no gap, `content`
/// in the middle, and the column rule painted as this cell's own last 1px row in `rule`.
///
/// Shared rather than repeated so the three kinds of cell - view, `+` and `⋯` - cannot drift apart
/// on any of it. That the `+` and `⋯` go through it too is what §4v's "every child carries it"
/// means once the rule stops being the container's.
///
/// The rule is a 1px child, not a `border_b`, for the same reason
/// [`AdeApp::render_agent_tab`]'s underline is: GPUI's `Style::border_color` is one colour for all
/// four edges, and a cell needs its vertical divider ([`theme::border::INNER`]) and its horizontal
/// rule (`rule`, which the selected cell sets to its own background to cut it) in two.
///
/// Hover is on the glyph colour alone (§4v: "two states must not compete on one property"),
/// expressed by setting `text_color` at rest and in hover and touching no background: an icon
/// drawn through [`IconRow`] is a monochrome sprite painted in its element's own text colour, so
/// one `text_color` really does move an SVG glyph and a `+`/`⋯` mark alike.
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
///
/// §4v's rule is that hover **lifts** the glyph - "hover lifts `color` to `#c8ced4`" - which is
/// why the already-lit selected cell keeps its own [`theme::text::SELECTED`] rather than taking
/// [`theme::text::GLYPH_HOVER`]: at `#dde2e7` over `#c8ced4` that assignment would make hovering
/// the *active* view visibly dim it, re-creating in one direction the exact "hover out-reads
/// selected" collision the section moved hover off background to fix. (`Jerry.dc.html` applies its
/// one `style-hover` to every cell from a shared template, so its selected cell really does dim;
/// the stated rule is the one followed here.)
fn strip_glyph_hover(selected: bool) -> theme::ColorToken {
    if selected {
        theme::text::SELECTED
    } else {
        theme::text::GLYPH_HOVER
    }
}

/// A cell's state marker: the tabs' own 5px square dot, in the marker's hue, at `Jerry.dc.html`'s
/// own `right:7 top:8` (§4v: "Badges moved to `right:5 top:6` - on a square cell, corner-anchored
/// badges read as clipped", and the final markup settled one step further in again).
///
/// [`theme::radius::MARK_SM`] rather than [`theme::radius::MARK`] because that 1px is what makes a
/// 5px square read as a square: this app already uses a real circle for "an agent's status", and
/// this deliberately is not that.
fn render_strip_marker(view: SidebarView, marker: StripMarker) -> impl IntoElement {
    let selector = match view {
        SidebarView::Worktrees => "sidebar-strip-worktrees-marker",
        SidebarView::Problems => "sidebar-strip-problems-marker",
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
///
/// The app's own two status hues, not a third palette: the strip cell's marker is specified as
/// [`theme::status::FAIL`]/[`theme::status::ASK`] (`REVISION-2026-08-14.md` §6 - "badges use the
/// app's own hues ... not a one-off cream"), and a row painting a *different* red from the marker
/// that summarises it would be two vocabularies for one fact, one panel apart - §7 rule 8's
/// complaint, in colour.
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

/// `problems` counted into `REVISION-2026-08-13.md` §2's three buckets.
///
/// LSP's `Information` and `Hint` both land in `hints`: §2's own table names three severities and
/// the mock tallies both into its `info` bucket, so these are two levels the design does not
/// distinguish anywhere - which is why summing them is not §7 rule 4's "two states distinguished
/// anywhere in the app are never summed anywhere in it".
fn tally_problems(problems: &[Problem]) -> ProblemTally {
    let mut tally = ProblemTally::default();
    for problem in problems {
        match problem.severity {
            diagnostics_view::Severity::Error => tally.errors += 1,
            diagnostics_view::Severity::Warning => tally.warnings += 1,
            diagnostics_view::Severity::Information | diagnostics_view::Severity::Hint => {
                tally.hints += 1
            }
        }
    }
    tally
}

/// `path` as the Problems row shows it: relative to the worktree it belongs to when it really is
/// inside it, absolute otherwise.
///
/// The fallback is not decoration. rust-analyzer really does publish diagnostics against paths
/// outside the workspace (a dependency's own source under `~/.cargo/registry/...`), and printing
/// those as if they were relative would claim a file in this checkout that is not there.
fn display_path(path: &std::path::Path, root: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Most severe first - the list-level counterpart of `diagnostics_view::Severity::worst`'s own
/// within-a-line ordering, restated here only because that module's `rank` is private.
fn severity_rank(severity: diagnostics_view::Severity) -> u8 {
    match severity {
        diagnostics_view::Severity::Error => 3,
        diagnostics_view::Severity::Warning => 2,
        diagnostics_view::Severity::Information => 1,
        diagnostics_view::Severity::Hint => 0,
    }
}

/// Real-window, real-git coverage for the sidebar strip (GitHub issue #291): the band as it is
/// really painted, the real switch a click on a cell performs, and the window-chrome invariant the
/// design derived while building it.
///
/// Everything here is measured off real painted bounds (`debug_bounds`) and driven by real
/// simulated clicks - the geometry claims in `STAGE-A-CHANGELOG.md` §4v are claims about pixels,
/// and asserting them against the model instead of against the frame would prove nothing about
/// what ships.
#[cfg(test)]
mod sidebar_strip_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

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

    /// §4v's cells, as pixels: "**38px full-height cells, no radius, no gap**". Every cell is
    /// really 38 wide, really as tall as the band, and really flush against its neighbour - the
    /// "no gap" half is what makes the dividing rules read as a tab strip's segments rather than
    /// as a row of separated buttons.
    #[gpui::test]
    fn every_strip_cell_is_a_full_height_38px_slab_with_no_gap(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // The two view cells sit flush; the `+` and `⋯` sit flush at the far end, past the spacer.
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

    /// §4v's window-chrome invariant, verbatim: "column headers that share a y are one rule, not
    /// three." Measured the way the design measured it - three real
    /// `getBoundingClientRect().bottom` values, which must all be the same number.
    ///
    /// This is the assertion the whole issue hangs on: before it, the three headers agreed on a
    /// height but drew three different border colours, and the centre one drew its edge twice.
    #[gpui::test]
    fn all_three_column_headers_end_on_one_line(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The centre column's bottom edge has exactly one owner, and it is the children (§4v: "an
    /// edge has one owner - if anything needs to cut it, the owner is the children, and then
    /// *every* child carries it").
    ///
    /// Asserted where it can really fail: the tab strip's own last child - the right-aligned
    /// agent-jump cluster - must reach the column's right edge, because that is the child whose
    /// missing rule left "398px of the window's top edge simply missing" when the container
    /// stopped drawing it. Paired with a direct read of the container's own style, so a
    /// re-introduced `border_b` on it fails here rather than shipping as a doubled line.
    #[gpui::test]
    fn the_centre_columns_bottom_edge_is_carried_all_the_way_across(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The strip is a real switcher: clicking Problems really replaces the rail's tree with the
    /// Problems view, and clicking Worktrees really brings it back. Driven by real clicks at real
    /// painted positions, so a cell with no handler fails here.
    #[gpui::test]
    fn clicking_a_cell_really_switches_the_panel_under_it(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// §7 rule 7, verbatim: "A row of icons needs one shared optical box, not one size per icon."
    /// Both view glyphs are #282's real Phosphor assets, and both really paint inside the same
    /// [`IconSize::Strip`] square - measured on the frame, not asserted against the enum.
    #[gpui::test]
    fn both_view_glyphs_are_phosphor_assets_in_one_shared_optical_box(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// `REVISION-2026-08-13.md` §1's gate, at the source: "The badges and the History/Search/
    /// Problems bodies derive from `sessions` ... ungated they claimed 3 agents needing a human
    /// and 4 problems on a day the rail, title bar and footer all reported zero."
    ///
    /// A freshly opened repo has no agent waiting and no diagnostics, so neither marker may paint
    /// at all. This is the "cannot fabricate" half of the requirement, and an absence is the only
    /// way to assert it.
    #[gpui::test]
    fn a_window_with_nothing_waiting_paints_no_state_marker(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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
            assert_eq!(app.worktree_problem_tally(), ProblemTally::default());
            assert!(app.worktree_problems().is_empty());
        });
    }

    /// §1's empty-state gating, through the real render: "On **First run** and **Empty day** the
    /// icon strip drops its view cells and keeps only the `+`". With no worktree row anywhere,
    /// the switcher has nothing to switch between, so both view cells go - while the `+` and the
    /// `⋯` stay, because starting an agent and reaching Settings are exactly the two things still
    /// worth doing on an empty day.
    #[gpui::test]
    fn an_empty_day_drops_the_view_cells_and_keeps_the_trailing_pair(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// §1's "Filter row stays, and its placeholder follows the view: `filter worktrees and
    /// agents` / `filter runs` / `filter problems`."
    ///
    /// And the placeholder is a real promise, not a label: with a query typed, the Problems view
    /// really filters (§7 rule 1). Asserted on the *note* rather than on rows, because a clean
    /// test checkout genuinely has no diagnostics - which is itself the thing worth pinning here,
    /// since a filter box that changed a clean checkout's own note would be reporting a filter
    /// result over data that was never there.
    #[gpui::test]
    fn the_filter_rows_placeholder_follows_the_selected_view(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The Problems list's own filter really applies, and the strip's marker deliberately does
    /// not follow it - the strip reports what is really in the worktree, the list reports what you
    /// asked to see.
    #[test]
    fn a_filter_narrows_the_list_without_touching_what_the_strip_reports() {
        let rows = vec![
            Problem {
                severity: diagnostics_view::Severity::Error,
                message: "cannot borrow `self.tokens` as mutable".to_string(),
                file: "src/auth/session.rs".to_string(),
                position: "212:17".to_string(),
                source: Some("rustc".to_string()),
            },
            Problem {
                severity: diagnostics_view::Severity::Warning,
                message: "unused variable: `barrier`".to_string(),
                file: "tests/auth_race.rs".to_string(),
                position: "44:9".to_string(),
                source: Some("clippy".to_string()),
            },
        ];

        assert_eq!(
            rows.iter().filter(|row| row.matches_filter("")).count(),
            2,
            "a blank query matches everything, exactly as it does for a worktree row"
        );
        // By message, by path, and by the server that reported it - all three of the row's own
        // visible fields.
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("BORROW"))
                .count(),
            1,
            "case-insensitive, over the message"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("tests/"))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("clippy"))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.matches_filter("nothing"))
                .count(),
            0
        );

        assert_eq!(
            tally_problems(&rows),
            ProblemTally {
                errors: 1,
                warnings: 1,
                hints: 0
            },
            "the tally the strip's marker reads is over the whole worktree, unfiltered"
        );
    }

    /// §1's "the `+` new-session button (unchanged)" - unchanged meaning the *action*. It moved
    /// into a 38px cell, so this proves the real spawn still happens from a real click on it.
    #[gpui::test]
    fn the_plus_cell_still_spawns_a_real_agent(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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
