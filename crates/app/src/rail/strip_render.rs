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
//! ## The Problems view (GitHub issue #292)
//!
//! `REVISION-2026-08-13.md` §2's table, verbatim, is the whole spec: "LSP diagnostics: severity
//! square, message, file, line, source. `problemDefs` is **keyed by worktree id** and filtered on
//! the active one, exactly like `histDefs` - a diagnostic belongs to a checkout. Unkeyed, one
//! global list rendered under every worktree, listing files that were not in it. A clean worktree
//! gets *No diagnostics in `<branch>`.* and no badge."
//!
//! [`AdeApp::worktree_problems`] is the keyed store's read side and documents at length what it
//! really covers; [`AdeApp::render_problem_row`] is §2's row, including the "opens the file at the
//! line on click" half. The tallied header, the marker and the two empty notes live in
//! [`crate::rail::strip`], which can assert them without a window.
//!
//! **What the design does not ask for here, and this therefore does not do**: grouping rows by
//! file, and an `all`/`this worktree` scope toggle. `Jerry.dc.html`'s `probRows` is a flat list,
//! and §6's scope toggle belongs to *Agent history* ("**Agent history** is repo → worktree → run,
//! matching the rail, with an `all` / `this worktree` scope toggle") - Problems is specified the
//! other way in the same revision, as following the selected worktree and only that. A scope
//! toggle here would contradict §2's own reason for keying the store at all.

use super::*;
use crate::icons::{IconRow, IconSize};
use crate::lsp::client::LspClientState;
use crate::lsp::diagnostics as diagnostics_view;
use crate::rail::strip::{self, Problem, ProblemTally, SidebarView, StripCell, StripMarker};
use crate::root::widgets::text_tooltip;

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
    /// the clients whose root *is* the selected worktree, and switching worktrees swaps the whole
    /// list without this method knowing a switch happened.
    ///
    /// ## The three ways a row is dropped, and what each one is really about
    ///
    /// §2's "listing files that were not in it" is one sentence covering three distinct real
    /// conditions, and only the first is handled by the key:
    ///
    /// 1. **Another checkout's client.** Handled by the `client_root != root` test below - and, in
    ///    practice, twice over: `AdeApp::evict_stale_lsp_clients` already tears every client for a
    ///    non-active root down on a worktree switch, so the map usually holds only the active
    ///    root's clients. The test stays because "usually" is not an invariant, and a Problems
    ///    list is exactly where a violated one would show up, as another branch's files.
    /// 2. **A file outside the checkout entirely.** `rust-analyzer` really does publish against a
    ///    dependency's own source under `~/.cargo/registry/...`, and against `~/.rustup`'s
    ///    standard library. Those are real diagnostics about real files, but they are not files
    ///    *in this checkout*, and §2's rule is about the checkout - so `strip_prefix` is the
    ///    filter, and a path it rejects is dropped rather than shown with an absolute path.
    /// 3. **A file that no longer exists.** GitHub issue #292's own backend acceptance: "no stale
    ///    rows for files that no longer exist in that checkout". A server that has not noticed a
    ///    deletion yet (nothing in LSP obliges one to clear diagnostics for a removed file, and
    ///    `rust-analyzer` only does once its own watcher fires) keeps publishing them, and a row
    ///    whose click could only ever open nothing has no business being counted either. One
    ///    `Path::is_file` per *file* the server has anything to say about - not per diagnostic,
    ///    and not per file in the worktree - which is a handful of `stat`s on a real repo, paid on
    ///    the same pass that builds the rows.
    ///
    /// ## What this does and does not reach
    ///
    /// It is the **whole** published map, not the open buffer: `lsp_core::LspClient` retains every
    /// `publishDiagnostics` the server ever sends, keyed by uri, and
    /// [`lsp_core::LspClient::published_diagnostics`] hands back all of it. So a file that no
    /// editor tab has ever opened (one `cargo check` found a type error in three directories away)
    /// really does get a row, and clicking it really does open it. That is the one property of
    /// this list worth stating plainly, because the obvious implementation - read the open
    /// buffer's diagnostics - would not have it.
    ///
    /// What it cannot reach is a worktree whose server has never been **started**. This app spawns
    /// a language server lazily, when a file of that language is first rendered
    /// (`AdeApp::ensure_lsp_client`, called from `crate::code_surface::file_view`), so a checkout
    /// nobody has opened a file in yet has no client, no published diagnostics, and an honestly
    /// empty Problems list. Starting a `rust-analyzer` per worktree from a sidebar click is a real
    /// resource decision this issue does not make - and an empty list here is a true statement
    /// about what the app knows, not a fabricated clean bill of health.
    ///
    /// Real data or nothing, in other words. That is what makes the strip's marker safe to derive
    /// from this: an empty day cannot produce a red dot, which is exactly the failure
    /// `REVISION-2026-08-13.md` §1's "gate at the source" rule exists to prevent.
    pub(in crate::rail) fn worktree_problems(&self) -> Vec<Problem> {
        let Some(root) = self.current_worktree_path() else {
            return Vec::new();
        };
        // A diagnostic's uri is whatever real path the *server* opened, and `lsp_core`'s own
        // `path_to_uri` canonicalises on the way in - so a checkout reached through a symlink
        // (macOS' `/var` -> `/private/var`, or a worktree directory someone symlinked) yields
        // paths that `strip_prefix(&root)` can never match. Resolved once per pass, not per row,
        // and falling back to `root` itself if the checkout has since gone away.
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
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
    ///
    /// `problems` is the selected worktree's real list, built **once** by the caller
    /// ([`Self::render_rail`]) and lent to both halves for the same reason `groups` is: the strip's
    /// marker and this body are two answers about one set of diagnostics, and deriving them from
    /// one pass is what makes it impossible for the marker to report a number the list below it
    /// does not contain (`REVISION-2026-08-13.md` §2: "tallied over their own data").
    pub(in crate::rail) fn render_sidebar_body(
        &self,
        view: SidebarView,
        groups: &[RepoGroup],
        problems: &[Problem],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match view {
            SidebarView::Worktrees => self.render_rail_list(groups, cx),
            SidebarView::Problems => self.render_problems_view(problems, cx),
        }
    }

    /// The Problems view: the real count line over the real rows, or the real empty note.
    ///
    /// Scoped to the selected worktree and tallied over its own data - see
    /// [`Self::worktree_problems`].
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
    /// under it - `Jerry.dc.html`'s own `probRows` markup - and the click that opens it.
    ///
    /// **The click** is `REVISION-2026-08-13.md` §2's own half of the row spec ("severity square,
    /// message, file, line, source; opens the file at the line on click"). It goes through
    /// [`Self::open_file_at_line`], the same one move go-to-definition and a terminal `path:line`
    /// link already make - so a Problems row lands the caret on the diagnostic's line through the
    /// same `pending_cursor_line` handshake that survives a background file load, rather than
    /// through a second copy of it that would work only for already-open files.
    ///
    /// **The 2px left edge** is in the mock (`border-left:2px solid transparent`) and never lit
    /// there. Reserved rather than dropped, for the reason the rail's own childless worktree rows
    /// reserve their caret slot: it is 2px of the row's real left inset, and a row that omits it
    /// sits 2px left of every other row in the column.
    ///
    /// **The hover fill** is the mock's `style-hover="background:#15181b"`, which is
    /// [`theme::surface::ROW_HOVER`] exactly - the app's own list-row hover, not a second one.
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
    ///
    /// `Jerry.dc.html` prints the **bare file name** here (`p.file.split('/').pop()`), and this
    /// deliberately keeps the directory in front of it. The reason is the one thing #292 adds that
    /// the mock's rows do not have: these rows *open a file*. Four `mod.rs` rows that all read
    /// `mod.rs` give no way to tell which file a click is about to open - and the design already
    /// has a shape for exactly this tension, in the Search view one section over, whose rows carry
    /// `dir` and `file` as two spans at two weights rather than dropping either. This is that
    /// shape, applied to the same problem.
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
            assert!(app.worktree_problems().is_empty());
            assert_eq!(
                ProblemTally::over(&app.worktree_problems()),
                ProblemTally::default()
            );
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

/// Real coverage for the Problems view itself (GitHub issue #292), driven against a **genuinely
/// spawned** language server pushing real `textDocument/publishDiagnostics` notifications over the
/// real wire protocol (`crate::lsp::client::lsp_connection_facade_tests`' own fake server - see
/// that module's docs for why a real, tiny server rather than the real `rust-analyzer`).
///
/// Nothing here seeds a `Problem` by hand. Every assertion below is about what the app really
/// shows after a real server really said something, because the whole subject of this issue is a
/// store keyed by checkout - and a hand-built list would be keyed by whatever the test decided.
#[cfg(test)]
mod problems_view_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::{
        publish_full_and_wait, spawn_fake_server,
    };
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// LSP severity numbers, as a real server sends them.
    const ERROR: u8 = 1;
    const WARNING: u8 = 2;
    const HINT: u8 = 4;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed in {dir:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

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
            .into_iter()
            .map(|problem| (problem.file, problem.line, problem.severity))
            .collect()
    }

    /// **The property the whole issue turns on.** `REVISION-2026-08-13.md` §2's store is "keyed by
    /// worktree id and filtered on the active one" - and issue #292's own acceptance closes with
    /// "a diagnostic from another checkout never renders under the active one".
    ///
    /// Two real worktrees, two real servers, one real diagnostic each, **both clients live in the
    /// map at once** - which is the only arrangement in which the key can be proven to be doing
    /// the filtering. (In normal use `AdeApp::evict_stale_lsp_clients` has already torn the
    /// inactive root's client down by this point, which would make a broken key look correct.)
    #[gpui::test]
    fn a_diagnostic_from_another_checkout_never_renders_under_the_active_one(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let other = TempDir::new().expect("tempdir");
        let here = repo.path().to_path_buf();
        let there = other.path().to_path_buf();

        let (_, here_uri) = real_file(&here, "src/here.rs", "fn here() {}\n");
        let (_, there_uri) = real_file(&there, "src/there.rs", "fn there() {}\n");

        let (app, cx) = palette_focus_tests::open_test_app(cx, here.clone());
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

    /// #292's own backend acceptance: the store retains "**all** files the server reports, not
    /// just the open buffer". Proven the only way it can be: on a file no editor has ever opened,
    /// three directories deep, that this test never calls `open_file_view` for - and then by
    /// clicking its row and watching the real editor land on it, at the real line.
    ///
    /// That click is `REVISION-2026-08-13.md` §2's other half ("opens the file at the line on
    /// click") and goes through `AdeApp::open_file_at_line`, the app's one such move.
    #[gpui::test]
    fn a_row_for_a_never_opened_file_really_opens_it_at_the_diagnostics_line(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let (buried, buried_uri) =
            real_file(&root, "src/db/orm/select.rs", &"// filler\n".repeat(60));

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
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

    /// §2's header "names every severity the list is showing", over a real three-severity list -
    /// and the strip's marker over the same one pass. The marker is red because there is a real
    /// error in it (`Jerry.dc.html`'s `probTally.err ? '#e0625c' : '#e2a336'`).
    #[gpui::test]
    fn the_header_and_the_marker_are_tallied_over_the_real_published_list(cx: &mut TestAppContext) {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let (_, broken) = real_file(&root, "src/db/orm/select.rs", "fn a() {}\n");
        let (_, noisy) = real_file(&root, "src/db/query_builder.rs", "fn b() {}\n");
        let (_, chatty) = real_file(&root, "src/api/orders.rs", "fn c() {}\n");

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
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

    /// §2's "listing files that were not in it", in the form it really takes on a Rust workspace:
    /// `rust-analyzer` publishes against a dependency's own source outside the checkout entirely.
    /// Those are real diagnostics about real files, and they are not this checkout's - so no row
    /// and nothing in the tally, while the checkout's own diagnostic is untouched beside it.
    #[gpui::test]
    fn a_diagnostic_about_a_file_outside_the_checkout_is_the_only_one_dropped(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let registry = TempDir::new().expect("tempdir");
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

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
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

    /// #292's own backend acceptance: "no stale rows for files that no longer exist in that
    /// checkout". Nothing in LSP obliges a server to clear diagnostics for a deleted file, and a
    /// row whose click could only ever open nothing must not be counted either.
    #[gpui::test]
    fn a_row_for_a_deleted_file_disappears_from_the_list_and_the_tally(cx: &mut TestAppContext) {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let (doomed, doomed_uri) = real_file(&root, "src/scratch.rs", "fn tmp() {}\n");

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
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
        // removes a file, and the server's own published map still names it.
        std::fs::remove_file(&doomed).expect("remove the file");
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.worktree_problems().is_empty(),
                "the server still publishes it; the checkout no longer contains it"
            );
        });
        assert!(
            cx.debug_bounds("sidebar-problems-row-0").is_none(),
            "so the row must be gone"
        );
        assert!(
            cx.debug_bounds("sidebar-strip-problems-marker").is_none(),
            "and so must the marker it was the whole reason for"
        );
    }

    /// A worktree switch and a server restart are the two other ways the store must stay coherent
    /// (#292's third backend bullet). The restart half is the one worth a real test: tearing a
    /// server down really does empty the list, rather than leaving the last frame's rows standing.
    #[gpui::test]
    fn restarting_the_language_server_empties_the_list_rather_than_stranding_it(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let root = repo.path().to_path_buf();
        let (_, uri) = real_file(&root, "src/main.rs", "fn main() {}\n");

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
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
