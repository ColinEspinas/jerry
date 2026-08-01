//! The real GPUI surface for the git graph tab - tab strip entry, toolbar, lane canvas + row
//! list, row `⋯` menu, Push `▾` menu, and the Commit/Branches right panel - plus the `impl
//! AdeApp` glue that opens/closes/loads it. See `super`'s module docs for scope.

use super::*;
use crate::root::widgets::{render_sidebar_message, render_tag_pill};
use crate::settings::widgets;
use crate::sidebar::changes;
use crate::work_surface::render::render_dropdown_menu_row;
use gpui::{BoxShadow, KeyDownEvent, Pixels};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wt_core::graph::{DotKind, ElbowKind, Graph, GraphRow, GraphScope, RefKind};

impl AdeApp {
    /// Opens the git graph tab (the tab strip's own entry, the `+` menu's "Git graph" row, the
    /// palette's "Open git graph", `mod+shift+G`, and the status bar's branch cluster all funnel
    /// through this). Idempotent: re-invoking while already open just re-activates it (used by
    /// the tab's own click handler too, so there is exactly one open/activate code path).
    pub(crate) fn open_git_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Mirrors `Self::open_settings`'s own defensive top-of-function close: reachable directly
        // (the status bar cluster, the `+` menu, `mod+shift+G`) while the palette also happens to
        // be open, not just via `crate::palette::render::AdeApp::execute_palette_command`'s own
        // "Open git graph" entry (which never hits this branch - the palette closes itself around
        // that call instead, see `Self::run_selected_palette_entry`'s docs).
        if self.palette_open {
            self.close_palette(window, cx);
        }
        self.graph_tab_open = true;
        let was_active = self.graph_tab_active;
        self.graph_tab_active = true;
        self.open_change = None;
        self.plus_menu_open = false;
        self.title_menu_open = None;
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;

        // Opening the graph tab replaces the right sidebar's Files/Changes panel with Commit/
        // Branches (`Self::render_right_sidebar`), unrendering the file tree exactly the way
        // switching to the Changes tab does - the same "leaving Files" dangling-focus sweep
        // `crate::sidebar::render::AdeApp::set_right_sidebar_view` performs, reached here through
        // a different door. See that function's own docs for the real bug class this closes.
        self.tree_context_menu = None;
        self.tree_inline_edit = None;
        if self.tree_focus_handle.is_focused(window) {
            restore_focus(&self.agents, &mut self.code_focus, window, cx);
        }
        self.palette_focus.forget_target(&self.tree_focus_handle);
        self.settings_focus.forget_target(&self.tree_focus_handle);
        self.code_focus.forget_target(&self.tree_focus_handle);

        // Same sweep, for the code surface this `self.open_change = None` above just unrendered
        // (`Self::render_center_pane` only mounts it for `Some(open_path)`) - a real, adversarial-
        // audit-found gap: without this, a file tab focused right before opening the graph tab
        // left `code_focus_handle` captured as this tab's own return target, a handle that stops
        // being rendered the moment `open_change` clears.
        if self.code_focus_handle.is_focused(window) {
            restore_focus(&self.agents, &mut self.code_focus, window, cx);
        }
        self.palette_focus.forget_target(&self.code_focus_handle);
        self.settings_focus.forget_target(&self.code_focus_handle);
        // `open_change` just changed; every cache keyed on it must follow, exactly like every
        // other site that clears it (`crate::root::state::AdeApp::select_worktree`,
        // `crate::code_surface::tabs::AdeApp::close_file_tab`).
        self.refresh_open_diff_file_cache();

        if !was_active && !self.focus_is_on_an_overlay(window, cx) {
            self.graph_focus.capture(window, &self.agents, cx);
        }
        window.focus(&self.graph_focus_handle, cx);

        if matches!(self.graph_state.load, GraphLoadState::NotLoaded) {
            self.load_graph(cx);
        }
        cx.notify();
    }

    /// `mod+shift+G` (`NewGitGraph`, bound in `crate::default_key_bindings`). The palette's own
    /// "Open git graph" entry and the tab's `+` menu both call [`Self::open_git_graph`] directly;
    /// this is the action-dispatch door for the raw keystroke, matching every other global
    /// shortcut's `handle_*_action` -> `on_action` wiring in `root::mod::AdeApp::render`.
    pub(crate) fn handle_new_git_graph_action(
        &mut self,
        _action: &NewGitGraph,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_git_graph(window, cx);
    }

    /// Closes the git graph tab outright (its `×`), removing it from the tab strip. Dropping the
    /// cached [`GraphLoadState`] back to `NotLoaded` means a later re-open does a fresh load
    /// rather than showing a stale snapshot - cheap insurance since re-opening is exactly when a
    /// user most likely wants to see what changed since they last looked.
    pub(crate) fn close_git_graph_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.graph_tab_open = false;
        self.leave_graph_tab(window, cx);
        self.graph_state.load = GraphLoadState::NotLoaded;
        self.graph_state.row_menu_open = None;
        self.graph_state.push_menu_open = false;
        self.graph_state.commit_files_cache = None;
        // A reopened tab is a genuinely new widget instance, so the Branches filter's undo
        // history must not be reachable from it - `reset`, not `clear` (itself a real, undoable
        // step) - the same reasoning `crate::root::AdeApp::open_palette` documents for its own
        // query field.
        self.graph_state.branches_filter.reset();
        cx.notify();
    }

    /// Common bookkeeping whenever the graph tab stops being the active centre-pane content -
    /// selecting an agent or file tab while it was showing, or closing it outright. A no-op if
    /// it wasn't active (e.g. closing it via its `×` while an agent tab is showing).
    ///
    /// `graph_focus_handle` is about to stop being `track_focus`'d (`Self::render_center_pane`
    /// only renders the graph view while `graph_tab_active` is `true`), so real keyboard focus is
    /// moved off it *first*, before anything else has a chance to capture it as its own
    /// `OverlayFocus` return target - and any target already holding it from earlier is swept.
    /// This mirrors `crate::sidebar::render::AdeApp::set_right_sidebar_view`'s identical
    /// `tree_focus_handle` sweep; see that function's docs for the exact "restore later lands on
    /// a handle nothing renders any more" bug class this closes. Called from
    /// `crate::root::state::AdeApp::select_worktree`, `crate::code_surface::tabs::AdeApp::
    /// open_and_focus_file` (the single real chokepoint every "open a file" entry point already
    /// goes through) and `crate::code_surface::tabs::AdeApp::activate_file_tab`, and
    /// [`Self::close_git_graph_tab`] above.
    pub(crate) fn leave_graph_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.graph_tab_active {
            return;
        }
        self.graph_tab_active = false;
        // Two handles stop being `track_focus`'d here, not one: `graph_focus_handle` itself, and
        // the Branches panel's real filter box (`graph_state.branches_filter_focus_handle`),
        // which is only rendered while this tab is active and can independently hold real
        // keyboard focus (its own `on_click` moves focus onto it). Missing this second handle was
        // a real, adversarial-audit-found gap: clicking the filter box then switching to an
        // agent tab left `Window::focus` on a handle no longer in the frame.
        if self.graph_focus_handle.is_focused(window)
            || self
                .graph_state
                .branches_filter_focus_handle
                .is_focused(window)
        {
            restore_focus(&self.agents, &mut self.graph_focus, window, cx);
        }
        self.palette_focus.forget_target(&self.graph_focus_handle);
        self.settings_focus.forget_target(&self.graph_focus_handle);
        self.code_focus.forget_target(&self.graph_focus_handle);
        self.palette_focus
            .forget_target(&self.graph_state.branches_filter_focus_handle);
        self.settings_focus
            .forget_target(&self.graph_state.branches_filter_focus_handle);
        self.code_focus
            .forget_target(&self.graph_state.branches_filter_focus_handle);
        // Real, reachable-without-Settings bug an adversarial audit of this exact change found:
        // `open_git_graph` only calls `load_graph` (which is what actually clears
        // `row_menu_open`/`push_menu_open`) while `GraphLoadState` is still `NotLoaded` - so
        // switching away from an already-loaded graph tab with a row menu open, then back (no
        // reload in between), left the stale menu's `graph_tab_active`-gated overlay
        // (`crate::root::AdeApp::render`) reappear the instant the tab became active again, with
        // no click at all. Dismissing both here, the same way `close_git_graph_tab` already does
        // for an outright close, closes the gap for the "switch tabs and back" path too.
        self.graph_state.row_menu_open = None;
        self.graph_state.push_menu_open = false;
    }

    /// Loads (or reloads) the graph and its upstream ahead/behind counts, off the UI thread -
    /// mirrors `crate::code_surface::tabs::AdeApp::load_diff`'s shape exactly.
    pub(crate) fn load_graph(&mut self, cx: &mut Context<Self>) {
        let root = self.diff_root.clone();
        let scope = self.graph_state.scope;
        self.graph_state.load = GraphLoadState::Loading;
        // A reload can renumber rows entirely (new commits, a scope change) - stale row-indexed
        // menu state pointing at whatever used to be at that index would be actively wrong, not
        // just outdated.
        self.graph_state.row_menu_open = None;
        self.graph_state.row_menu_bounds.clear();
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move {
                        let graph = wt_core::graph::build_graph(&root, scope, 0);
                        let upstream = wt_core::graph::ahead_behind_against_upstream(&root);
                        (graph, upstream)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (graph_result, upstream_result) = result;
                this.graph_state.upstream_counts = upstream_result.ok().flatten();
                this.graph_state.commit_files_cache = None;
                match graph_result {
                    Ok(graph) => {
                        let first_real_row = graph
                            .rows
                            .iter()
                            .find(|row| !row.commit.id.is_empty())
                            .cloned();
                        this.graph_state.selected_row =
                            if graph.rows.is_empty() { None } else { Some(0) };
                        this.graph_state.load = GraphLoadState::Loaded(graph);
                        if let Some(row) = first_real_row {
                            this.load_commit_files(row.commit.id, cx);
                        }
                    }
                    Err(err) => {
                        this.graph_state.load = GraphLoadState::Error(err.to_string());
                    }
                }
                cx.notify();
            });
        });
        self._load_graph_task = Some(task);
    }

    pub(crate) fn set_graph_scope(&mut self, scope: GraphScope, cx: &mut Context<Self>) {
        if self.graph_state.scope == scope {
            return;
        }
        self.graph_state.scope = scope;
        self.load_graph(cx);
    }

    pub(crate) fn select_graph_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.graph_state.selected_row = Some(index);
        self.graph_state.right_panel = GraphRightPanel::Commit;
        if let Some(row) = self.current_graph_row(index) {
            if !row.commit.id.is_empty() {
                self.load_commit_files(row.commit.id.clone(), cx);
            } else {
                self.graph_state.commit_files_cache = None;
            }
        }
        cx.notify();
    }

    /// Loads the Commit panel's real "Files changed" list for `sha`, off the UI thread -
    /// `wt_core::graph::commit_changed_files` performs real blocking I/O (spawns `git show`), so
    /// (unlike an earlier version of this feature, a real bug an adversarial audit caught) it
    /// must never be called inline from `render_graph_commit_panel`. A no-op if `sha` is already
    /// the cache's key (re-selecting the same row shouldn't re-spawn `git`).
    fn load_commit_files(&mut self, sha: String, cx: &mut Context<Self>) {
        if self
            .graph_state
            .commit_files_cache
            .as_ref()
            .is_some_and(|(cached_sha, _)| cached_sha == &sha)
        {
            return;
        }
        let root = self.diff_root.clone();
        let task = cx.spawn(async move |this, cx| {
            let sha_for_result = sha.clone();
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    let sha = sha.clone();
                    async move { wt_core::graph::commit_changed_files(&root, &sha) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.graph_state.commit_files_cache =
                    Some((sha_for_result, result.map_err(|err| err.to_string())));
                cx.notify();
            });
        });
        self._load_commit_files_task = Some(task);
    }

    pub(crate) fn set_graph_right_panel(&mut self, panel: GraphRightPanel, cx: &mut Context<Self>) {
        self.graph_state.right_panel = panel;
        cx.notify();
    }

    /// The `⋯` button's own click handler: toggles the menu for `index` closed if it was already
    /// open for that same row, otherwise opens it anchored off that row's own trigger bounds
    /// (`graph_state.row_menu_bounds`, captured by the button's own `gpui::canvas` child every
    /// render - the same mechanism `Self::render_graph_push_button` uses for its own popover).
    /// The popover's right edge aligns with the button's right edge (opening left-and-down from
    /// it) since the button sits at the row's own trailing edge, and a menu anchored to its left
    /// edge would run off the window - [`Self::open_graph_row_menu_at`] still clamps this back
    /// inside the window if the row itself is near an edge.
    pub(crate) fn toggle_graph_row_menu(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let already_open_here = matches!(
            self.graph_state.row_menu_open,
            Some(menu) if menu.row_index == index
        );
        if already_open_here {
            self.graph_state.row_menu_open = None;
            cx.notify();
            return;
        }
        let bounds = self
            .graph_state
            .row_menu_bounds
            .get(&index)
            .copied()
            .unwrap_or_default();
        let anchor_x = bounds.origin.x + bounds.size.width - theme::graph::ROW_MENU_WIDTH;
        let anchor_y = bounds.origin.y + bounds.size.height + px(2.0);
        self.open_graph_row_menu_at(index, anchor_x, anchor_y, window, cx);
    }

    /// Opens row `index`'s menu at `origin_x`/`origin_y` - either a right-click's own real
    /// `event.position` (GitHub issue #19's file-tree context menu -
    /// `crate::sidebar::tree_ops::AdeApp::open_tree_context_menu` - established this project's
    /// real right-click pattern; this mirrors it for a graph row), or [`Self::toggle_graph_row_menu`]'s
    /// button-anchored point above. Always (re)opens at the given position, even if a menu - for
    /// this row or another - was already open, so a second right-click never leaves a stale
    /// popover at the old position or two popovers open at once.
    ///
    /// Two things `open_tree_context_menu` also does that a first draft of this method missed
    /// (an adversarial audit of this exact change caught both):
    /// - **Clamped inside the window**, via the same `context_menu::clamp_menu_origin` the tree
    ///   menu uses, rather than painting off-screen for any row in the lower half of a
    ///   reasonably tall list - `theme::graph::ROW_MENU_WIDTH`/`ROW_MENU_HEIGHT` are this menu's
    ///   own real, fixed painted size (its content never varies).
    /// - **Explicitly focused**, via `window.focus`. The row's own right-click handler
    ///   (`Self::render_graph_row`) calls `cx.stop_propagation()` so a right-click doesn't also
    ///   select the row underneath it, but that same `stop_propagation` also preempts the graph
    ///   container's own auto-focus-on-mousedown listener that a left-click would otherwise get
    ///   for free - so without this explicit call, a right-click opened the menu but left
    ///   keyboard focus wherever it was before. Mirrors `open_tree_context_menu`'s own
    ///   `self.focus_file_tree(window, cx)` call, made for the identical reason.
    pub(crate) fn open_graph_row_menu_at(
        &mut self,
        index: usize,
        origin_x: gpui::Pixels,
        origin_y: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = window.bounds().size;
        let (clamped_x, clamped_y) = crate::sidebar::context_menu::clamp_menu_origin(
            f32::from(origin_x),
            f32::from(origin_y),
            f32::from(theme::graph::ROW_MENU_WIDTH),
            f32::from(theme::graph::ROW_MENU_HEIGHT),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        self.graph_state.row_menu_open = Some(GraphRowMenu {
            row_index: index,
            origin_x: px(clamped_x),
            origin_y: px(clamped_y),
        });
        // An adversarial audit's own finding: this menu and the Push `▾` menu
        // (`Self::toggle_graph_push_menu`) are independent booleans/options with no shared
        // "only one overlay open" invariant, so without this a right-click while the Push menu
        // was open left both painted at once (and this menu's own scrim, which *does*
        // `stop_propagation` on a left-click, then ate the next click meant to dismiss the Push
        // menu). Pre-existing gap, not introduced by this change - fixed here since it's the same
        // "which overlay owns the next click" property this change is already about.
        self.graph_state.push_menu_open = false;
        window.focus(&self.graph_focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn toggle_graph_push_menu(&mut self, cx: &mut Context<Self>) {
        self.graph_state.push_menu_open = !self.graph_state.push_menu_open;
        if self.graph_state.push_menu_open {
            self.graph_state.row_menu_open = None;
        }
        cx.notify();
    }

    /// Copies `text` to the real system clipboard - mirrors `crate::sidebar::tree_ops::AdeApp`'s
    /// own `cx.write_to_clipboard` use for "Copy path".
    pub(crate) fn copy_graph_text(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.graph_state.row_menu_open = None;
        cx.notify();
    }

    /// The honest not-yet-wired response for Fetch/Pull/Push (and their menu entries): real,
    /// visible feedback rather than a silent no-op or a fabricated success - see `super`'s module
    /// docs for why none of these are actually implemented in phase (a). Still does one real
    /// thing: reloads the graph, so a click at least confirms nothing crashed and the view is
    /// current.
    pub(crate) fn graph_action_not_yet_wired(&mut self, action: &str, cx: &mut Context<Self>) {
        self.graph_state.status_message = Some(format!("{action} - not implemented yet"));
        self.graph_state.push_menu_open = false;
        self.graph_state.row_menu_open = None;
        // The one real thing this honestly can do: reload, so a click at least confirms the view
        // is current rather than doing visibly nothing beyond the status line.
        self.load_graph(cx);
    }

    pub(in crate::graph_view) fn handle_branches_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        let changed = match keystroke.key.as_str() {
            "backspace" => self.graph_state.branches_filter.pop(Instant::now()),
            "escape" => self.graph_state.branches_filter.clear(Instant::now()),
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => self
                    .graph_state
                    .branches_filter
                    .push_str(text, Instant::now()),
                _ => false,
            },
        };
        if changed {
            cx.notify();
            cx.stop_propagation();
        }
    }

    pub(in crate::graph_view) fn handle_branches_filter_text_undo(
        &mut self,
        _: &crate::root::TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph_state.branches_filter.undo() {
            cx.notify();
        }
    }

    pub(in crate::graph_view) fn handle_branches_filter_text_redo(
        &mut self,
        _: &crate::root::TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph_state.branches_filter.redo() {
            cx.notify();
        }
    }
}

/// The tab strip's own "git graph" entry - a fourth, independent parallel slot alongside agent
/// tabs and file tabs, mirroring `crate::work_surface::render::render_tab_strip`'s existing
/// two-collection shape rather than unifying into one `Tab` enum (see that function's docs, and
/// this project's own note that a forced unification wasn't the right call here). Rendered only
/// while `AdeApp::graph_tab_open` is `true`.
pub(crate) fn render_graph_tab(app: &AdeApp, cx: &mut Context<AdeApp>) -> impl IntoElement {
    let is_active = app.graph_tab_active;
    let colors = work_surface::tab_colors(is_active);
    let close_color = if is_active {
        theme::text::DIMMER
    } else {
        theme::text::DISABLED
    };

    div()
        .id("graph-tab")
        .debug_selector(|| "graph-tab".to_string())
        .flex()
        .flex_none()
        .flex_col()
        .border_r_1()
        .border_color(theme::border::INNER)
        .bg(colors.bg)
        // Middle-click closes the graph tab too (GitHub issue #26) - the same real
        // `close_git_graph_tab` teardown the `×` button already uses, matching file/session tabs.
        .on_mouse_down(
            gpui::MouseButton::Middle,
            cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.close_git_graph_tab(window, cx);
            }),
        )
        .child(
            div()
                .id("graph-tab-hit")
                .flex_1()
                .flex()
                .items_center()
                .gap(px(7.0))
                .px(px(13.0))
                .cursor_pointer()
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.open_git_graph(window, cx);
                }))
                .child(render_graph_tab_chip())
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(app.ui_text_size(11.0))
                        .text_color(colors.label)
                        .child("Git graph"),
                )
                .child(
                    div()
                        .id("close-graph-tab")
                        .w(px(15.0))
                        .h(px(15.0))
                        .rounded(theme::radius::CHIP)
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|el| el.bg(theme::surface::TAB_CLOSE_HOVER))
                        .font(font(theme::font::MONO))
                        .text_size(px(11.0))
                        .text_color(close_color)
                        .child("\u{d7}")
                        .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_git_graph_tab(window, cx);
                        })),
                ),
        )
        .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline))
}

/// The tab's own fork-glyph chip: `#2a2030` bg, `#c98fbf` fork glyph drawn from four rects (two
/// 3px dots, a 1px riser, a 1px branch) - design spec §1: "no icon font".
pub(crate) fn render_graph_tab_chip() -> impl IntoElement {
    let dot = || {
        div()
            .absolute()
            .w(px(3.0))
            .h(px(3.0))
            .rounded(px(1.0))
            .bg(theme::graph::TAB_CHIP_FG)
    };
    div()
        .flex_none()
        .relative()
        .w(px(14.0))
        .h(px(14.0))
        .rounded(theme::radius::CHIP)
        .bg(theme::graph::TAB_CHIP_BG)
        .child(dot().top(px(2.0)).left(px(3.0)))
        .child(dot().bottom(px(2.0)).left(px(3.0)))
        .child(dot().bottom(px(2.0)).left(px(8.0)))
        // the riser (vertical) - 1px wide, spanning the two dots' vertical extent
        .child(
            div()
                .absolute()
                .w(px(1.0))
                .h(px(7.0))
                .top(px(3.5))
                .left(px(4.5))
                .bg(theme::graph::TAB_CHIP_FG),
        )
        // the branch (diagonal-ish stub, approximated as a short horizontal riser into the
        // third dot - GPUI has no line-drawing primitive, only rects)
        .child(
            div()
                .absolute()
                .w(px(4.0))
                .h(px(1.0))
                .bottom(px(3.5))
                .left(px(4.5))
                .bg(theme::graph::TAB_CHIP_FG),
        )
}

impl AdeApp {
    /// The git graph tab's full centre-pane content - toolbar, column header, then the row
    /// list. Called from
    /// `crate::work_surface::render::AdeApp::render_center_pane` whenever `graph_tab_active` is
    /// `true`, taking priority over a file tab or agent pane.
    pub(crate) fn render_graph_view(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let container = div()
            .id("graph-view")
            .track_focus(&self.graph_focus_handle)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(theme::surface::CENTER)
            .child(self.render_graph_toolbar(cx))
            .child(render_graph_header());

        let body = match &self.graph_state.load {
            GraphLoadState::NotLoaded | GraphLoadState::Loading => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .font(font(theme::font::SANS))
                .text_size(px(11.5))
                .text_color(theme::text::FAINT)
                .child("loading commit history\u{2026}")
                .into_any_element(),
            GraphLoadState::Error(message) => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .font(font(theme::font::SANS))
                .text_size(px(11.5))
                .text_color(theme::status::FAIL)
                .child(message.clone())
                .into_any_element(),
            GraphLoadState::Loaded(graph) => self.render_graph_rows(graph, cx),
        };

        let mut result = container.child(body);
        if let Some(message) = &self.graph_state.status_message {
            result = result.child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(theme::surface::FOOTER)
                    .border_t_1()
                    .border_color(theme::border::INNER)
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::DIM)
                    .child(message.clone()),
            );
        }
        // The Push `▾` menu and the row `⋯` menu are *not* rendered here, even though they're
        // conceptually part of this view: `gpui::canvas`-captured bounds are window-space, and
        // `.absolute()` positioning built from them is only correct if the popover is a direct
        // child of the window's own root element (the same reason `crate::root::AdeApp::render`
        // renders `render_plus_menu`/the tree context menu/the "New file" prompt as siblings of
        // the workspace body rather than nested inside it) - a real, adversarial-audit-found
        // fidelity bug: nested here, both popovers painted double-offset by this container's own
        // position (roughly the rail's width and the title bar's height), and their scrims only
        // covered this container instead of the whole window. `Self::render_graph_push_menu`/
        // `Self::render_graph_row_menu` are rendered from `AdeApp::render` instead - see there.
        result.into_any_element()
    }

    /// Toolbar (design spec §4): `HEAD` branch/chip/counts, the `All | Sessions | Current` scope
    /// segment, and the Fetch/Pull/Push button group. None of Fetch/Pull/Push perform a real git
    /// operation yet (see `super`'s module docs) - clicking any of them calls
    /// [`AdeApp::graph_action_not_yet_wired`], a real, honest, visible response.
    fn render_graph_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == self.diff_root)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| "(detached)".to_string());
        let counts = self.graph_state.upstream_counts;

        div()
            .id("graph-toolbar")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::graph::TOOLBAR)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(theme::text::HEADING)
                    .child(branch),
            )
            .child(
                div()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::graph::HEAD_CHIP_BG)
                    .font(font(theme::font::MONO))
                    .text_size(px(9.5))
                    .text_color(theme::graph::HEAD_CHIP_FG)
                    .child("HEAD"),
            )
            .when_some(counts, |el, counts| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::text::DIM)
                        .child(format!(
                            "\u{2191}{} \u{2193}{}",
                            counts.ahead, counts.behind
                        )),
                )
            })
            .child(div().flex_1())
            .child(self.render_graph_scope_segment(cx))
            .child(
                render_graph_toolbar_button("Fetch", false, false).on_click(cx.listener(
                    |this, _event: &ClickEvent, _window, cx| {
                        this.graph_action_not_yet_wired("Fetch", cx);
                    },
                )),
            )
            .child(
                render_graph_toolbar_button(
                    "Pull",
                    counts.map(|c| c.behind).unwrap_or(0) > 0,
                    false,
                )
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.graph_action_not_yet_wired("Pull", cx);
                })),
            )
            .child(self.render_graph_push_button(cx))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                // A click anywhere else in the toolbar (not on a specific button) just closes any
                // open popover, matching the tab strip `+` menu's own scrim behavior.
                if this.graph_state.push_menu_open {
                    this.graph_state.push_menu_open = false;
                    cx.notify();
                }
            }))
    }

    fn render_graph_scope_segment(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options = [
            widgets::ChoiceOption::new("All"),
            widgets::ChoiceOption::new("Sessions"),
            widgets::ChoiceOption::new("Current"),
        ];
        let selected = match self.graph_state.scope {
            GraphScope::All => "All",
            GraphScope::Sessions => "Sessions",
            GraphScope::Current => "Current",
        };
        self.render_choice_control(
            "graph-scope",
            &options,
            selected.to_string(),
            cx,
            |this, index, _window, cx| {
                let scope = match index {
                    0 => GraphScope::All,
                    1 => GraphScope::Sessions,
                    _ => GraphScope::Current,
                };
                this.set_graph_scope(scope, cx);
            },
        )
    }

    /// `Push ↑N ▾` - opens the Push menu (design spec §4: 268-wide, Push / Force with lease /
    /// Force). Every entry mutates the remote, so every entry is disabled - see `super`'s module
    /// docs.
    fn render_graph_push_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // `None` (no configured upstream, or still loading) renders as a bare "Push", never a
        // fabricated "↑0" - matching `wt_core::graph::ahead_behind_against_upstream`'s own "no
        // entry rather than a fabricated value" contract.
        let label = match self.graph_state.upstream_counts {
            Some(counts) => format!("Push \u{2191}{} \u{25be}", counts.ahead),
            None => "Push \u{25be}".to_string(),
        };
        let this = cx.entity();
        div()
            .id("graph-push-button")
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(9.0))
            .h(px(24.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            .border_color(theme::button::BLUE_KEYCAP)
            .cursor_pointer()
            .hover(|el| el.bg(theme::button::BLUE_BG_HOVER))
            .font(font(theme::font::MONO))
            .text_size(px(10.5))
            .text_color(theme::button::BLUE_FG)
            .child(label)
            .child({
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.graph_state.push_button_bounds = bounds;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                let _ = event;
                this.toggle_graph_push_menu(cx);
            }))
    }

    pub(crate) fn render_graph_push_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.graph_state.push_button_bounds;
        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::PLUS_MENU;
        div()
            .id("graph-push-menu-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.graph_state.push_menu_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("graph-push-menu-popover")
                    .absolute()
                    .left(bounds.origin.x)
                    .top(bounds.origin.y + bounds.size.height + px(2.0))
                    .w(theme::graph::PUSH_MENU_WIDTH)
                    .py(px(4.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .shadow(vec![BoxShadow::new(
                        shadow_x,
                        shadow_y,
                        gpui::black().opacity(0.55),
                    )
                    .blur_radius(shadow_blur)])
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .child(render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::BLUE_FG.into(),
                        theme::button::BLUE_BG.into(),
                        "Push",
                        "not implemented yet".to_string(),
                        Vec::new(),
                        false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::DANGER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Force with lease",
                        "aborts if the remote moved - not implemented yet".to_string(),
                        Vec::new(),
                        false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::DANGER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Force",
                        "not implemented yet".to_string(),
                        Vec::new(),
                        false,
                    )),
            )
    }

    /// The row list - not virtualized (a reasonable, honest simplification for phase (a) given
    /// `wt_core::graph::DEFAULT_MAX_COMMITS` already caps loaded data at 500 rows; a
    /// `uniform_list` upgrade is real future work if that turns out to matter for perf, not a
    /// correctness gap).
    fn render_graph_rows(&self, graph: &Graph, cx: &mut Context<Self>) -> gpui::AnyElement {
        if graph.rows.is_empty() {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .font(font(theme::font::SANS))
                .text_size(px(11.5))
                .text_color(theme::text::FAINT)
                .child("no commits reachable from this scope")
                .into_any_element();
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut list = div()
            .id("graph-rows")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (index, row) in graph.rows.iter().enumerate() {
            list = list.child(self.render_graph_row(index, row, graph.lane_count, now, cx));
        }
        if graph.truncated {
            list = list.child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(6.0))
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(format!(
                        "showing the first {} commits",
                        wt_core::graph::DEFAULT_MAX_COMMITS
                    )),
            );
        }
        list.into_any_element()
    }

    /// One row: lane canvas 100 · ref chips · subject (flex) · author 88 · relative time 40
    /// right · sha 62 right · `⋯` 22 (`revision 3/REVISION-2026-07-31.md` §6.2 - supersedes the
    /// revision-2 entry's own column list, which also had a `note` column and a per-commit
    /// session column between subject and author; both are gone. A commit belongs to a
    /// worktree, which can hold several agents, so pinning one agent's live status to a past
    /// commit was exactly the imprecision that revision set out to remove; the `note` column
    /// next to it was never in either revision's own column list and had no real data behind
    /// it either - see the removed `render_graph_session_column`'s own former doc comment, git
    /// history has it, for what it used to render).
    fn render_graph_row(
        &self,
        index: usize,
        row: &GraphRow,
        lane_count: usize,
        now_unix: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.graph_state.selected_row == Some(index);
        let is_working_tree = row.commit.id.is_empty();
        let relative = if row.commit.id.is_empty() {
            "now".to_string()
        } else {
            relative_time(row.commit.committer_time_unix, now_unix)
        };

        div()
            .id(("graph-row", index))
            .debug_selector(move || format!("graph-row-{index}"))
            .flex()
            .items_center()
            .w_full()
            .h(theme::graph::ROW)
            .border_b_1()
            .border_color(theme::border::ROW)
            .cursor_pointer()
            // `border_l_2()` is applied unconditionally, reserving the 2px of space whether or
            // not this row is selected, so selecting a row never shifts its content (lane
            // canvas, ref chips, subject, everything) 2px to the right - only the border's
            // *colour* toggles. Mirrors `crate::work_surface::state`'s own `TRANSPARENT`
            // convention (see its doc comment: "so every button/tab can always call
            // `.bg()`/`.border_color()` uniformly rather than conditionally skipping the call,
            // which would also shift the box model by the border's width").
            .border_l_2()
            .border_color(if selected {
                theme::border::SELECTED_EDGE.into()
            } else {
                work_surface::TRANSPARENT
            })
            .when(selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            // The row's own right-click (mirrors the file tree's real right-click pattern,
            // GitHub issue #19 §1 - `crate::sidebar::render::AdeApp::render_file_tree_row`).
            // `cx.stop_propagation()` keeps it from also reaching any ancestor click handler.
            // Gated the same way the `⋯` button itself is: the honestly-empty working-tree row
            // has no context menu at all, so a right-click on it is a no-op.
            .when(!is_working_tree, |el| {
                el.on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_graph_row_menu_at(
                            index,
                            event.position.x,
                            event.position.y,
                            window,
                            cx,
                        );
                    }),
                )
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_graph_row(index, cx);
            }))
            .child(render_graph_lane_canvas(index, row, lane_count))
            .child(render_graph_ref_chips(row))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(6.0))
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .text_color(theme::text::BODY)
                    .child(row.commit.subject.clone()),
            )
            .child(
                div()
                    .debug_selector(move || format!("graph-row-{index}-author"))
                    .w(px(88.0))
                    .px(px(4.0))
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::text::DIM)
                    .child(row.commit.author_name.clone()),
            )
            .child(
                div()
                    .debug_selector(move || format!("graph-row-{index}-age"))
                    .w(px(40.0))
                    .text_right()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::FAINT)
                    .child(relative),
            )
            .child(
                div()
                    .debug_selector(move || format!("graph-row-{index}-sha"))
                    .w(px(62.0))
                    .text_right()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(row.commit.short_id.clone()),
            )
            .child(self.render_graph_row_menu_button(index, row, cx))
    }

    fn render_graph_row_menu_button(
        &self,
        index: usize,
        row: &GraphRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity();
        let is_working_tree = row.commit.id.is_empty();
        div()
            .id(("graph-row-menu-button", index))
            .debug_selector(move || format!("graph-row-menu-button-{index}"))
            .relative()
            .w(px(22.0))
            .h(px(22.0))
            .rounded(theme::radius::CHIP)
            .flex()
            .items_center()
            .justify_center()
            .when(!is_working_tree, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(12.0))
                            .text_color(theme::text::DIM)
                            .child("\u{22ef}"),
                    )
                    .child({
                        gpui::canvas(
                            move |bounds, _window, cx| {
                                this.update(cx, |this, _cx| {
                                    this.graph_state.row_menu_bounds.insert(index, bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    })
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.toggle_graph_row_menu(index, window, cx);
                    }))
            })
    }

    /// The row `⋯` context menu (design spec §4): grouped Branch / Apply / Reset / Copy. Every
    /// entry that would perform a real git mutation is disabled - only Copy's entries are wired.
    /// Anchored to [`GraphRowMenu::origin_x`]/`origin_y` - the real position it was opened at
    /// (either a right-click's own `event.position`, or the `⋯` button's own captured bounds via
    /// `AdeApp::graph_state.row_menu_bounds`, the same `gpui::canvas`-bounds-capture mechanism
    /// `crate::work_surface::render::AdeApp::render_plus_menu` uses), resolved once at open time
    /// in `AdeApp::open_graph_row_menu_at`/`toggle_graph_row_menu` - never recomputed here from
    /// the row's index, so a scrolled row's menu is never mispositioned.
    pub(crate) fn render_graph_row_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.graph_state.row_menu_open else {
            return gpui::Empty.into_any_element();
        };
        let index = menu.row_index;
        let Some(row) = self.current_graph_row(index) else {
            return gpui::Empty.into_any_element();
        };
        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::PLUS_MENU;
        let sha = row.commit.id.clone();
        let short_sha = row.commit.short_id.clone();
        let subject = row.commit.subject.clone();

        div()
            .id("graph-row-menu-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            // `cx.stop_propagation()` here (an adversarial audit's own finding) is what stops a
            // click on the *same* row's now-open `⋯` button from re-opening the menu it just
            // dismissed: this scrim paints on top of that button (it is a later, sibling child of
            // the same root - `AdeApp::render`'s own docs above `Self::render_graph_row_menu`),
            // so without this a single click ran *both* this dismiss and the button's own
            // re-open, in that order, for the same `MouseUpEvent`.
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.graph_state.row_menu_open = None;
                cx.notify();
            }))
            // A right-click elsewhere must dismiss too (mirrors `crate::sidebar::render::AdeApp::
            // render_tree_context_menu`'s own scrim - "otherwise the next right-click anywhere
            // would land on the scrim and do nothing at all"). Deliberately does *not*
            // `cx.stop_propagation()`, unlike the left-click dismiss above: a right-click that
            // lands on a *different* row must still reach that row's own `on_mouse_down(Right, ..)`
            // handler afterwards so it opens fresh there in the same click, rather than requiring
            // a second right-click the way the tree menu does - `Self::open_graph_row_menu_at`
            // unconditionally overwrites `row_menu_open`, so this dismiss is a harmless no-op
            // whenever a row's own handler goes on to run right after it.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _event: &gpui::MouseDownEvent, _window, cx| {
                    this.graph_state.row_menu_open = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("graph-row-menu-popover")
                    .debug_selector(|| "graph-row-menu-popover".to_string())
                    .absolute()
                    .left(menu.origin_x)
                    .top(menu.origin_y)
                    .w(theme::graph::ROW_MENU_WIDTH)
                    .py(px(4.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .shadow(vec![BoxShadow::new(shadow_x, shadow_y, gpui::black().opacity(0.55))
                        .blur_radius(shadow_blur)])
                    // Occludes so a right-click *inside* the popover's own bounds can never fall
                    // through to whatever row it happens to be painted on top of (a real,
                    // adversarial-audit-found bug: the popover opens *over* the row list, and
                    // without this a right-click on the panel itself retargeted the menu to
                    // whichever row was underneath it). Scoped to the panel alone, not the whole
                    // scrim above - the panel is a small, content-sized rectangle that never
                    // reaches the title bar, so (unlike `render_tree_context_menu`'s own
                    // full-window occluding scrim, which had to start below the title bar for
                    // exactly this reason - see that method's docs) no caption-button interaction
                    // is possible here.
                    .occlude()
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .child(render_graph_row_menu_header("Branch"))
                    .child(render_dropdown_menu_row(
                        "\u{2713}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Check out", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "+", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Create branch here", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{25b8}", theme::button::BLUE_FG.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Start session from this commit", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_graph_row_menu_header("Apply"))
                    .child(render_dropdown_menu_row(
                        "\u{2398}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Cherry-pick", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{21b6}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Revert", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{2191}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Rebase onto this commit", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{2191}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Interactive rebase from here", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_graph_row_menu_header("Reset"))
                    .child(render_dropdown_menu_row(
                        "\u{21ba}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Soft", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{21ba}", theme::text::GHOST.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Mixed", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_dropdown_menu_row(
                        "\u{21ba}", theme::button::DANGER_FG.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Hard", "not implemented yet".to_string(), Vec::new(), false,
                    ))
                    .child(render_graph_row_menu_header("Copy"))
                    .child(render_dropdown_menu_row(
                        "#", theme::text::SECONDARY.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Copy SHA", short_sha, Vec::new(), true,
                    ).on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.copy_graph_text(sha.clone(), cx);
                    })))
                    .child(render_dropdown_menu_row(
                        "\u{ab}", theme::text::SECONDARY.into(), theme::surface::CHIP_NEUTRAL.into(),
                        "Copy subject", String::new(), Vec::new(), true,
                    ).on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.copy_graph_text(subject.clone(), cx);
                    })))
                    .child(
                        div()
                            .px(px(11.0))
                            .pt(px(4.0))
                            .font(font(theme::font::SANS))
                            .text_size(px(9.5))
                            .text_color(theme::text::GHOSTER)
                            .child("rebase and reset run in the focused worktree, never the main checkout"),
                    ),
            )
            .into_any_element()
    }

    fn current_graph_row(&self, index: usize) -> Option<&GraphRow> {
        match &self.graph_state.load {
            GraphLoadState::Loaded(graph) => graph.rows.get(index),
            _ => None,
        }
    }

    /// The right panel while the graph tab is focused - replaces Files/Changes with Commit/
    /// Branches (design spec §5). Called from `crate::sidebar::render::AdeApp::
    /// render_right_sidebar` whenever `graph_tab_active` is `true`.
    pub(crate) fn render_graph_right_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let options = [
            widgets::ChoiceOption::new("Commit"),
            widgets::ChoiceOption::new("Branches"),
        ];
        let selected = match self.graph_state.right_panel {
            GraphRightPanel::Commit => "Commit",
            GraphRightPanel::Branches => "Branches",
        };
        let toggle = div()
            .flex_none()
            .px(px(10.0))
            .py(px(8.0))
            .child(self.render_choice_control(
                "graph-right-panel",
                &options,
                selected.to_string(),
                cx,
                |this, index, _window, cx| {
                    let panel = if index == 0 {
                        GraphRightPanel::Commit
                    } else {
                        GraphRightPanel::Branches
                    };
                    this.set_graph_right_panel(panel, cx);
                },
            ));

        let body = match self.graph_state.right_panel {
            GraphRightPanel::Commit => self.render_graph_commit_panel(),
            GraphRightPanel::Branches => self.render_graph_branches_panel(cx),
        };

        div()
            .id("graph-right-panel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(toggle)
            .child(body)
            .into_any_element()
    }

    fn render_graph_commit_panel(&self) -> gpui::AnyElement {
        let Some(index) = self.graph_state.selected_row else {
            return render_sidebar_message(
                "select a commit to see its details".to_string(),
                theme::text::FAINT.into(),
            )
            .into_any_element();
        };
        let Some(row) = self.current_graph_row(index) else {
            return render_sidebar_message(
                "select a commit to see its details".to_string(),
                theme::text::FAINT.into(),
            )
            .into_any_element();
        };
        if row.commit.id.is_empty() {
            return render_sidebar_message(
                "uncommitted changes - see the Changes list".to_string(),
                theme::text::FAINT.into(),
            )
            .into_any_element();
        }

        // Real background-loaded data (`Self::load_commit_files`), never a blocking `git show`
        // spawned here in the render path - see that method's own docs for the real bug this
        // replaced. Three honest states: not yet requested/still loading (a real, un-fabricated
        // "loading" line), a real error, or the real file list.
        let files_section: gpui::AnyElement = match &self.graph_state.commit_files_cache {
            Some((sha, Ok(files))) if sha == &row.commit.id => div()
                .children(files.iter().cloned().map(render_graph_file_row))
                .into_any_element(),
            Some((sha, Err(message))) if sha == &row.commit.id => div()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::status::FAIL)
                .child(message.clone())
                .into_any_element(),
            _ => div()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::GHOST)
                .child("loading\u{2026}")
                .into_any_element(),
        };

        div()
            .id("graph-commit-panel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(12.0))
            .py(px(10.0))
            .gap(px(8.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(12.5))
                    .text_color(theme::text::HEADING)
                    .child(row.commit.subject.clone()),
            )
            .when(!row.commit.body.is_empty(), |el| {
                el.child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(px(10.5))
                        .text_color(theme::text::DIM)
                        .child(row.commit.body.clone()),
                )
            })
            .child(render_graph_meta_row(
                "author",
                row.commit.author_name.clone(),
            ))
            .child(render_graph_meta_row(
                "when",
                relative_time(row.commit.committer_time_unix, unix_now()),
            ))
            .child(render_graph_meta_row("sha", row.commit.id.clone()))
            .child(render_graph_meta_row(
                "parent",
                row.commit.parent_ids.join(", "),
            ))
            .child(
                div()
                    .pt(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.5))
                    .text_color(theme::text::SECONDARY)
                    .child("Files changed"),
            )
            .child(files_section)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .pt(px(8.0))
                    .child(render_graph_disabled_footer_button("Cherry-pick"))
                    .child(render_graph_disabled_footer_button("Revert")),
            )
            .into_any_element()
    }

    fn render_graph_branches_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(graph) = self.current_graph() else {
            return render_sidebar_message(
                "open the graph to see branches".to_string(),
                theme::text::FAINT.into(),
            )
            .into_any_element();
        };
        let query = self.graph_state.branches_filter.as_str();
        let mut branches: Vec<(String, RefKind, bool, usize)> = Vec::new();
        for row in &graph.rows {
            for chip in &row.commit.refs {
                if matches!(chip.kind, RefKind::LocalBranch) {
                    branches.push((chip.name.clone(), chip.kind, chip.is_head, row.lane));
                }
            }
        }
        branches.retain(|(name, ..)| {
            query.is_empty() || name.to_lowercase().contains(&query.to_lowercase())
        });
        branches.sort_by(|a, b| a.0.cmp(&b.0));

        div()
            .id("graph-branches-panel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_graph_branches_filter_row(branches.len(), cx))
            .child(
                div()
                    .id("graph-branches-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(branches.into_iter().map(|(name, kind, is_head, lane)| {
                        render_graph_branch_row(name, kind, is_head, lane)
                    })),
            )
            .into_any_element()
    }

    fn render_graph_branches_filter_row(
        &self,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_query = !self.graph_state.branches_filter.is_empty();
        div()
            .id("graph-branches-filter")
            .track_focus(&self.graph_state.branches_filter_focus_handle)
            .key_context("text-input")
            .on_action(cx.listener(Self::handle_branches_filter_text_undo))
            .on_action(cx.listener(Self::handle_branches_filter_text_redo))
            .on_key_down(cx.listener(Self::handle_branches_filter_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.graph_state.branches_filter_focus_handle, cx);
            }))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .h(theme::graph::BRANCHES_FILTER_ROW)
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child("/"),
            )
            .child(
                div()
                    .flex_1()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(if has_query {
                        theme::text::DIM
                    } else {
                        theme::text::GHOST
                    })
                    .child(if has_query {
                        self.graph_state.branches_filter.as_str().to_string()
                    } else {
                        "filter branches".to_string()
                    }),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(format!("{count}")),
            )
    }

    fn current_graph(&self) -> Option<&Graph> {
        match &self.graph_state.load {
            GraphLoadState::Loaded(graph) => Some(graph),
            _ => None,
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn render_graph_meta_row(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .gap(px(8.0))
        .font(font(theme::font::MONO))
        .text_size(px(10.0))
        .child(
            div()
                .w(px(48.0))
                .text_color(theme::text::GHOST)
                .child(label),
        )
        .child(div().flex_1().text_color(theme::text::DIM).child(value))
}

fn render_graph_disabled_footer_button(label: &'static str) -> impl IntoElement {
    div()
        .px(px(10.0))
        .py(px(5.0))
        .rounded(theme::radius::BUTTON)
        .border_1()
        .border_color(theme::border::BUTTON_DISABLED)
        .font(font(theme::font::SANS))
        .text_size(px(10.5))
        .text_color(theme::text::DISABLED)
        .child(label)
}

/// One "Files changed" row - the change-row visual's spirit (path + status pill), simplified
/// since a historical commit has no review checkbox or per-file stat counts to show.
fn render_graph_file_row(file: wt_core::graph::CommitFileChange) -> impl IntoElement {
    let (dir, name) = changes::split_dir_name(&file.path);
    let tag = changes::change_tag(file.status);
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(theme::band::CHANGE_ROW)
        .when(!dir.is_empty(), |el| {
            el.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOSTER)
                    .child(format!("{dir}/")),
            )
        })
        .child(
            div()
                .font(font(theme::font::MONO))
                .text_size(px(11.5))
                .text_color(theme::text::STRONG)
                .child(name),
        )
        .when_some(tag, |el, tag| el.child(render_tag_pill(tag)))
}

fn render_graph_branch_row(
    name: String,
    kind: RefKind,
    is_head: bool,
    lane: usize,
) -> impl IntoElement {
    let dot_color: gpui::Rgba = if matches!(kind, RefKind::LocalBranch) {
        lane_color(lane)
    } else {
        theme::graph::BRANCH_NO_LANE_DOT.into()
    };
    div()
        .id(format!("graph-branch-row-{name}"))
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(theme::graph::BRANCH_ROW)
        .px(px(10.0))
        .border_b_1()
        .border_color(theme::border::ROW)
        .hover(|el| el.bg(theme::surface::ROW_HOVER))
        .child(div().w(px(6.0)).h(px(6.0)).rounded(px(3.0)).bg(dot_color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font(font(theme::font::MONO))
                .text_size(px(11.0))
                .text_color(theme::text::BODY)
                .child(name),
        )
        .when(is_head, |el| {
            el.child(
                div()
                    .px(px(5.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::graph::HEAD_CHIP_BG)
                    .font(font(theme::font::MONO))
                    .text_size(px(9.0))
                    .text_color(theme::graph::HEAD_CHIP_FG)
                    .child("HEAD"),
            )
        })
}

fn render_graph_row_menu_header(label: &'static str) -> impl IntoElement {
    div()
        .px(px(11.0))
        .pt(px(6.0))
        .pb(px(2.0))
        .font(font(theme::font::MONO))
        .text_size(px(9.0))
        .text_color(theme::text::GHOSTER)
        .child(label)
}

fn render_graph_toolbar_button(
    label: &'static str,
    has_activity: bool,
    _reserved: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(format!("graph-toolbar-button-{label}"))
        .px(px(9.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .rounded(theme::radius::BUTTON)
        .cursor_pointer()
        .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
        .font(font(theme::font::MONO))
        .text_size(px(10.5))
        .text_color(if has_activity {
            theme::text::PRIMARY
        } else {
            theme::text::DIM
        })
        .child(label)
}

/// Which edge of a curve box carries the horizontal border stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HorizontalEdge {
    Top,
    Bottom,
}

/// Which edge of a curve box carries the vertical border stroke (and, paired with
/// [`HorizontalEdge`], which corner gets the radius).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerticalEdge {
    Left,
    Right,
}

/// How one curve's vertical stroke relates to the lane it is anchored to. The two cases need
/// *different* positions relative to the same `lane_x`, which is why a single uniform shift for
/// the whole assembly (what this module used to do) can never be right for both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneJoin {
    /// This curve's vertical stroke **is** that lane's own line for this half-row - the plain
    /// `LaneSegment` stub is deliberately skipped for exactly this lane (see
    /// `render_graph_lane_canvas`), so the stroke has to land on the lane's own column.
    ContinuesLane,
    /// This curve departs from (or arrives at) this row's own dot while `own_lane`'s own line runs
    /// straight through the very same rows. Painted one stroke to the side the elbow travels, so
    /// the two 1px lines sit side by side instead of one silently erasing the other.
    LeavesDot,
}

/// One quarter-circle curve piece, as a real paintable `div()`: where it sits, how tall it is, and
/// which two edges carry the border (and so which corner gets the radius).
///
/// Two GPUI behaviors drive every non-obvious number in here, and both are ordinary and fully
/// deterministic - neither is anti-aliasing.
///
/// **1. A corner radius is clamped to half the box's shorter side.** `Style::paint`
/// (`vendor/zed/crates/gpui/src/style.rs`) always calls `Corners::clamp_radii_for_quad_size`. So
/// `theme::graph::ELBOW_CURVE_SIZE` is deliberately `2 * ELBOW_RADIUS`: the clamp's own threshold
/// then exactly equals the requested radius and never reduces it. A corollary that shapes the rest
/// of this module: the arc only ever occupies an `ELBOW_RADIUS` square in the box's own corner, so
/// each bordered edge always carries exactly `ELBOW_RADIUS` of *straight* lead-in beyond it.
///
/// **2. A border is painted *inside* the box's own bounds**, exactly like CSS
/// `box-sizing: border-box`. The quad shader (`fs_quad` in `crates/gpui_wgpu/src/shaders.wgsl`)
/// tests each pixel against `corner_to_point + border_widths`, measured *inward* from the bounds
/// edge. For the 1px borders painted here that means:
///
/// * `border_t`/`border_l` on an edge at `v` paint the pixel row/column `[v, v + 1)`;
/// * `border_b`/`border_r` on an edge at `v` paint the pixel row/column `[v - 1, v)`.
///
/// So two boxes whose *edges* meet at a shared coordinate paint their strokes on two **different,
/// adjacent** pixels. Anchoring therefore has to be stated in terms of the *painted* stroke, never
/// the box edge - which is what [`CurveBox::anchored`] does on both axes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CurveBox {
    left: Pixels,
    top: Pixels,
    /// Not always `ELBOW_CURVE_SIZE`: a `HorizontalEdge::Bottom` curve is exactly one
    /// [`ELBOW_STROKE`] taller, so that its inside-painted bottom border lands *on* the waist row
    /// instead of one row above it.
    ///
    /// Growing the height (rather than moving the box) is what makes that correction safe: GPUI
    /// anchors the arc at the box's own corner, so on a `rounded_bl`/`rounded_br` box the extra
    /// stroke slides the arc down by exactly one pixel and lengthens the straight vertical lead-in
    /// above it by the same amount - leaving `top`, the end that has to meet the row's dot or the
    /// neighbouring row's own stub, exactly where it was.
    height: Pixels,
    horizontal: HorizontalEdge,
    vertical: VerticalEdge,
}

impl CurveBox {
    /// Positions one curve so that **both** its painted strokes land where they have to: its
    /// horizontal stroke on the `waist_y` row, and its vertical stroke on (or, for
    /// [`LaneJoin::LeavesDot`], one stroke beside) the lane vertical at `lane_x`.
    ///
    /// Every offset here is a direct consequence of GPUI painting borders inside the box - see the
    /// type's own docs. There is deliberately no uniform "nudge" applied afterwards: `border_l` and
    /// `border_r` need corrections of *different* signs, so no single shift can serve both.
    fn anchored(
        lane_x: Pixels,
        waist_y: Pixels,
        horizontal: HorizontalEdge,
        vertical: VerticalEdge,
        join: LaneJoin,
    ) -> Self {
        let size = theme::graph::ELBOW_CURVE_SIZE;
        // A `Left` box extends right of its own stroke, a `Right` box extends left of it; for
        // `LeavesDot` that is also the direction the elbow travels, so stepping one stroke that way
        // is what puts this curve *beside* `own_lane`'s own through-line rather than on top of it.
        let left = match (vertical, join) {
            (VerticalEdge::Left, LaneJoin::ContinuesLane) => lane_x,
            (VerticalEdge::Left, LaneJoin::LeavesDot) => lane_x + ELBOW_STROKE,
            (VerticalEdge::Right, LaneJoin::ContinuesLane) => lane_x + ELBOW_STROKE - size,
            (VerticalEdge::Right, LaneJoin::LeavesDot) => lane_x - size,
        };
        // A `Top` box needs no correction - its top border already paints the `waist_y` row, the
        // same row the 1px-tall filled bridge occupies. A `Bottom` box's bottom border paints one
        // row *above* its own bottom edge, so that edge has to sit one stroke past the waist.
        let (top, height) = match horizontal {
            HorizontalEdge::Top => (waist_y, size),
            HorizontalEdge::Bottom => (waist_y - size, size + ELBOW_STROKE),
        };
        Self {
            left,
            top,
            height,
            horizontal,
            vertical,
        }
    }

    /// This box's own right edge. The *painted* vertical stroke is one [`ELBOW_STROKE`] inside
    /// this for a [`VerticalEdge::Right`] box - see the type's own docs.
    fn right(&self) -> Pixels {
        self.left + theme::graph::ELBOW_CURVE_SIZE
    }
}

/// The width of every stroke in the lane canvas: the plain lane segments' `w(px(1.0))`, the
/// straight bridge's `h(px(1.0))`, and each curve box's `border_*_1()`. Named rather than left as a
/// bare `px(1.0)` because [`CurveBox::anchored`]'s corrections are *exactly* one stroke width and
/// only make sense in those terms. If the graph ever moves to thicker lines this must change with
/// `border_*_1()` together, or every seam it closes reopens by `stroke - 1` px.
const ELBOW_STROKE: Pixels = px(1.0);

/// A plain 1px-tall filled segment bridging the entry and exit curves.
///
/// Always present, even between adjacent lanes where the two curves' own boxes already overlap, and
/// always overlapping `ELBOW_RADIUS` into each - far enough to cover each curve's whole straight
/// border lead-in (see [`CurveBox`] for why that run is always exactly `ELBOW_RADIUS` long). A real
/// user report found a hairline gap at this seam: a border-radius arc and a filled rect are two
/// different rendering paths, and GPUI (like CSS) does not guarantee their anti-aliased edges land
/// on the same physical pixel even when the coordinates agree exactly. Covering the straight run
/// with the fill takes the border's own rendering out of that stretch entirely.
///
/// `top` sits at the plain, unadjusted `waist_y`: a 1px-tall filled rect there occupies the row
/// `[waist_y, waist_y + 1)`, which is exactly the row a `border_t` on a box starting at `waist_y`
/// paints. It is the `border_b` curve that needs correcting, and it is corrected via its own height
/// rather than by moving this bridge - see [`CurveBox::anchored`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct StraightSegment {
    left: Pixels,
    top: Pixels,
    width: Pixels,
}

/// One elbow's S-curve geometry: two quarter-circle corners joined by a straight middle segment.
/// Pure and GPUI-element-free, so it is testable with plain `Pixels` values.
///
/// A real user report against an earlier single-corner design ("curve the start and end of branch
/// lines to make them join the horizontal lines instead of continuing... the end of the lines need
/// to have corners too so they rejoin after merge") asked for exactly this shape: both ends of the
/// connector get their own curve, not one rounded corner and one flat dead-end.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ElbowGeometry {
    entry: CurveBox,
    straight: StraightSegment,
    exit: CurveBox,
}

/// Computes one elbow's S-curve geometry. Pure and GPUI-element-free, so it is testable with plain
/// `Pixels` values. `x_from`/`x_to` are `lane_x(elbow.from_lane)`/`lane_x(elbow.to_lane)`.
///
/// Which end sits on this row's dot differs by kind, and that is what decides each curve's
/// [`LaneJoin`]. `Diverging`'s `from_lane` always *is* `own_lane` (`wt_core::graph::layout_lanes`
/// Step 4 sets it so), so its entry curve leaves this row's dot and its exit curve delivers the
/// line to `to_lane`'s own vertical. `Converging` is the mirror: Step 2 sets `from_lane` to the
/// *ending* lane, so its entry curve continues that lane's already-painted line from the row above
/// and its exit curve lands on `own_lane`'s dot.
///
/// The two kinds occupy opposite row halves (design spec §2 puts a real merge's elbow "in the lower
/// half"; `Converging` is the same shape mirrored into the upper half), which is the only thing
/// that tells them apart geometrically - both use the same entry/exit border-edge pattern.
fn elbow_geometry(kind: ElbowKind, x_from: Pixels, x_to: Pixels, row_h: Pixels) -> ElbowGeometry {
    let curve_size = theme::graph::ELBOW_CURVE_SIZE;
    let radius = theme::graph::ELBOW_RADIUS;
    let rightward = x_to >= x_from;

    // The single pixel row all three pieces paint their horizontal stroke on: one curve away from
    // the dot, below it for `Diverging` and above it for `Converging` - but never so far that the
    // far curve's own *arc* would fall outside this row's own box.
    //
    // That clamp is what makes `render_graph_lane_canvas`'s clip safe. The far curve is always
    // `ELBOW_CURVE_SIZE` tall while only `ELBOW_RADIUS` of that is arc (see [`CurveBox`]), so the
    // stretch it reaches past the row edge is pure *straight vertical stroke*, sitting on exactly
    // the column the neighbouring row's own `LaneSegment` continues the line on. Clipping it away
    // therefore loses nothing: the neighbour redraws the identical pixels. Let the waist sit one
    // full curve from the dot instead (`ROW / 2 + CURVE_SIZE` = 23 against `ROW` = 26) and the clip
    // would cut the arc mid-sweep, 2px before it has finished turning onto the lane's own column -
    // a visible kink at every row boundary.
    let waist_y = match kind {
        ElbowKind::Diverging => (row_h / 2.0 + curve_size).min(row_h - radius),
        ElbowKind::Converging => (row_h / 2.0 - curve_size).max(radius),
    };
    let (entry_join, exit_join) = match kind {
        ElbowKind::Diverging => (LaneJoin::LeavesDot, LaneJoin::ContinuesLane),
        ElbowKind::Converging => (LaneJoin::ContinuesLane, LaneJoin::LeavesDot),
    };
    // Each curve's vertical stroke faces the *other* lane: the entry turns its vertical into the
    // horizontal, the exit turns the horizontal back into a vertical.
    let (entry_vertical, exit_vertical) = if rightward {
        (VerticalEdge::Left, VerticalEdge::Right)
    } else {
        (VerticalEdge::Right, VerticalEdge::Left)
    };

    let entry = CurveBox::anchored(
        x_from,
        waist_y,
        HorizontalEdge::Bottom,
        entry_vertical,
        entry_join,
    );
    let exit = CurveBox::anchored(x_to, waist_y, HorizontalEdge::Top, exit_vertical, exit_join);

    // The bridge runs from where the left curve's own arc *ends* to where the right curve's own
    // arc *begins* - `ELBOW_RADIUS` inside each box, since the arc only ever fills an
    // `ELBOW_RADIUS` corner square (see [`CurveBox`]). That covers each curve's whole straight
    // border lead-in and nothing else (see [`StraightSegment`] for why the lead-ins must be
    // covered).
    //
    // Both ends have to be clamped that way, not just the near one. Adjacent lanes are only
    // `LANE_STEP` = 14px apart while the two boxes together want `2 * ELBOW_CURVE_SIZE` = 20px, so
    // the boxes genuinely overlap and the raw span comes out *shorter* than the wide-gap case.
    // Flooring the width at a fixed minimum (what this used to do) then pushed the bridge's far end
    // straight past the far curve's arc - a real user-reported glitch on every one-lane-wide elbow,
    // both at a branch start and at a merge back: a 1px horizontal stub dangling in mid-air past
    // the corner, with the arc itself flattened under the fill that ran across it. Wider elbows
    // were unaffected, which is exactly why it read as "only when it merges back just below".
    // Clamp to zero instead: the two curves' own straight lead-ins already overlap when the boxes
    // do, so there is nothing left for a bridge to cover.
    let overlap = theme::graph::ELBOW_RADIUS;
    let (left_curve, right_curve) = if rightward {
        (&entry, &exit)
    } else {
        (&exit, &entry)
    };
    let straight_left = left_curve.right() - overlap;
    let straight_width = (right_curve.left + overlap - straight_left).max(px(0.0));

    ElbowGeometry {
        entry,
        straight: StraightSegment {
            left: straight_left,
            top: waist_y,
            width: straight_width,
        },
        exit,
    }
}

/// Which lane's color an elbow should be painted with - pulled out as its own pure function (like
/// `elbow_geometry`) since GPUI's test harness can only inspect painted *bounds*, not colors; this
/// makes the choice itself independently testable. `Diverging`'s `from_lane` is always `own_lane`
/// (the branch being merged *into*) - the connector reads as the branch actually being merged in
/// (`to_lane`) continuing its own color, not the color of the branch it lands in. A real user
/// report ("the line going back to the branch merged should be the same color as the branch
/// instead of the color it is being merged in") asked for exactly this. `Converging` has no
/// "merged into" branch at all (two independently-diverged lanes just happen to share an ancestor,
/// with no merge commit involved) - `from_lane` (the ending lane) is already the right choice,
/// matching the color of the `ends_here` stub it continues.
fn elbow_color_lane(kind: ElbowKind, from_lane: usize, to_lane: usize) -> usize {
    match kind {
        ElbowKind::Diverging => to_lane,
        ElbowKind::Converging => from_lane,
    }
}

/// The vertical span (`top`, `height`) one plain lane segment occupies in its row.
///
/// Pure and GPUI-element-free, like `elbow_geometry`/`elbow_color_lane`, so it is directly testable
/// rather than only reachable through a real painted repo.
///
/// A through-lane always runs the *full* row height, edge to edge. That is what lets an elbow's own
/// straight lead-out be clipped at the row boundary (see `render_graph_lane_canvas`): the stretch
/// the clip removes is exactly the stretch the neighbouring row's own segment starts at, so the
/// line stays continuous while still being painted by exactly one element per lane per row. This
/// used to inset the neighbour by an `ELBOW_OVERSHOOT` constant instead, because the elbow really
/// did paint into the neighbouring row back then; clipping removes the overlap at its source, so
/// there is nothing left to inset for.
fn lane_segment_span(segment: &wt_core::graph::LaneSegment, row_h: Pixels) -> (Pixels, Pixels) {
    match (segment.starts_here, segment.ends_here) {
        (true, _) => (row_h / 2.0, row_h / 2.0),
        (false, true) => (px(0.0), row_h / 2.0),
        (false, false) => (px(0.0), row_h),
    }
}

/// The column header band, sitting between the toolbar and the row list (`revision 3/
/// REVISION-2026-07-31.md` §6.1). Column widths mirror `AdeApp::render_graph_row`'s own real
/// cells exactly - `graph` matches [`theme::graph::LANE_CANVAS`] plus the row's own reserved
/// 2px selection edge, `commit` is the single flex cell standing in for the row's ref-chip
/// (variable-width) and subject (flex) cells combined, `author`/`age`/`sha` and the trailing
/// spacer match the row's own fixed-width cells one for one - so in a flex row, whose trailing
/// fixed-width items always sit a constant distance from the container's own right edge
/// regardless of what precedes the single flex item, this header's `author`/`age`/`sha`/spacer
/// land on exactly the same x as the row's do. Proven, not assumed, by
/// `graph_header_tests::the_fixed_header_columns_land_on_the_same_x_as_the_row_columns_below`.
///
/// Labels are uppercased in Rust (`.to_uppercase()`) rather than via a CSS-style text-transform -
/// GPUI's `Styled` trait has no such property, so every uppercase label in this codebase already
/// does this (`crate::rail::render::AdeApp::render_urgency_worktree_group`,
/// `crate::settings::render::AdeApp::render_settings_nav`). The spec's `.07em` letter-tracking is
/// dropped for the same reason those two also drop their own `.08em`/tracking values: GPUI's
/// `TextStyle` (`vendor/zed/crates/gpui/src/style.rs`) has no letter-spacing field to set it
/// with - a pre-existing gap in this GPUI build, not something newly cut for this header.
fn render_graph_header() -> impl IntoElement {
    fn label(text: &'static str) -> impl IntoElement {
        div()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight(450.0))
            .text_size(px(9.0))
            .text_color(theme::graph::HEADER_LABEL_FG)
            .child(text.to_uppercase())
    }

    div()
        .id("graph-header")
        .debug_selector(|| "graph-header".to_string())
        .flex_none()
        .flex()
        .items_center()
        .w_full()
        .h(theme::graph::HEADER)
        .bg(theme::graph::HEADER_BG)
        .border_b_1()
        .border_color(theme::border::INNER)
        .child(
            div()
                .debug_selector(|| "graph-header-graph".to_string())
                .flex_none()
                .w(theme::graph::LANE_CANVAS + px(2.0))
                .pl(px(11.0))
                .child(label("graph")),
        )
        .child(
            div()
                .debug_selector(|| "graph-header-commit".to_string())
                .flex_1()
                .min_w_0()
                .px(px(6.0))
                .child(label("commit")),
        )
        .child(
            div()
                .debug_selector(|| "graph-header-author".to_string())
                .flex_none()
                .w(px(88.0))
                .px(px(4.0))
                .child(label("author")),
        )
        .child(
            div()
                .debug_selector(|| "graph-header-age".to_string())
                .flex_none()
                .w(px(40.0))
                .text_right()
                .child(label("age")),
        )
        .child(
            div()
                .debug_selector(|| "graph-header-sha".to_string())
                .flex_none()
                .w(px(62.0))
                .text_right()
                .child(label("sha")),
        )
        // The 22px spacer under the row's `⋯` menu column - no label (§6.1: "22 spacer under
        // the `⋯` menu column").
        .child(
            div()
                .debug_selector(|| "graph-header-spacer".to_string())
                .flex_none()
                .w(px(22.0)),
        )
}

/// Draws one row's lane canvas: full-height verticals for every lane passing through, half-height
/// stubs where a lane starts/ends this row, and an elbow box for each merge/branch point (design
/// spec §2). Every element is a flat rect - "Emit one element per lane per row... do not draw two
/// stacked halves per row", so a `starts_here`/`ends_here` segment renders as a single half-height
/// rect anchored to the correct edge, never two.
///
/// That "one element per lane per row" rule holds across row boundaries too, and `overflow_hidden()`
/// is what enforces it. An elbow's far curve is `ELBOW_CURVE_SIZE` tall against only `ROW / 2` of
/// row past the dot, so it always reaches past this row's own box; `elbow_geometry` guarantees the
/// part that spills over is nothing but the curve's *straight* vertical lead-out, on exactly the
/// column the neighbouring row's own full-height `LaneSegment` already draws.
///
/// Clipping it is not merely tidiness. Each row is its own `div()` and rows paint in order, so a
/// later row's opaque `surface::ROW_HOVER`/`ROW_SELECTED` background paints *over* anything an
/// earlier row spilled into its rectangle - which is precisely the reported "lines disappear at
/// elbow connections when hovering" bug: hovering row N erased the tail of row N-1's branch-start
/// elbow. GPUI clips nothing by default (`Style::overflow_mask` returns `None` for the default
/// `Overflow::Visible`, `gpui/src/style.rs`), so this has to be asked for. Note GPUI's content mask
/// is a single rectangle, so this bounds x at the canvas' own width as well - safe because
/// `graph_lane_canvas_width` always reserves `LANE_X_BASE` past the rightmost lane while no elbow
/// piece ever reaches more than one stroke past it (pinned by
/// `elbow_geometry_tests::no_elbow_piece_ever_reaches_past_the_lane_canvas_own_width`).
fn render_graph_lane_canvas(
    row_index: usize,
    row: &GraphRow,
    lane_count: usize,
) -> impl IntoElement {
    let row_h = theme::graph::ROW;
    let mut canvas = div()
        .relative()
        .flex_none()
        .overflow_hidden()
        .w(graph_lane_canvas_width(lane_count))
        .h(row_h)
        .debug_selector(move || format!("graph-row-{row_index}-lane-canvas"));

    for segment in &row.lane_segments {
        // A lane that *exclusively* starts or ends here (not a plain through-lane, which never
        // matches either check below) and has a real elbow at this exact same row already gets
        // that half painted by the elbow box itself (`ElbowGeometry`'s own vertical stroke,
        // anchored at the exact same x - see `elbow_geometry`'s docs): a Converging elbow's
        // `from_lane` is the ending lane, a Diverging elbow's `to_lane` is the lane that starts
        // here. Drawing the plain stub *as well* doubles up on the exact same pixels the elbow
        // already covers, which reads as the vertical line running further than it should (a
        // real user report: "the vertical lines are going too far at the end and at the start").
        // Skip the plain stub in that case rather than draw it twice - the module's own "do not
        // draw two stacked halves per row" principle, extended to also mean "do not draw a plain
        // half *and* an elbow over the same half."
        let ends_here_has_elbow = segment.ends_here
            && !segment.starts_here
            && row
                .elbows
                .iter()
                .any(|e| e.kind == ElbowKind::Converging && e.from_lane == segment.lane);
        let starts_here_has_elbow = segment.starts_here
            && !segment.ends_here
            && row
                .elbows
                .iter()
                .any(|e| e.kind == ElbowKind::Diverging && e.to_lane == segment.lane);
        if ends_here_has_elbow || starts_here_has_elbow {
            continue;
        }

        let x = lane_x(segment.lane);
        let color = lane_color(segment.lane);
        let (top, height) = lane_segment_span(segment, row_h);
        let lane = segment.lane;
        let mut line = div()
            .absolute()
            .w(ELBOW_STROKE)
            .left(x)
            .top(top)
            .h(height)
            .bg(color)
            .debug_selector(move || format!("graph-row-{row_index}-segment-{lane}"));
        if segment.dashed {
            // GPUI has no dashed-border primitive on a plain rect; approximate with a lighter,
            // narrower fill rather than a solid line, so it still reads as visually distinct.
            line = line.opacity(0.5);
        }
        canvas = canvas.child(line);
    }

    for (elbow_index, elbow) in row.elbows.iter().enumerate() {
        let x_from = lane_x(elbow.from_lane);
        let x_to = lane_x(elbow.to_lane);
        let geo = elbow_geometry(elbow.kind, x_from, x_to, row_h);
        let kind_tag = match elbow.kind {
            ElbowKind::Diverging => "diverging",
            ElbowKind::Converging => "converging",
        };
        // Diverging's from_lane is always own_lane (the branch being merged *into*) and to_lane is
        // the branch actually being merged in - the connector must read as that merged branch's own
        // color continuing, not the color of the branch it lands in. Converging has no such
        // "merged into" branch (no merge commit is involved at all - two independent lanes just
        // happen to share an ancestor), so from_lane (the ending lane) is already the right color,
        // matching the already-painted `ends_here` stub it continues.
        let color = lane_color(elbow_color_lane(elbow.kind, elbow.from_lane, elbow.to_lane));

        let render_curve = |curve: CurveBox, part: &'static str| {
            let curve_box = div()
                .absolute()
                .left(curve.left)
                .top(curve.top)
                .w(theme::graph::ELBOW_CURVE_SIZE)
                // Never `ELBOW_CURVE_SIZE` unconditionally - see `CurveBox::height`. The box only
                // ever *grows* on one axis, so `min(width, height) / 2` stays exactly
                // `ELBOW_RADIUS` and GPUI's radius clamp still never engages.
                .h(curve.height)
                .border_color(color)
                .debug_selector(move || {
                    format!("graph-row-{row_index}-elbow-{elbow_index}-{kind_tag}-{part}")
                });
            let curve_box = match curve.horizontal {
                HorizontalEdge::Bottom => curve_box.border_b_1(),
                HorizontalEdge::Top => curve_box.border_t_1(),
            };
            match (curve.horizontal, curve.vertical) {
                (HorizontalEdge::Bottom, VerticalEdge::Left) => curve_box
                    .border_l_1()
                    .rounded_bl(theme::graph::ELBOW_RADIUS),
                (HorizontalEdge::Bottom, VerticalEdge::Right) => curve_box
                    .border_r_1()
                    .rounded_br(theme::graph::ELBOW_RADIUS),
                (HorizontalEdge::Top, VerticalEdge::Left) => curve_box
                    .border_l_1()
                    .rounded_tl(theme::graph::ELBOW_RADIUS),
                (HorizontalEdge::Top, VerticalEdge::Right) => curve_box
                    .border_r_1()
                    .rounded_tr(theme::graph::ELBOW_RADIUS),
            }
        };

        canvas = canvas.child(render_curve(geo.entry, "entry"));
        canvas = canvas.child(
            div()
                .absolute()
                .left(geo.straight.left)
                .top(geo.straight.top)
                .w(geo.straight.width)
                .h(px(1.0))
                .bg(color)
                .debug_selector(move || {
                    format!("graph-row-{row_index}-elbow-{elbow_index}-{kind_tag}-straight")
                }),
        );
        canvas = canvas.child(render_curve(geo.exit, "exit"));
    }

    let dot_lane = row.lane;
    let dot_x = lane_x(dot_lane);
    let dot_color = lane_color(dot_lane);
    let (size, dot) = match row.dot_kind {
        DotKind::Commit => (theme::graph::DOT_COMMIT, div().rounded_full().bg(dot_color)),
        DotKind::Head => (
            theme::graph::DOT_HEAD_OR_MERGE,
            div()
                .rounded_full()
                .bg(dot_color)
                .border_2()
                .border_color(theme::graph::HEAD_RING),
        ),
        DotKind::Merge => (
            theme::graph::DOT_HEAD_OR_MERGE,
            div().rounded_full().border_2().border_color(dot_color),
        ),
        DotKind::WorkingTree => (
            theme::graph::DOT_COMMIT,
            div()
                .rounded_full()
                .border_1()
                .border_color(theme::graph::WORKING_TREE_BORDER)
                .opacity(0.8),
        ),
    };
    canvas = canvas.child(
        dot.absolute()
            .left(dot_x - size / 2.0 + px(0.5))
            .top(row_h / 2.0 - size / 2.0)
            .w(size)
            .h(size),
    );

    let _ = lane_count;
    canvas
}

/// Ref chips for one row (design spec §2): local branches on their lane-colour dim pair, `HEAD`,
/// outlined remotes, and tags.
fn render_graph_ref_chips(row: &GraphRow) -> impl IntoElement {
    let mut chips = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0));
    for chip in &row.commit.refs {
        let element = match chip.kind {
            RefKind::LocalBranch => div()
                .px(px(6.0))
                .h(px(15.0))
                .flex()
                .items_center()
                .rounded(theme::radius::MARK)
                .bg(local_branch_dim_bg(lane_for_ref(row, chip)))
                .font(font(theme::font::MONO))
                .text_size(px(9.0))
                .text_color(lane_color(lane_for_ref(row, chip)))
                .child(chip.name.clone()),
            RefKind::RemoteBranch => div()
                .px(px(6.0))
                .h(px(15.0))
                .flex()
                .items_center()
                .rounded(theme::radius::MARK)
                .border_1()
                .border_color(theme::graph::REMOTE_CHIP_BORDER)
                .font(font(theme::font::MONO))
                .text_size(px(9.0))
                .text_color(theme::text::DIM)
                .child(chip.name.clone()),
            RefKind::Tag => div()
                .px(px(6.0))
                .h(px(15.0))
                .flex()
                .items_center()
                .rounded(theme::radius::MARK)
                .bg(theme::graph::TAG_CHIP_BG)
                .font(font(theme::font::MONO))
                .text_size(px(9.0))
                .text_color(theme::graph::TAG_CHIP_FG)
                .child(chip.name.clone()),
        };
        chips = chips.child(element);
        if chip.is_head {
            chips = chips.child(
                div()
                    .px(px(6.0))
                    .h(px(15.0))
                    .flex()
                    .items_center()
                    .rounded(theme::radius::MARK)
                    .bg(theme::graph::HEAD_CHIP_BG)
                    .font(font(theme::font::MONO))
                    .text_size(px(9.0))
                    .text_color(theme::graph::HEAD_CHIP_FG)
                    .child("HEAD"),
            );
        }
    }
    chips
}

fn lane_for_ref(row: &GraphRow, _chip: &wt_core::graph::RefChip) -> usize {
    row.lane
}

/// Real coverage for the row `⋯`/right-click context menu's positioning - a follow-up refinement
/// to the git graph tab (GitHub issue #1) fixing two real user reports: the menu only opened via
/// the `⋯` button, never a right-click anywhere on the row; and wherever it did open was computed
/// from a fixed per-row formula (`left(px(140.0))`, `top(bounds.origin.y - bounds.size.height)`)
/// rather than any real click or button position. Mirrors `crate::sidebar::tree_ops`'s own real
/// right-click coverage (`right_clicking_a_folder_row_opens_the_folder_menu_at_a_clamped_origin`):
/// `cx.simulate_event`/`cx.simulate_click` drive genuine mouse events through the real dispatch
/// path, not direct method calls, so these also cover real interactions between the row's own
/// handler and the scrim/popover it opens (an adversarial audit's own two findings:
/// `a_real_second_click_on_the_same_dots_button_closes_the_menu` and
/// `right_clicking_inside_the_open_popover_does_not_retarget_to_the_row_underneath`).
#[cfg(test)]
mod graph_row_menu_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Bounds, Entity, Pixels, Point, Size, TestAppContext};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Three real commits, clean working tree at the end - `build_graph` yields exactly three
    /// real commit rows (indices 0..=2, newest first), with no "Uncommitted changes" row to
    /// throw off the indices these tests target.
    fn seed_three_commits(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        std::fs::write(dir.join("a.txt"), "1\n").expect("write a.txt");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "2\n").expect("write a.txt");
        git(dir, &["commit", "-am", "second"]);
        std::fs::write(dir.join("a.txt"), "3\n").expect("write a.txt");
        git(dir, &["commit", "-am", "third"]);
    }

    fn open_seeded_graph(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_three_commits(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        (repo, app, cx)
    }

    fn right_click(cx: &mut gpui::VisualTestContext, position: gpui::Point<Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn right_clicking_a_row_opens_its_menu_at_the_real_click_position(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let row = cx
            .debug_bounds("graph-row-0")
            .expect("the first commit row must be painted");
        right_click(cx, row.center());

        app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .row_menu_open
                .expect("a real right-click on a commit row must open its menu");
            assert_eq!(
                menu.row_index, 0,
                "the row under the cursor, not some other row"
            );
            assert_eq!(
                (menu.origin_x, menu.origin_y),
                (row.center().x, row.center().y),
                "anchored at the real click position, not a per-row-index formula"
            );
        });
        let painted = cx
            .debug_bounds("graph-row-menu-popover")
            .expect("and it must genuinely paint there");
        app.read_with(cx, |app, _| {
            let menu = app.graph_state.row_menu_open.expect("menu");
            assert_eq!(
                (painted.origin.x, painted.origin.y),
                (menu.origin_x, menu.origin_y),
                "the popover must paint at exactly the position captured when it opened"
            );
        });
    }

    /// `theme::graph::ROW_MENU_HEIGHT` is a hand-measured constant (this menu's content is fixed,
    /// so unlike `crate::sidebar::context_menu::menu_height` it has no analytical formula to
    /// compute it from - see that constant's own docs) that `AdeApp::open_graph_row_menu_at`'s
    /// edge clamp relies on being accurate. If the menu's real content ever changes (a row added
    /// or removed, a header renamed to wrap onto two lines), this is what catches the constant
    /// having quietly gone stale - a menu clamped against the wrong height can still paint
    /// off-screen, the exact bug this whole change fixed.
    #[gpui::test]
    fn the_row_menu_pins_the_real_height_this_edge_clamp_relies_on(cx: &mut TestAppContext) {
        let (_repo, _app, cx) = open_seeded_graph(cx);

        let row = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row.center());
        let painted = cx
            .debug_bounds("graph-row-menu-popover")
            .expect("the popover must genuinely paint");

        assert_eq!(
            (painted.size.width, painted.size.height),
            (theme::graph::ROW_MENU_WIDTH, theme::graph::ROW_MENU_HEIGHT),
            "the real painted size must match the constants the edge clamp uses - re-measure and \
             update ROW_MENU_HEIGHT if this menu's content genuinely changed"
        );
    }

    /// The scenario the design conversation flagged by name: right-clicking a different,
    /// unobscured row while another row's menu is already open must close the old one and open
    /// the new one at the new position - never leave the stale popover up, and never open the
    /// new one at the old row's position.
    ///
    /// Opens on row 2 (the *last* row) first and right-clicks row 0 (well above it) second -
    /// deliberately in that order, not row 0 then row 1: this popover's real painted height
    /// (`theme::graph::ROW_MENU_HEIGHT`) is far taller than the gap between two adjacent rows in
    /// this small fixture, so a menu opened on an *earlier* row would visually cover a *later*
    /// one, and right-clicking through an open popover onto whatever it covers is a different,
    /// separately-covered scenario (see
    /// `right_clicking_inside_the_open_popover_does_not_retarget_to_the_row_underneath`) with a
    /// deliberately different outcome (occluded, not retargeted). Row 0 sits above row 2's click
    /// point, so its own popover never reaches back up over it.
    #[gpui::test]
    fn right_clicking_a_different_unobscured_row_replaces_the_open_menu_at_the_new_position(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let row2 = cx.debug_bounds("graph-row-2").expect("row 2 painted");
        right_click(cx, row2.center());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.row_menu_open.map(|m| m.row_index),
                Some(2),
                "premise: row 2's menu really is open first"
            );
        });

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        assert!(
            row0.center().y < row2.center().y,
            "premise: row 0 sits above row 2, so row 2's own downward-opening popover cannot \
             cover it"
        );
        right_click(cx, row0.center());

        app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .row_menu_open
                .expect("row 0's right-click must open a menu");
            assert_eq!(
                menu.row_index, 0,
                "the new row's menu must win - not still row 2's stale one"
            );
            assert_eq!(
                (menu.origin_x, menu.origin_y),
                (row0.center().x, row0.center().y),
                "anchored at row 0's own click position, not row 2's leftover one"
            );
        });
    }

    /// `gpui`'s own `on_click` only ever fires for `MouseButton::Left`
    /// (`~/.cargo/git/checkouts/zed-*/*/crates/gpui/src/elements/div.rs`, the
    /// `event.button == MouseButton::Left` gate around its mouse-down tracking), so this is
    /// structurally guaranteed rather than something `cx.stop_propagation()` in the row's own
    /// right-click handler achieves - still worth a real assertion, since it is exactly the kind
    /// of "surely that's fine" gap an adversarial audit exists to catch.
    #[gpui::test]
    fn right_clicking_a_row_does_not_also_select_it(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.selected_row,
                Some(0),
                "premise: loading the graph really does select the first row"
            );
        });

        let row1 = cx.debug_bounds("graph-row-1").expect("row 1 painted");
        right_click(cx, row1.center());

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.selected_row,
                Some(0),
                "a right-click must open the menu without also changing the selected row"
            );
            assert_eq!(app.graph_state.row_menu_open.map(|m| m.row_index), Some(1));
        });
    }

    /// Critical fix: unlike a left-click, `MouseDownEvent::is_focusing()` is `false` for a
    /// right-click (`vendor` `gpui`'s own default), so `gpui`'s automatic click-to-focus never
    /// ran for it - an adversarial audit found a right-click opened the menu but left keyboard
    /// focus wherever it was before. `AdeApp::open_graph_row_menu_at`'s own explicit
    /// `window.focus` is the fix, mirroring `crate::sidebar::tree_ops::AdeApp::
    /// open_tree_context_menu`'s identical `self.focus_file_tree(window, cx)` call for the same
    /// reason.
    #[gpui::test]
    fn right_clicking_a_row_focuses_the_graph_view(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        // `open_git_graph` already focuses `graph_focus_handle` by default, so proving a
        // right-click *moves* focus there needs it to genuinely start somewhere else first - the
        // Branches filter box, the same real, independently-focusable surface
        // `leaving_the_graph_tab_from_the_branches_filter_lands_on_the_real_session_pane` (in
        // `graph_focus_tests` below) uses for the identical reason.
        app.update_in(cx, |app, _window, cx| {
            app.set_graph_right_panel(GraphRightPanel::Branches, cx);
        });
        let (focused_before, graph_handle, filter_handle) = app.update_in(cx, |app, window, cx| {
            window.focus(&app.graph_state.branches_filter_focus_handle, cx);
            (
                window.focused(cx),
                app.graph_focus_handle.clone(),
                app.graph_state.branches_filter_focus_handle.clone(),
            )
        });
        assert_eq!(
            focused_before.as_ref(),
            Some(&filter_handle),
            "premise: focus really did move to the filter box first"
        );
        assert_ne!(focused_before.as_ref(), Some(&graph_handle));

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());

        let focused_after = app.update_in(cx, |_app, window, cx| window.focused(cx));
        assert_eq!(
            focused_after.as_ref(),
            Some(&graph_handle),
            "a right-click that opens the row menu must also move real keyboard focus onto the \
             graph view"
        );
    }

    /// The `⋯` button's own anchor: derived from that row's real captured trigger bounds
    /// (`row_menu_bounds`), not a formula involving the row's index - proven here with a bounds
    /// value far down the y axis, standing in for a row deep in a scrolled list, which a fixed
    /// `index * row_height` formula would get wrong (the real, adversarial-audit-relevant case).
    #[gpui::test]
    fn the_dots_button_anchors_off_its_own_real_captured_bounds(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_three_commits(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let scrolled_bounds = Bounds {
            origin: Point::new(px(500.0), px(2000.0)),
            size: Size::new(px(22.0), px(22.0)),
        };
        app.update_in(cx, |app, window, cx| {
            app.graph_state.row_menu_bounds.insert(2, scrolled_bounds);
            app.toggle_graph_row_menu(2, window, cx);
        });

        let viewport = cx.update(|window, _cx| window.bounds().size);
        // `y = 2000` is past the (real, 1080px-tall) test viewport, so the real edge-clamp must
        // have kicked in - computed here via the exact same real `clamp_menu_origin` the
        // implementation calls, not a second, hand-derived formula that could quietly drift from
        // it.
        let unclamped_x =
            scrolled_bounds.origin.x + scrolled_bounds.size.width - theme::graph::ROW_MENU_WIDTH;
        let unclamped_y = scrolled_bounds.origin.y + scrolled_bounds.size.height + px(2.0);
        let (expected_x, expected_y) = crate::sidebar::context_menu::clamp_menu_origin(
            f32::from(unclamped_x),
            f32::from(unclamped_y),
            f32::from(theme::graph::ROW_MENU_WIDTH),
            f32::from(theme::graph::ROW_MENU_HEIGHT),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        app.read_with(cx, |app, _| {
            let menu = app.graph_state.row_menu_open.expect("the menu must open");
            assert_eq!(menu.row_index, 2);
            assert_eq!(
                (menu.origin_x, menu.origin_y),
                (px(expected_x), px(expected_y)),
                "x/y both come from the button's own real bounds (run through the real edge \
                 clamp), not row_index * a row height - a formula that would put this \
                 2000px-deep row's menu at roughly 48px"
            );
            assert_ne!(
                menu.origin_y, unclamped_y,
                "premise: y=2000 really is past the viewport, so the clamp really did something"
            );
        });
    }

    /// `revision 3/REVISION-2026-07-31.md` §6.1 added a real 22px column header band between
    /// the toolbar and the row list. The `⋯` button's own anchor
    /// (`AdeApp::toggle_graph_row_menu`) is built entirely from that row's own real captured
    /// `row_menu_bounds` - never `TOOLBAR`/`HEADER`/the row's index - so adding the header should
    /// shift it down for free, with zero code changes to the anchor formula itself. This proves
    /// that, end to end, through a *real* click on row 1's own `⋯` button (not a synthetic bounds
    /// value like `the_dots_button_anchors_off_its_own_real_captured_bounds` above uses): first
    /// that the row list genuinely starts immediately under the header's own real painted bottom
    /// edge (not an assumed offset), then that the button click opens the menu at exactly that
    /// real, header-shifted button position.
    #[gpui::test]
    fn the_dots_button_anchor_reflects_the_real_header_band_now_above_the_rows(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let header = cx
            .debug_bounds("graph-header")
            .expect("the column header must be painted");
        assert_eq!(
            header.size.height,
            theme::graph::HEADER,
            "the header's real painted height must match the constant the anchor math relies on"
        );

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        assert_eq!(
            row0.origin.y,
            header.origin.y + header.size.height,
            "the row list must start immediately under the header's own real painted bottom \
             edge, not some assumed `TOOLBAR`-only offset"
        );

        let row1 = cx.debug_bounds("graph-row-1").expect("row 1 painted");
        assert_eq!(
            row1.origin.y,
            row0.origin.y + row0.size.height,
            "premise: row 1 sits immediately below row 0 with no gap - the real, contiguous \
             layout the header must have shifted as a whole, not just row 0"
        );

        let button = cx
            .debug_bounds("graph-row-menu-button-1")
            .expect("row 1's own ⋯ trigger must be painted");

        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .row_menu_open
                .expect("clicking row 1's own ⋯ button must open its menu");
            assert_eq!(menu.row_index, 1);
            assert_eq!(
                (menu.origin_x, menu.origin_y),
                (
                    button.origin.x + button.size.width - theme::graph::ROW_MENU_WIDTH,
                    button.origin.y + button.size.height + px(2.0),
                ),
                "anchored off row 1's own real captured bounds, which already sit lower now \
                 that the header band is a real sibling above the row list - `open_graph_row_menu_at`/ \
                 `toggle_graph_row_menu` needed zero changes for this to hold"
            );
        });
    }

    /// Real dispatch, not a direct method call: an adversarial audit of an earlier draft of this
    /// change found that a *real* second click on the same button did not close the menu the way
    /// a direct `toggle_graph_row_menu` call in a test claimed - the already-open menu's own
    /// scrim paints on top of the button (see `Self::render_graph_row_menu`'s docs) and, without
    /// `cx.stop_propagation()` in the scrim's dismiss handler, that same click *also* reached the
    /// button underneath and reopened what the scrim had just closed. This exercises the real fix
    /// through the real click path.
    #[gpui::test]
    fn a_real_second_click_on_the_same_dots_button_closes_the_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let button = cx
            .debug_bounds("graph-row-menu-button-0")
            .expect("row 0's ⋯ button must be painted");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.row_menu_open.map(|m| m.row_index),
                Some(0),
                "the first real click opens it"
            );
        });

        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_none(),
                "a second real click on the same button must close it, not reopen it"
            );
        });
    }

    /// `toggle_graph_row_menu`'s own decision logic, given manually-seeded `row_menu_bounds` -
    /// deliberately a direct-call, pure-state test (not a claim about real click dispatch; see
    /// `a_real_second_click_on_the_same_dots_button_closes_the_menu` above for that): it proves
    /// the `already_open_here` branch keys off `row_index`, not "any menu is open at all", so
    /// switching to a different row's button reanchors rather than toggling the first one closed.
    #[gpui::test]
    fn toggle_graph_row_menu_reanchors_for_a_different_row_rather_than_toggling_closed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_three_commits(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.graph_state.row_menu_bounds.insert(
                0,
                Bounds {
                    origin: Point::new(px(10.0), px(10.0)),
                    size: Size::new(px(22.0), px(22.0)),
                },
            );
            app.graph_state.row_menu_bounds.insert(
                1,
                Bounds {
                    origin: Point::new(px(10.0), px(40.0)),
                    size: Size::new(px(22.0), px(22.0)),
                },
            );
            app.toggle_graph_row_menu(0, window, cx);
        });
        app.update_in(cx, |app, window, cx| {
            app.toggle_graph_row_menu(1, window, cx);
        });

        app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .row_menu_open
                .expect("row 1's button must open a menu, not close row 0's");
            assert_eq!(menu.row_index, 1);
            assert_eq!(menu.origin_y, px(40.0) + px(22.0) + px(2.0));
        });
    }

    /// Critical fix: a right-click *inside the open popover's own painted bounds* (but not on any
    /// actual menu row) must not fall through to whatever graph row the popover happens to be
    /// painted on top of - an adversarial audit reproduced exactly this by right-clicking inside
    /// an open menu and getting a *different commit's* menu back at the cursor. `.occlude()` on
    /// the popover panel (`Self::render_graph_row_menu`) is the fix; this proves it holds via a
    /// real click, not by reading the code and trusting it.
    #[gpui::test]
    fn right_clicking_inside_the_open_popover_does_not_retarget_to_the_row_underneath(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());
        let (before_index, before_origin) = app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .row_menu_open
                .expect("premise: row 0's menu really is open");
            (menu.row_index, (menu.origin_x, menu.origin_y))
        });
        assert_eq!(before_index, 0);

        let popover = cx
            .debug_bounds("graph-row-menu-popover")
            .expect("premise: the popover really is painted");
        // The popover opens *over* the row list in this small, three-row fixture (its real
        // painted height is far taller than three rows), so its own centre genuinely sits on top
        // of another row's hitbox - exactly the geometry the original bug needed.
        right_click(cx, popover.center());

        app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .row_menu_open
                .expect("still open - occluded, not dismissed by this click");
            assert_eq!(
                (menu.row_index, (menu.origin_x, menu.origin_y)),
                (before_index, before_origin),
                "a right-click inside the popover must not retarget it to whatever row is \
                 underneath, and must not move it either"
            );
        });
    }

    /// A real, adversarial-audit-found gap in this change (pre-existing, not introduced by it):
    /// the row `⋯`/right-click menu and the Push `▾` menu are independent state, with nothing
    /// stopping both from being open at once - opening one left the other's own full-window scrim
    /// painted underneath it, silently eating the next click aimed at dismissing it.
    /// `Self::open_graph_row_menu_at` and `Self::toggle_graph_push_menu` now each close the other.
    #[gpui::test]
    fn opening_the_row_menu_closes_an_open_push_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        app.update_in(cx, |app, _window, cx| {
            app.toggle_graph_push_menu(cx);
        });
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.push_menu_open,
                "premise: the Push menu really is open"
            );
        });

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());

        app.read_with(cx, |app, _| {
            assert_eq!(app.graph_state.row_menu_open.map(|m| m.row_index), Some(0));
            assert!(
                !app.graph_state.push_menu_open,
                "opening the row menu must close the Push menu, not paint both at once"
            );
        });
    }

    /// The other direction of the same fix: opening the Push menu while a row menu is open must
    /// close the row menu.
    #[gpui::test]
    fn opening_the_push_menu_closes_an_open_row_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_some(),
                "premise: the row menu really is open"
            );
        });

        app.update_in(cx, |app, _window, cx| {
            app.toggle_graph_push_menu(cx);
        });

        app.read_with(cx, |app, _| {
            assert!(app.graph_state.push_menu_open);
            assert!(
                app.graph_state.row_menu_open.is_none(),
                "opening the Push menu must close the row menu, not paint both at once"
            );
        });
    }

    /// Real, reachable-without-Settings bug an adversarial audit found: `open_git_graph` only
    /// calls `load_graph` (which is what actually clears `row_menu_open`) while the graph is
    /// still `NotLoaded`, so switching away from an *already-loaded* graph tab with a row menu
    /// open and then back does not reload - and, before this fix, left the stale menu's
    /// `graph_tab_active`-gated overlay (`crate::root::AdeApp::render`) reappear the instant the
    /// tab became active again, with no click at all. `Self::leave_graph_tab` now clears it.
    #[gpui::test]
    fn switching_away_from_the_graph_tab_and_back_does_not_resurrect_a_stale_row_menu(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_some(),
                "premise: the row menu really is open"
            );
        });

        // Calls `leave_graph_tab` directly, not through `select_session` - `select_session` can
        // also route through `Self::select_worktree` (when the target session belongs to a
        // worktree not already selected), which calls `Self::load_graph` unconditionally on its
        // own and would clear `row_menu_open` for an unrelated reason, confounding what this test
        // means to isolate: `leave_graph_tab`'s *own* clear, for the plain "leave the tab, same
        // worktree, same session, tab was already loaded" path.
        app.update_in(cx, |app, window, cx| {
            app.leave_graph_tab(window, cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app.graph_tab_active),
            "premise: the graph tab really was left"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| matches!(
                app.graph_state.load,
                GraphLoadState::Loaded(_)
            )),
            "premise: re-opening reused the already-loaded graph rather than reloading it, so \
             load_graph's own row_menu_open clear did not run"
        );

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_none(),
                "a stale row menu must not resurrect itself just from re-activating the tab"
            );
        });
    }

    /// Real, adversarial-audit-found bug: opening Settings does not clear `graph_tab_active` (the
    /// graph tab, if showing, is still "active" underneath Settings), so an open row or Push menu
    /// kept painting its full-window scrim over the Settings surface, swallowing the first click
    /// aimed at it. `Self::open_settings` now dismisses both.
    #[gpui::test]
    fn opening_settings_dismisses_an_open_row_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_some(),
                "premise: the row menu really is open"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
        });

        app.read_with(cx, |app, _| {
            assert!(app.settings_open, "premise: Settings really did open");
            assert!(
                app.graph_state.row_menu_open.is_none(),
                "an open row menu must not keep painting its scrim over Settings"
            );
        });
    }

    /// `revision 3/REVISION-2026-07-31.md` §6.1: "Verify the fixed columns land on the same x as
    /// a real row's." The spec gives illustrative numbers for its own 1520-wide reference build;
    /// this compares this app's own real painted bounds against each other instead of hardcoding
    /// those numbers, since a real column width here would make a hardcoded literal the thing
    /// that's wrong, not the layout. Covers `author`/`age`/`sha` - the three fixed-width columns
    /// both the header and a row genuinely share (the header's `graph`/`commit` cells stand in
    /// for the row's `lane canvas`/`ref chips`/`subject` cells, which don't have single matching
    /// counterparts to compare against 1:1).
    #[gpui::test]
    fn the_fixed_header_columns_land_on_the_same_x_as_the_row_columns_below(
        cx: &mut TestAppContext,
    ) {
        let (_repo, _app, cx) = open_seeded_graph(cx);

        let columns: [(&'static str, &'static str, &'static str); 3] = [
            ("author", "graph-header-author", "graph-row-0-author"),
            ("age", "graph-header-age", "graph-row-0-age"),
            ("sha", "graph-header-sha", "graph-row-0-sha"),
        ];
        for (column, header_selector, row_selector) in columns {
            let header = cx
                .debug_bounds(header_selector)
                .unwrap_or_else(|| panic!("the header's {column} cell must be painted"));
            let row = cx
                .debug_bounds(row_selector)
                .unwrap_or_else(|| panic!("row 0's {column} cell must be painted"));
            assert_eq!(
                (header.origin.x, header.size.width),
                (row.origin.x, row.size.width),
                "the header's {column} column must land on the exact same x and width as the \
                 row's own {column} cell, or the label above it describes the wrong content"
            );
        }

        let header_spacer = cx
            .debug_bounds("graph-header-spacer")
            .expect("the header's trailing spacer must be painted");
        let row_menu_button = cx
            .debug_bounds("graph-row-menu-button-0")
            .expect("row 0's own ⋯ trigger must be painted");
        assert_eq!(
            (header_spacer.origin.x, header_spacer.size.width),
            (row_menu_button.origin.x, row_menu_button.size.width),
            "the header's unlabelled trailing spacer must sit exactly over the row's own `⋯` \
             column"
        );
    }
}

/// Pure, GPUI-paint-free coverage for `elbow_geometry`'s real S-curve shape - an adversarial audit
/// of the git graph tab's original `Converging`-elbow fix found that `graph_elbow_render_tests`
/// below (which only checks painted *box bounds* via `debug_bounds`) cannot distinguish
/// `border_t_1`+`rounded_tl` from `border_b_1`+`rounded_bl`: swapping which edge/corner a box's
/// border goes on does not change the box's own position or size, only which pixels inside it are
/// actually drawn. These tests assert the edge/corner choice directly for both the entry and exit
/// curve, and that the straight middle segment (when the lane gap is wide enough to need one)
/// bridges exactly between them with no gap or overlap - a real user report against the earlier
/// single-corner design ("the end of the lines need to have corners too so they rejoin after
/// merge") is what this two-curve shape exists to satisfy.
#[cfg(test)]
mod elbow_geometry_tests {
    use super::*;

    const ROW_H: Pixels = theme::graph::ROW;
    const RADIUS: Pixels = theme::graph::ELBOW_RADIUS;
    const CURVE_SIZE: Pixels = theme::graph::ELBOW_CURVE_SIZE;
    /// The smallest disc a row's own dot can be painted at, and so the tightest bound on how far an
    /// elbow's dot end may run before it stops being hidden underneath that dot.
    const SMALLEST_DOT: Pixels = theme::graph::DOT_COMMIT;

    #[test]
    fn elbow_curve_size_is_exactly_double_the_radius_so_gpuis_own_clamp_never_kicks_in() {
        // GPUI (`Corners::clamp_radii_for_quad_size`, `vendor/zed/crates/gpui/src/style.rs`)
        // always clamps a requested corner radius to half the box's own shorter side. A real
        // user-reported vertical-alignment bug traced back to this: the curve box used to be
        // exactly `ELBOW_RADIUS`-square with a requested radius of `ELBOW_RADIUS` itself, silently
        // clamped down to half that (an invisible straight lead-in eating the other half of the
        // box, with only a much smaller arc actually rendering). This is the one invariant that
        // must hold for the radius to render unclamped, at its own full requested size.
        assert_eq!(CURVE_SIZE, RADIUS * 2.0);
    }

    /// The `waist_y` row every piece of one elbow must actually *paint* on. Deliberately derived
    /// the same way GPUI itself lays a border out - inward from the box's own bounds edge (see
    /// `CurveBox`'s own docs) - rather than from the box's anchoring edge, because the entire bug
    /// this module spent four failed attempts on was the difference between those two.
    fn painted_horizontal_row(curve: &CurveBox) -> Pixels {
        match curve.horizontal {
            // `border_t` on a box whose top edge is `y` paints `[y, y + 1)`.
            HorizontalEdge::Top => curve.top,
            // `border_b` on a box whose bottom edge is `y` paints `[y - 1, y)`.
            HorizontalEdge::Bottom => curve.top + curve.height - ELBOW_STROKE,
        }
    }

    #[test]
    fn all_three_elbow_pieces_paint_their_horizontal_stroke_on_the_very_same_pixel_row() {
        // The real regression test for the root cause described in `CurveBox`'s own docs, and the
        // one invariant every earlier fix attempt violated while its *coordinate* arithmetic
        // looked perfectly consistent. GPUI paints a 1px border inside the box, so an entry curve
        // whose bottom edge equals the exit curve's top edge paints one row *above* it - a
        // reproducible 1px step, measured directly off the user's own screenshots (entry on row
        // 121 against a bridge and exit on row 122 for a real diverging elbow; entry on row 23
        // against a bridge and exit on row 24 for a real converging one).
        //
        // Covers every shape this module can produce: both kinds, both directions, and the
        // degenerate same-lane case.
        for kind in [ElbowKind::Diverging, ElbowKind::Converging] {
            for (x_from, x_to) in [
                (px(9.0), px(23.0)),
                (px(23.0), px(9.0)),
                (px(9.0), px(9.0 + 3.0 * 14.0)),
                (px(9.0 + 3.0 * 14.0), px(9.0)),
                (px(9.0), px(9.0)),
            ] {
                let geo = elbow_geometry(kind, x_from, x_to, ROW_H);
                let entry_row = painted_horizontal_row(&geo.entry);
                let exit_row = painted_horizontal_row(&geo.exit);
                assert_eq!(
                    entry_row, geo.straight.top,
                    "{kind:?} {x_from:?}->{x_to:?}: the entry curve's painted horizontal row \
                     must be the straight bridge's own row, not one above it"
                );
                assert_eq!(
                    exit_row, geo.straight.top,
                    "{kind:?} {x_from:?}->{x_to:?}: the exit curve's painted horizontal row must \
                     be the straight bridge's own row"
                );
            }
        }
    }

    #[test]
    fn only_the_bottom_edged_curve_carries_the_extra_stroke_of_height() {
        // Pins *which* box gets corrected. An earlier attempt grew both curves toward the waist,
        // which just moved the 1px step to the other side of the bridge - only the box whose
        // border is painted on the far side of its own anchoring edge is wrong.
        for kind in [ElbowKind::Diverging, ElbowKind::Converging] {
            let geo = elbow_geometry(kind, px(9.0), px(23.0), ROW_H);
            assert_eq!(geo.entry.horizontal, HorizontalEdge::Bottom);
            assert_eq!(geo.entry.height, CURVE_SIZE + ELBOW_STROKE);
            assert_eq!(geo.exit.horizontal, HorizontalEdge::Top);
            assert_eq!(geo.exit.height, CURVE_SIZE);
        }
    }

    #[test]
    fn the_extra_stroke_of_height_still_cannot_trigger_gpuis_radius_clamp() {
        // The correction above grows a box on one axis only. GPUI clamps a requested radius to
        // half the box's own *shorter* side, so the invariant that actually has to hold is on the
        // shorter side, not on `ELBOW_CURVE_SIZE` alone - growing the other axis is free, but a
        // future change that shrank either one below `2 * RADIUS` would silently halve the arc
        // again (exactly the earlier bug the sibling test above pins).
        for kind in [ElbowKind::Diverging, ElbowKind::Converging] {
            let geo = elbow_geometry(kind, px(9.0), px(23.0), ROW_H);
            for curve in [geo.entry, geo.exit] {
                let shorter = CURVE_SIZE.min(curve.height);
                assert!(
                    shorter / 2.0 >= RADIUS,
                    "{kind:?} {curve:?}: half the shorter side ({:?}) must still be at least the \
                     requested radius {RADIUS:?}, or GPUI silently renders a smaller arc",
                    shorter / 2.0
                );
            }
        }
    }

    /// The column a curve's own vertical border stroke is actually painted on. Derived the way
    /// GPUI lays a border out - inward from the bounds edge, see `CurveBox`'s own docs - rather
    /// than from the box's anchoring edge, because the difference between those two *is* the bug
    /// this module exists to keep fixed.
    fn painted_vertical_column(curve: &CurveBox) -> Pixels {
        match curve.vertical {
            VerticalEdge::Left => curve.left,
            VerticalEdge::Right => curve.right() - ELBOW_STROKE,
        }
    }

    /// The `(lane_x, curve)` pair for each end of an elbow, given the lanes it runs between.
    fn ends(kind: ElbowKind, x_from: Pixels, x_to: Pixels) -> [(Pixels, CurveBox, LaneJoin); 2] {
        let geo = elbow_geometry(kind, x_from, x_to, ROW_H);
        let (entry_join, exit_join) = match kind {
            ElbowKind::Diverging => (LaneJoin::LeavesDot, LaneJoin::ContinuesLane),
            ElbowKind::Converging => (LaneJoin::ContinuesLane, LaneJoin::LeavesDot),
        };
        [(x_from, geo.entry, entry_join), (x_to, geo.exit, exit_join)]
    }

    /// Every shape `elbow_geometry` can produce: both kinds, both directions. `wt_core::graph`'s
    /// `layout_lanes` only ever emits leftward `Converging` elbows (Step 2 takes `own_lane` as the
    /// *lowest* lane expecting this commit, so every other converging lane has a higher index),
    /// but it does emit `Diverging` in both directions - Step 4's `to_lane` is either an existing
    /// lane found by `position` or a reused free slot, and either can sit left of `own_lane`. The
    /// rightward-`Converging` case is therefore currently unreachable in practice and covered here
    /// only so the geometry stays right if that ever changes.
    const ALL_SHAPES: [(ElbowKind, Pixels, Pixels); 4] = [
        (ElbowKind::Diverging, px(9.0), px(51.0)),
        (ElbowKind::Diverging, px(51.0), px(9.0)),
        (ElbowKind::Converging, px(51.0), px(9.0)),
        (ElbowKind::Converging, px(9.0), px(51.0)),
    ];

    /// Every shape in [`ALL_SHAPES`] at both a wide gap (three lane steps, `lane_x(0)`/`lane_x(3)`)
    /// and the *minimum* one-lane-step gap (`lane_x(0)`/`lane_x(1)`). The narrow gap is its own
    /// distinct case rather than just a smaller number: `LANE_STEP` (14px) is less than
    /// `2 * ELBOW_CURVE_SIZE` (20px), so the two curve boxes genuinely overlap, and a real
    /// user-reported glitch lived in exactly that regime - see
    /// `the_straight_bridge_never_runs_past_the_far_curves_own_arc`.
    const ALL_SHAPES_AND_GAPS: [(ElbowKind, Pixels, Pixels); 8] = [
        (ElbowKind::Diverging, px(9.0), px(51.0)),
        (ElbowKind::Diverging, px(51.0), px(9.0)),
        (ElbowKind::Converging, px(51.0), px(9.0)),
        (ElbowKind::Converging, px(9.0), px(51.0)),
        (ElbowKind::Diverging, px(9.0), px(23.0)),
        (ElbowKind::Diverging, px(23.0), px(9.0)),
        (ElbowKind::Converging, px(23.0), px(9.0)),
        (ElbowKind::Converging, px(9.0), px(23.0)),
    ];

    #[test]
    fn a_lane_continuing_curve_paints_its_stroke_on_that_lanes_own_column() {
        // The real regression test for the horizontal counterpart of the border-box off-by-one.
        // A `LaneJoin::ContinuesLane` curve *replaces* that lane's own plain `LaneSegment` stub
        // (`render_graph_lane_canvas` skips it), so its painted stroke has to land on exactly the
        // column `lane_x` where that lane's own 1px vertical sits - in the row above for a
        // `Converging` entry, in the row below for a `Diverging` exit.
        //
        // This is why anchoring has to be per-edge. `border_l` paints at `left` and `border_r` at
        // `right - stroke`, so any single uniform shift applied to the whole assembly (which is
        // what this module used to do) necessarily anchors one edge choice and leaves the other a
        // column off. That happened to anchor the shapes which occur most often, which is why it
        // went unnoticed: the leftward `Diverging` exit, a real shape `layout_lanes` does emit, was
        // landing a column off the very lane line it is supposed to continue.
        for (kind, x_from, x_to) in ALL_SHAPES {
            for (lane_x, curve, join) in ends(kind, x_from, x_to) {
                if join != LaneJoin::ContinuesLane {
                    continue;
                }
                assert_eq!(
                    painted_vertical_column(&curve),
                    lane_x,
                    "{kind:?} {x_from:?}->{x_to:?}: a lane-continuing curve ({:?} edge) must paint \
                     its vertical stroke on the lane's own column",
                    curve.vertical
                );
            }
        }
    }

    #[test]
    fn a_dot_leaving_curve_paints_one_stroke_clear_of_its_lanes_own_line() {
        // The mirror case, and the reason this is not simply "anchor everything on `lane_x`". A
        // `LaneJoin::LeavesDot` curve is a *different* branch's line departing from (or arriving
        // at) this row's dot while `own_lane`'s own line runs straight through the very same rows.
        // Painting it on `lane_x` would erase a stroke's worth of that through-line; it belongs one
        // stroke to the side, and specifically the side the elbow travels, so it reads as leaving
        // the dot rather than crossing it.
        for (kind, x_from, x_to) in ALL_SHAPES {
            let rightward = x_to >= x_from;
            for (lane_x, curve, join) in ends(kind, x_from, x_to) {
                if join != LaneJoin::LeavesDot {
                    continue;
                }
                // The elbow's horizontal run is to the right of a `Left`-edged box and to the left
                // of a `Right`-edged one, so that is the side the stroke steps toward.
                let expected = match curve.vertical {
                    VerticalEdge::Left => lane_x + ELBOW_STROKE,
                    VerticalEdge::Right => lane_x - ELBOW_STROKE,
                };
                assert_eq!(
                    painted_vertical_column(&curve),
                    expected,
                    "{kind:?} {x_from:?}->{x_to:?} (rightward {rightward}): a dot-leaving curve \
                     must sit exactly one stroke clear of the lane's own line, on the side it \
                     travels"
                );
            }
        }
    }

    #[test]
    fn a_diverging_elbow_leaves_this_rows_dot_and_lands_on_the_other_lanes_line() {
        // Pins the `LaneJoin` assignment itself, which is what tells the two kinds apart now that
        // both use the same entry/exit border-edge pattern. `Diverging`'s `from_lane` is always
        // `own_lane` (`layout_lanes` Step 4), so its entry is the dot end.
        let geo = elbow_geometry(ElbowKind::Diverging, px(9.0), px(23.0), ROW_H);
        // The entry's own vertical stroke has to start under the dot's own disc, so no gap opens
        // between the dot and the line leaving it. It may start above the dot's centre - the dot is
        // painted last and hides that stretch - but never below the disc's own bottom edge, and
        // never so far above that it pokes out of the smallest dot this row can have.
        assert!(
            (ROW_H / 2.0 - SMALLEST_DOT / 2.0..=ROW_H / 2.0).contains(&geo.entry.top),
            "entry must start under the dot's own disc: {:?}",
            geo.entry.top
        );
        assert_eq!(geo.entry.horizontal, HorizontalEdge::Bottom);
        assert_eq!(geo.entry.vertical, VerticalEdge::Left);
        assert_eq!(geo.exit.horizontal, HorizontalEdge::Top);
        assert_eq!(geo.exit.vertical, VerticalEdge::Right);
        // The exit still reaches past the row's own edge, into the next row where `to_lane`'s own
        // full-height segment picks the line up - `render_graph_lane_canvas` clips that stretch
        // away rather than letting it paint over the neighbour.
        assert!(
            geo.exit.top + geo.exit.height > ROW_H,
            "the exit curve must reach the row's own bottom edge: {:?}",
            geo.exit.top + geo.exit.height
        );
    }

    #[test]
    fn a_converging_elbow_continues_the_ending_lane_and_lands_on_this_rows_dot() {
        // The mirror of the test above, and the shape this repository's own real history produces:
        // `own_lane` (lane 0) sits left of the ending lane. The entry curve must continue
        // `from_lane`'s own already-painted line - a previous single-corner version got this
        // backwards, confirmed by a real user report that the curve rendered disconnected from the
        // straight line it was supposed to continue.
        let geo = elbow_geometry(ElbowKind::Converging, px(23.0), px(9.0), ROW_H);
        assert_eq!(geo.entry.horizontal, HorizontalEdge::Bottom);
        assert_eq!(geo.entry.vertical, VerticalEdge::Right);
        assert_eq!(
            painted_vertical_column(&geo.entry),
            px(23.0),
            "the entry curve must continue from_lane's own already-painted line"
        );
        assert_eq!(geo.exit.horizontal, HorizontalEdge::Top);
        assert_eq!(geo.exit.vertical, VerticalEdge::Left);
        // The mirror of the Diverging entry above: the dot end has to finish under the dot's own
        // disc - at or past its centre, but not out the far side of the smallest dot.
        let exit_bottom = geo.exit.top + geo.exit.height;
        assert!(
            (ROW_H / 2.0..=ROW_H / 2.0 + SMALLEST_DOT / 2.0).contains(&exit_bottom),
            "the exit curve must land under own_lane's own dot: {exit_bottom:?}"
        );
        // And it must reach back past the row's own top edge, to where the ending lane's own
        // full-height segment in the row above left off - `render_graph_lane_canvas` clips that
        // stretch away rather than letting it paint over the neighbour.
        assert!(
            geo.entry.top < px(0.0),
            "the entry curve must reach the row's own top edge: {:?}",
            geo.entry.top
        );
    }

    #[test]
    fn a_wide_lane_gap_gets_a_real_straight_middle_segment_covering_each_curves_own_straight_run() {
        // Three lane steps apart (42px) is comfortably past 2*CURVE_SIZE (20px) - a real straight
        // segment must bridge the two curves, each end reaching exactly `RADIUS` *past* the
        // natural tangent point (where each curve's own arc ends and its own straight border
        // lead-in begins) and out to that curve's own outer edge (see `StraightSegment`'s own docs
        // for why: a real user report found a persistent, direction-flipping vertical
        // misalignment at this exact seam, traced back to a border-radius arc and a filled rect
        // being different rendering paths - covering each curve's own straight-run length with the
        // fill removes the border's own rendering from that stretch of the seam entirely).
        let geo = elbow_geometry(ElbowKind::Diverging, px(9.0), px(9.0 + 3.0 * 14.0), ROW_H);
        assert_eq!(geo.straight.left, geo.entry.left + RADIUS);
        assert_eq!(
            geo.straight.left + geo.straight.width,
            geo.exit.left + RADIUS
        );
        // Both ends of the bridge must sit on the row each curve actually *paints* its
        // horizontal stroke on, which for the bottom-edged entry curve is not its box edge - see
        // `all_three_elbow_pieces_paint_their_horizontal_stroke_on_the_very_same_pixel_row`.
        assert_eq!(geo.straight.top, painted_horizontal_row(&geo.entry));
        assert_eq!(geo.straight.top, painted_horizontal_row(&geo.exit));
    }

    #[test]
    fn the_straight_bridge_never_runs_past_the_far_curves_own_arc() {
        // The real regression test for a user-reported glitch on **one-lane-wide** elbows, seen at
        // both ends of a branch: "when a branch merges back into a branch that is just one below"
        // and "also happening at the start of branches". Two screenshots showed a 1px horizontal
        // stub dangling in mid-air past the corner, with the corner itself flattened.
        //
        // Adjacent lanes are `LANE_STEP` = 14px apart while the two curve boxes together want
        // `2 * CURVE_SIZE` = 20px, so they genuinely overlap and the honest bridge span is
        // *shorter* than in the wide-gap case. Flooring the width at `2 * RADIUS` (what this used
        // to do) then ran the fill 2px past where the far curve's arc begins, straight over the arc
        // and out the other side. The invariant that has to hold for every gap: the bridge starts
        // exactly where the near curve's arc ends and stops exactly where the far curve's arc
        // begins - never a pixel further, so it can neither paint over an arc nor stick out past
        // the lane line the arc turns onto.
        for (kind, x_from, x_to) in ALL_SHAPES_AND_GAPS {
            let geo = elbow_geometry(kind, x_from, x_to, ROW_H);
            let (near, far) = if x_to >= x_from {
                (&geo.entry, &geo.exit)
            } else {
                (&geo.exit, &geo.entry)
            };
            let label = format!("{kind:?} {x_from:?}->{x_to:?}");
            assert_eq!(
                geo.straight.left,
                near.right() - RADIUS,
                "{label}: the bridge must start where the near curve's own arc ends"
            );
            assert_eq!(
                geo.straight.left + geo.straight.width,
                far.left + RADIUS,
                "{label}: the bridge must stop where the far curve's own arc begins"
            );
            // The consequence that was actually visible on screen: the fill must never reach past
            // the column the far curve's own vertical stroke sits on.
            assert!(
                geo.straight.left + geo.straight.width
                    <= painted_vertical_column(far) + ELBOW_STROKE,
                "{label}: the bridge ran past the far curve's own vertical stroke - that is the \
                 reported dangling stub: bridge end {:?}, stroke column {:?}",
                geo.straight.left + geo.straight.width,
                painted_vertical_column(far)
            );
            assert!(
                geo.straight.width >= px(0.0),
                "{label}: a negative width would render as nothing at all: {:?}",
                geo.straight.width
            );
        }
    }

    #[test]
    fn adjacent_lanes_get_a_real_but_shorter_bridge_than_a_wide_gap() {
        // Pins the concrete numbers behind the invariant above, so a future change that quietly
        // reintroduces a minimum width has to face them. One lane step apart: the two boxes overlap
        // by `2 * CURVE_SIZE - LANE_STEP` = 6px, and the bridge that survives is 4px - real, but
        // less than the `2 * RADIUS` = 10px floor that used to be forced on it.
        let adjacent = elbow_geometry(ElbowKind::Diverging, px(9.0), px(23.0), ROW_H);
        assert_eq!(adjacent.straight.width, px(4.0));
        let wide = elbow_geometry(ElbowKind::Diverging, px(9.0), px(9.0 + 3.0 * 14.0), ROW_H);
        assert!(
            adjacent.straight.width < wide.straight.width,
            "an adjacent-lane bridge must be shorter than a wide-gap one, not clamped up to a \
             fixed minimum: {:?} vs {:?}",
            adjacent.straight.width,
            wide.straight.width
        );
    }

    /// The vertical span one curve's own *arc* sweeps: it always fills exactly an `ELBOW_RADIUS`
    /// corner square of the box, anchored at the bordered corner (see [`CurveBox`]). The rest of
    /// the box's bordered vertical edge is straight stroke.
    fn arc_span(curve: &CurveBox) -> (Pixels, Pixels) {
        match curve.horizontal {
            HorizontalEdge::Top => (curve.top, curve.top + RADIUS),
            HorizontalEdge::Bottom => {
                let bottom = curve.top + curve.height;
                (bottom - RADIUS, bottom)
            }
        }
    }

    #[test]
    fn every_arc_sweeps_entirely_inside_its_own_rows_box() {
        // The invariant `render_graph_lane_canvas`'s `overflow_hidden()` depends on, and the real
        // regression test for the reported "lines disappearing at elbow connections when hovering".
        //
        // Root cause: each row is its own `div()` and rows paint in order, so a later row's opaque
        // hover background painted over whatever an earlier row had spilled into its rectangle. The
        // fix clips each row's canvas to its own box - which is only lossless if the stretch being
        // clipped is pure *straight* vertical stroke, sitting on exactly the column the
        // neighbouring row's own full-height `LaneSegment` continues the line on. If an arc were
        // cut mid-sweep the two would not line up and every row boundary would show a kink, so this
        // is what `elbow_geometry`'s `waist_y` clamp exists to guarantee.
        for (kind, x_from, x_to) in ALL_SHAPES_AND_GAPS {
            let geo = elbow_geometry(kind, x_from, x_to, ROW_H);
            for (part, curve) in [("entry", geo.entry), ("exit", geo.exit)] {
                let (arc_top, arc_bottom) = arc_span(&curve);
                assert!(
                    arc_top >= px(0.0) && arc_bottom <= ROW_H,
                    "{kind:?} {x_from:?}->{x_to:?}: the {part} curve's arc sweeps \
                     {arc_top:?}..{arc_bottom:?}, outside its own row's 0..{ROW_H:?} box - the \
                     canvas clip would cut it mid-turn"
                );
            }
            assert!(
                geo.straight.top >= px(0.0) && geo.straight.top < ROW_H,
                "{kind:?} {x_from:?}->{x_to:?}: the bridge's own row {:?} must be inside the row",
                geo.straight.top
            );
        }
    }

    #[test]
    fn what_the_row_clip_removes_is_exactly_what_the_neighbouring_row_paints() {
        // The other half of the same argument: the part of an elbow that *does* fall outside the
        // row must be the far curve's straight vertical lead-out, painted on the very column that
        // lane's own line uses - so the neighbouring row's full-height segment resumes it pixel for
        // pixel and nothing is actually lost to the clip.
        for (kind, x_from, x_to) in ALL_SHAPES_AND_GAPS {
            let geo = elbow_geometry(kind, x_from, x_to, ROW_H);
            // `Diverging` spills out of the bottom via its exit, `Converging` out of the top via
            // its entry - the two ends that continue a lane rather than leaving the dot.
            let (spilling, lane_x, spill) = match kind {
                ElbowKind::Diverging => (
                    geo.exit,
                    x_to,
                    geo.exit.top + geo.exit.height - ROW_H, // past the bottom edge
                ),
                ElbowKind::Converging => (geo.entry, x_from, -geo.entry.top), // past the top edge
            };
            assert!(
                spill > px(0.0),
                "{kind:?} {x_from:?}->{x_to:?}: sanity check - this end really must spill out of \
                 the row, or the clip has nothing to remove and this test proves nothing"
            );
            assert_eq!(
                spill,
                CURVE_SIZE - RADIUS,
                "{kind:?} {x_from:?}->{x_to:?}: the spill must be exactly the curve's own straight \
                 lead-out, never any part of its arc"
            );
            assert_eq!(
                painted_vertical_column(&spilling),
                lane_x,
                "{kind:?} {x_from:?}->{x_to:?}: the clipped-away stroke must sit on that lane's own \
                 column, which the neighbouring row's own segment continues"
            );
        }
        // And that neighbouring segment really does run the full row, edge to edge - it is no
        // longer inset for an overshoot, because there is no longer an overshoot to inset for.
        let through = wt_core::graph::LaneSegment {
            lane: 1,
            starts_here: false,
            ends_here: false,
            dashed: false,
        };
        assert_eq!(lane_segment_span(&through, ROW_H), (px(0.0), ROW_H));
    }

    #[test]
    fn a_lane_that_starts_or_ends_here_gets_exactly_one_half_height_stub() {
        // The plain half/half cases, unchanged: anchored to the correct edge, one element each,
        // never two stacked halves (design spec §2).
        let mut segment = wt_core::graph::LaneSegment {
            lane: 1,
            starts_here: true,
            ends_here: false,
            dashed: false,
        };
        assert_eq!(
            lane_segment_span(&segment, ROW_H),
            (ROW_H / 2.0, ROW_H / 2.0),
            "a lane starting here runs from the dot down to the row's bottom edge"
        );
        segment.starts_here = false;
        segment.ends_here = true;
        assert_eq!(
            lane_segment_span(&segment, ROW_H),
            (px(0.0), ROW_H / 2.0),
            "a lane ending here runs from the row's top edge down to the dot"
        );
    }

    #[test]
    fn no_elbow_piece_ever_reaches_past_the_lane_canvas_own_width() {
        // GPUI's content mask is a single rectangle, so `render_graph_lane_canvas`'s
        // `overflow_hidden()` bounds x as well as y. That is only safe while every piece stays
        // inside the canvas' own width - otherwise the clip added for the hover bug would silently
        // amputate the rightmost elbow instead. Checked against real widths, over every lane pair a
        // repository with that many lanes can produce.
        for lane_count in 2..=12usize {
            let width = graph_lane_canvas_width(lane_count);
            for from_lane in 0..lane_count {
                for to_lane in 0..lane_count {
                    // A same-lane elbow is a degenerate shape `layout_lanes` never emits (an elbow
                    // exists precisely because a line changes lane); its two curves would sit on top
                    // of each other and reach a stroke past the lane they share. Left out here
                    // rather than widening the canvas for a case that cannot occur.
                    if from_lane == to_lane {
                        continue;
                    }
                    for kind in [ElbowKind::Diverging, ElbowKind::Converging] {
                        let geo = elbow_geometry(kind, lane_x(from_lane), lane_x(to_lane), ROW_H);
                        let right = geo
                            .entry
                            .right()
                            .max(geo.exit.right())
                            .max(geo.straight.left + geo.straight.width);
                        assert!(
                            geo.entry.left >= px(0.0) && geo.exit.left >= px(0.0),
                            "{kind:?} lane {from_lane}->{to_lane}: a piece started left of the \
                             canvas ({:?}, {:?})",
                            geo.entry.left,
                            geo.exit.left
                        );
                        assert!(
                            right <= width,
                            "{kind:?} lane {from_lane}->{to_lane} of {lane_count}: a piece reached \
                             {right:?}, past the canvas' own {width:?} - the row clip would cut it"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn converging_and_diverging_always_occupy_opposite_row_halves() {
        // Both kinds share the same entry/exit border-edge pattern (Bottom then Top) - the S-curve
        // shape itself is identical either way, and what actually tells the two kinds apart is
        // *where* in the row that shape sits: `Diverging` always in the bottom half (at/after the
        // dot), `Converging` always in the top half (at/before the dot). A `Converging` elbow
        // rendered at `Diverging`'s vertical position (or vice versa) is exactly the "looks just
        // as broken as the original bug" failure mode an earlier adversarial audit was asked to
        // rule out - just via position now, since border-edge choice alone can no longer tell them
        // apart in this two-curve design.
        // Stated on the *waist* - the single row all three pieces paint their horizontal stroke on,
        // and the one thing that is unambiguously on one side of the dot or the other. The curve
        // boxes themselves each straddle the dot by design (their dot end has to finish underneath
        // it, see `a_diverging_elbow_leaves_this_rows_dot_and_lands_on_the_other_lanes_line`), so a
        // box edge is the wrong thing to measure this with.
        for (x_from, x_to) in [(px(9.0), px(23.0)), (px(23.0), px(9.0)), (px(9.0), px(9.0))] {
            let diverging = elbow_geometry(ElbowKind::Diverging, x_from, x_to, ROW_H);
            let converging = elbow_geometry(ElbowKind::Converging, x_from, x_to, ROW_H);
            assert!(
                diverging.straight.top > ROW_H / 2.0,
                "Diverging's waist must sit below the dot: {:?}",
                diverging.straight.top
            );
            assert!(
                converging.straight.top < ROW_H / 2.0,
                "Converging's waist must sit above the dot: {:?}",
                converging.straight.top
            );
            // And each kind's own far curve must reach out of the row on the matching side, since
            // that is the row whose lane segment continues the line.
            assert!(
                diverging.exit.top + diverging.exit.height > ROW_H,
                "Diverging must reach into the row below"
            );
            assert!(
                converging.entry.top < px(0.0),
                "Converging must reach into the row above"
            );
        }
    }

    #[test]
    fn a_diverging_elbows_color_follows_the_branch_merged_in_not_the_branch_it_lands_in() {
        // from_lane is always own_lane (the branch merged *into*) for Diverging - a real user
        // report found the connector was being colored with that landing branch's color instead of
        // the merged-in branch's own color (to_lane).
        assert_eq!(elbow_color_lane(ElbowKind::Diverging, 0, 2), 2);
        assert_eq!(elbow_color_lane(ElbowKind::Diverging, 2, 0), 0);
    }

    #[test]
    fn a_converging_elbows_color_follows_the_ending_lane_it_continues() {
        // Converging has no "merged into" branch at all - from_lane (the ending lane) is already
        // the color that continues its own already-painted `ends_here` stub from the row above.
        assert_eq!(elbow_color_lane(ElbowKind::Converging, 1, 0), 1);
        assert_eq!(elbow_color_lane(ElbowKind::Converging, 0, 1), 0);
    }
}

/// Real, paint-based coverage for `render_graph_lane_canvas`'s two elbow shapes
/// ([`wt_core::graph::ElbowKind`]) - a real regression test for the git graph tab's "start of
/// branches just are not connected at all and end of branches don't merge correctly" bug report:
/// a `Converging` elbow (two branches sharing an ancestor with no merge commit) was silently
/// dropped entirely (an empty `elbows` vec, a dangling stub with nothing connecting it), and once
/// added, needed to render as the geometric *mirror* of the already-correct `Diverging` case (top
/// half of the row, not the bottom) - a `Converging` elbow painting in the bottom half (or vice
/// versa) would look just as broken as the original bug. Uses `cx.simulate_event`/`debug_bounds`
/// real paint measurements (via `render_graph_lane_canvas`'s own `debug_selector` tags), not a
/// direct call into the pure layout function - `wt_core::graph`'s own unit tests already cover
/// that; this covers the *rendering* half only `crate::graph_view` owns.
#[cfg(test)]
mod graph_elbow_render_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Entity, TestAppContext};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Real timestamps (like `wt_core::graph`'s own `commit_at` test helper) so the time-sorted
    /// walk produces a deterministic row order across runs, rather than depending on the test
    /// process happening to cross a wall-clock second between two git invocations.
    fn commit_at(dir: &std::path::Path, file: &str, contents: &str, message: &str, unix: i64) {
        std::fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        let date = format!("{unix} +0000");
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("failed to spawn git commit");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
    }

    /// Where a painted curve's own vertical stroke must land, derived from the elbow's own lanes
    /// and the documented painting rules only - deliberately *not* by calling `elbow_geometry`,
    /// so this is a real independent check of it rather than a restatement.
    ///
    /// `is_entry` picks which end. Returns `(expected_stroke_x, stroke_x_from(bounds))`.
    fn expected_stroke_x(
        canvas_x: Pixels,
        elbow: &wt_core::graph::Elbow,
        is_entry: bool,
    ) -> (Pixels, fn(gpui::Bounds<Pixels>) -> Pixels) {
        let rightward = lane_x(elbow.to_lane) >= lane_x(elbow.from_lane);
        // The entry curve's vertical faces the other lane; the exit curve's faces back.
        let vertical_is_left = if is_entry { rightward } else { !rightward };
        // `Diverging` leaves this row's dot on its entry; `Converging` on its exit.
        let leaves_dot = match elbow.kind {
            wt_core::graph::ElbowKind::Diverging => is_entry,
            wt_core::graph::ElbowKind::Converging => !is_entry,
        };
        let lane = if is_entry {
            elbow.from_lane
        } else {
            elbow.to_lane
        };
        let mut x = canvas_x + lane_x(lane);
        if leaves_dot {
            x = if vertical_is_left {
                x + ELBOW_STROKE
            } else {
                x - ELBOW_STROKE
            };
        }
        let read: fn(gpui::Bounds<Pixels>) -> Pixels = if vertical_is_left {
            |b| b.origin.x
        } else {
            |b| b.origin.x + b.size.width - ELBOW_STROKE
        };
        (x, read)
    }

    /// A real, unmerged shared-ancestor history matching the exact shape found in this
    /// repository's own real row 9 (commit `ac8e6cd`) that this whole fix was found from: `root`
    /// is reached by *three* separate tips - `main` continues past it with one more real commit
    /// (`main2`, so `root` is reached by ordinary first-parent continuation, landing `root` on
    /// that *same*, already-established lane rather than on either `a` or `b`'s lane), while `a`
    /// and `b` are two entirely independent tips whose own first parent is `root` directly, with
    /// no merge commit involved anywhere. `root`'s row must therefore end up with real Converging
    /// elbows for *both* `a`'s and `b`'s now-ending lanes - not one, not zero.
    fn seed_converging_history(dir: &std::path::Path) {
        init_repo(dir);
        commit_at(dir, "root.txt", "1", "root", 1_700_000_000);
        git(dir, &["branch", "a"]);
        git(dir, &["branch", "b"]);
        git(dir, &["checkout", "a"]);
        commit_at(dir, "a.txt", "1", "a1", 1_700_000_100);
        git(dir, &["checkout", "b"]);
        commit_at(dir, "b.txt", "1", "b1", 1_700_000_200);
        git(dir, &["checkout", "main"]);
        commit_at(dir, "main2.txt", "1", "main2", 1_700_000_300);
    }

    fn open_seeded(
        cx: &mut TestAppContext,
        seed: impl FnOnce(&std::path::Path),
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        (repo, app, cx)
    }

    #[gpui::test]
    fn a_converging_elbow_paints_in_the_rows_top_half_on_the_ending_lanes_side(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx, seed_converging_history);

        // Row order newest-first by real commit time: main2, b1, a1, root - `root` (row 3) is the
        // real shared ancestor with two independent lanes (a's and b's) ending on it.
        let root_row_index = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph
                .rows
                .iter()
                .position(|row| row.commit.subject == "root")
                .expect("root row present")
        });
        let elbow_kinds = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph.rows[root_row_index].elbows.clone()
        });
        assert_eq!(
            elbow_kinds.len(),
            2,
            "root must get two real Converging elbows, one per ending lane, not an empty vec: \
             {elbow_kinds:?}"
        );
        assert!(
            elbow_kinds
                .iter()
                .all(|elbow| elbow.kind == wt_core::graph::ElbowKind::Converging),
            "a shared-ancestor row with no merge commit of its own must only ever produce \
             Converging elbows: {elbow_kinds:?}"
        );

        // `TestAppContext::debug_bounds` requires a `&'static str` selector; the row/elbow index
        // is only known at runtime, so leak the formatted string for the test's lifetime (a real,
        // process-lifetime-bounded test run, never freed - an accepted tradeoff for test code
        // that needs a dynamically-built `&'static str`, not a production code path).
        // The row's *lane canvas* (not the outer row div) - row 0 is auto-selected on load and
        // gets an extra `border_l_2()`, shifting its content 2px right of the row's own origin;
        // anchoring off the lane canvas's own painted bounds avoids depending on whether this
        // particular row happens to be the selected one.
        let row_selector: &'static str =
            Box::leak(format!("graph-row-{root_row_index}-lane-canvas").into_boxed_str());
        let row_bounds = cx
            .debug_bounds(row_selector)
            .expect("root row's lane canvas must be painted");
        let row_center_y = row_bounds.origin.y + theme::graph::ROW / 2.0;

        let touches = |edge: Pixels, target: Pixels| (edge - target).abs() < px(0.5);

        for (elbow_index, elbow) in elbow_kinds.iter().enumerate() {
            let entry_selector: &'static str = Box::leak(
                format!("graph-row-{root_row_index}-elbow-{elbow_index}-converging-entry")
                    .into_boxed_str(),
            );
            let exit_selector: &'static str = Box::leak(
                format!("graph-row-{root_row_index}-elbow-{elbow_index}-converging-exit")
                    .into_boxed_str(),
            );
            let entry_bounds = cx
                .debug_bounds(entry_selector)
                .unwrap_or_else(|| panic!("{entry_selector} must be painted"));
            let exit_bounds = cx
                .debug_bounds(exit_selector)
                .unwrap_or_else(|| panic!("{exit_selector} must be painted"));
            // Measured on the *waist* - the painted horizontal run itself, which is the piece that
            // is unambiguously on one side of the dot or the other. Each curve box straddles the
            // dot by design (its dot end has to finish underneath it), so a box edge is the wrong
            // thing to read a "which half" claim off.
            let straight_selector: &'static str = Box::leak(
                format!("graph-row-{root_row_index}-elbow-{elbow_index}-converging-straight")
                    .into_boxed_str(),
            );
            let straight_bounds = cx
                .debug_bounds(straight_selector)
                .unwrap_or_else(|| panic!("{straight_selector} must be painted"));
            assert!(
                straight_bounds.origin.y < row_center_y,
                "a Converging elbow's horizontal run must sit above the row's vertical centre \
                 {row_center_y:?}, not the bottom half where a Diverging elbow would render: was \
                 {:?}",
                straight_bounds.origin.y
            );
            assert!(
                entry_bounds.origin.y < row_center_y,
                "a Converging elbow's entry curve must start in the row's top half (entry top \
                 {:?} must be above the row's vertical centre {row_center_y:?}), not the bottom \
                 half where a Diverging elbow would render",
                entry_bounds.origin.y
            );
            // The exit lands *on* the dot, so it may finish a little past the centre - but never
            // further than the dot's own disc hides.
            assert!(
                exit_bounds.origin.y + exit_bounds.size.height
                    <= row_center_y + theme::graph::DOT_COMMIT / 2.0,
                "a Converging elbow's exit curve must finish under the row's own dot, not extend \
                 into the bottom half like a Diverging elbow would: bottom was {:?}, centre \
                 {row_center_y:?}",
                exit_bounds.origin.y + exit_bounds.size.height
            );
            // Real x coverage against the *painted* stroke, not merely the box: an adversarial
            // audit found the y-only assertions above cannot tell a swapped edge/corner choice
            // apart, since a box's position and size stay the same either way. This pins which
            // column each curve's own border actually lands on - the entry continuing the ending
            // lane's own line, the exit stepping one stroke clear of `own_lane`'s through-line as
            // it lands on the dot.
            for (is_entry, bounds) in [(true, entry_bounds), (false, exit_bounds)] {
                let (expected, read) = expected_stroke_x(row_bounds.origin.x, elbow, is_entry);
                assert!(
                    touches(read(bounds), expected),
                    "elbow {elbow_index}'s {} curve painted its vertical stroke at {:?}, expected \
                     {expected:?} (bounds {:?} w {:?})",
                    if is_entry { "entry" } else { "exit" },
                    read(bounds),
                    bounds.origin.x,
                    bounds.size.width
                );
            }
        }
    }

    #[gpui::test]
    fn a_diverging_elbow_paints_in_the_rows_bottom_half(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded(cx, |dir| {
            init_repo(dir);
            commit_at(dir, "base.txt", "1", "base", 1_700_000_000);
            git(dir, &["checkout", "-b", "feature"]);
            commit_at(dir, "feature.txt", "1", "feature work", 1_700_000_100);
            git(dir, &["checkout", "main"]);
            let date = "1700000200 +0000";
            let output = std::process::Command::new("git")
                .current_dir(dir)
                .args([
                    "merge",
                    "--no-ff",
                    "feature",
                    "-m",
                    "Merge branch 'feature'",
                ])
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .output()
                .expect("failed to spawn git merge");
            assert!(
                output.status.success(),
                "git merge failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        });

        let merge_row_index = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph
                .rows
                .iter()
                .position(|row| row.commit.subject == "Merge branch 'feature'")
                .expect("merge row present")
        });
        let elbow_kinds = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph.rows[merge_row_index].elbows.clone()
        });
        assert_eq!(
            elbow_kinds,
            vec![wt_core::graph::Elbow {
                from_lane: 0,
                to_lane: 1,
                kind: wt_core::graph::ElbowKind::Diverging,
            }],
            "a real 2-parent merge with no unrelated converging lane must produce exactly one \
             real Diverging elbow: {elbow_kinds:?}"
        );

        // See the Converging test above for why this anchors off the lane canvas, not the row.
        let row_selector: &'static str =
            Box::leak(format!("graph-row-{merge_row_index}-lane-canvas").into_boxed_str());
        let row_bounds = cx
            .debug_bounds(row_selector)
            .expect("merge row's lane canvas must be painted");
        let row_center_y = row_bounds.origin.y + theme::graph::ROW / 2.0;
        let touches = |edge: Pixels, target: Pixels| (edge - target).abs() < px(0.5);
        let entry_selector: &'static str = Box::leak(
            format!("graph-row-{merge_row_index}-elbow-0-diverging-entry").into_boxed_str(),
        );
        let exit_selector: &'static str = Box::leak(
            format!("graph-row-{merge_row_index}-elbow-0-diverging-exit").into_boxed_str(),
        );
        let entry_bounds = cx
            .debug_bounds(entry_selector)
            .unwrap_or_else(|| panic!("{entry_selector} must be painted"));
        let exit_bounds = cx
            .debug_bounds(exit_selector)
            .unwrap_or_else(|| panic!("{exit_selector} must be painted"));
        // Measured on the waist, for the reason spelled out in the Converging test above.
        let straight_selector: &'static str = Box::leak(
            format!("graph-row-{merge_row_index}-elbow-0-diverging-straight").into_boxed_str(),
        );
        let straight_bounds = cx
            .debug_bounds(straight_selector)
            .unwrap_or_else(|| panic!("{straight_selector} must be painted"));
        assert!(
            straight_bounds.origin.y > row_center_y,
            "a Diverging elbow's horizontal run must sit below the row's vertical centre \
             {row_center_y:?}, not the top half where a Converging elbow would render: was {:?}",
            straight_bounds.origin.y
        );
        // The entry leaves the dot, so it starts under the dot's own disc - at or above the centre,
        // but never further above than that disc hides.
        assert!(
            entry_bounds.origin.y >= row_center_y - theme::graph::DOT_COMMIT / 2.0
                && entry_bounds.origin.y <= row_center_y,
            "a Diverging elbow's entry curve must start under the row's own dot: top was {:?}, \
             centre {row_center_y:?}",
            entry_bounds.origin.y
        );
        assert!(
            exit_bounds.origin.y + exit_bounds.size.height > row_center_y,
            "a Diverging elbow's exit curve must extend into the row's bottom half: bottom was \
             {:?}, centre was {row_center_y:?}",
            exit_bounds.origin.y + exit_bounds.size.height
        );
        // Real x coverage against the *painted* stroke, mirroring the Converging test above: here
        // the entry leaves this row's own dot and the exit continues `to_lane`'s own line.
        let elbow = &elbow_kinds[0];
        for (is_entry, bounds) in [(true, entry_bounds), (false, exit_bounds)] {
            let (expected, read) = expected_stroke_x(row_bounds.origin.x, elbow, is_entry);
            assert!(
                touches(read(bounds), expected),
                "the {} curve painted its vertical stroke at {:?}, expected {expected:?}",
                if is_entry { "entry" } else { "exit" },
                read(bounds)
            );
        }
    }

    /// Real regression for a real user report against the two tests above: "the vertical lines
    /// are going too far at the end and at the start" - a lane that starts (or ends) here *and*
    /// has a real elbow at this same row was getting both a plain `starts_here`/`ends_here` stub
    /// *and* the elbow's own vertical stroke painted over the exact same pixels, at the exact
    /// same x - not visually distinguishable from one continuous line running further than it
    /// should. Reuses the same real Diverging-merge fixture as the test above: lane 1 both
    /// `starts_here` at the merge row (a real `new_lane_segments` push, `layout_lanes` Step 4)
    /// and has a real Diverging elbow (`to_lane: 1`) there - the plain segment for lane 1 must
    /// not be painted at all once the elbow already covers it.
    #[gpui::test]
    fn a_lane_with_a_diverging_elbow_does_not_also_paint_a_plain_starts_here_stub(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx, |dir| {
            init_repo(dir);
            commit_at(dir, "base.txt", "1", "base", 1_700_000_000);
            git(dir, &["checkout", "-b", "feature"]);
            commit_at(dir, "feature.txt", "1", "feature work", 1_700_000_100);
            git(dir, &["checkout", "main"]);
            let date = "1700000200 +0000";
            let output = std::process::Command::new("git")
                .current_dir(dir)
                .args([
                    "merge",
                    "--no-ff",
                    "feature",
                    "-m",
                    "Merge branch 'feature'",
                ])
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .output()
                .expect("failed to spawn git merge");
            assert!(output.status.success());
        });

        let merge_row_index = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph
                .rows
                .iter()
                .position(|row| row.commit.subject == "Merge branch 'feature'")
                .expect("merge row present")
        });
        let lane_1_starts_here = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph.rows[merge_row_index]
                .lane_segments
                .iter()
                .any(|s| s.lane == 1 && s.starts_here)
        });
        assert!(
            lane_1_starts_here,
            "sanity check: lane 1 must really start_here at the merge row, or this test would \
             pass for the wrong reason"
        );

        let selector: &'static str =
            Box::leak(format!("graph-row-{merge_row_index}-segment-1").into_boxed_str());
        assert!(
            cx.debug_bounds(selector).is_none(),
            "a plain starts_here stub for lane 1 must not be painted at all once its Diverging \
             elbow already covers that exact same span - painting both doubles up on the same \
             pixels, which is exactly the reported \"line goes too far\" bug"
        );

        // The elbow itself must still be there - this isn't testing that lane 1 lost its
        // connection entirely, only that it's represented once, not twice. The entry curve alone
        // is enough to prove the elbow rendered at all (every elbow always paints one).
        let elbow_selector: &'static str = Box::leak(
            format!("graph-row-{merge_row_index}-elbow-0-diverging-entry").into_boxed_str(),
        );
        assert!(
            cx.debug_bounds(elbow_selector).is_some(),
            "the Diverging elbow itself must still be painted - lane 1's connection must not be \
             lost, only de-duplicated"
        );
    }

    /// The layout fact `render_graph_lane_canvas`'s `overflow_hidden()` rests on, and the real
    /// paint-level regression for the "lines disappearing at elbow connections when hovering"
    /// report. That clip is what stops an elbow's straight lead-out from spilling into the next
    /// row's rectangle, where that row's own opaque hover background - painted later, since rows
    /// are siblings drawn in order - would cover it.
    ///
    /// Clipping is only *lossless* because consecutive lane canvases tile exactly: each is `ROW`
    /// tall and each sits `ROW` below the last, so the pixel row where one canvas' clip cuts the
    /// line is the very pixel row where the next canvas' own full-height `LaneSegment` resumes it.
    /// A gap or an overlap here would turn the clip into a visible dash (or bring back the doubled
    /// line an earlier round fixed), and neither is visible from geometry alone - it depends on the
    /// real flex layout of a row whose `border_b_1()` makes its content box shorter than `ROW`.
    #[gpui::test]
    fn consecutive_lane_canvases_tile_exactly_so_the_row_clip_loses_no_line(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx, seed_converging_history);
        let row_count = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph.rows.len()
        });
        assert!(
            row_count >= 4,
            "sanity check: this fixture must produce several real rows to compare, got {row_count}"
        );

        let mut previous: Option<gpui::Bounds<Pixels>> = None;
        for index in 0..row_count {
            let selector: &'static str =
                Box::leak(format!("graph-row-{index}-lane-canvas").into_boxed_str());
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} must be painted"));
            assert_eq!(
                bounds.size.height,
                theme::graph::ROW,
                "row {index}'s lane canvas must be exactly one row tall - the clip box is this \
                 box, so anything else clips at the wrong pixel"
            );
            if let Some(previous) = previous {
                assert_eq!(
                    bounds.origin.y - previous.origin.y,
                    theme::graph::ROW,
                    "row {index}'s lane canvas must start exactly where row {}'s ends, or the \
                     clipped-off lead-out and the next row's own segment do not meet",
                    index - 1
                );
                assert_eq!(
                    bounds.origin.x, previous.origin.x,
                    "every lane canvas must share one x origin, or lane columns would not line up \
                     across the clip boundary"
                );
            }
            previous = Some(bounds);
        }
    }
}

/// Real regression coverage for a row's selection border never shifting its own content - the
/// user-reported "when we click on a line, don't move the line, just highlight it" bug.
/// `render_graph_row` used to apply `.border_l_2()` only `.when(selected, ...)`, so selecting a
/// row added 2px of border-box inset that was not there before, pushing every child (lane
/// canvas, ref chips, subject, ...) 2px to the right - a visible jump on click. Mirrors
/// `crate::code_surface::editing`'s own `assert_eq!(a.size.width, b.size.width, ...)` bounds-
/// comparison pattern and `crate::merge::flow`'s "capture `debug_bounds` before and after a real
/// state-changing action, on the same live entity" shape - not a fresh `TestAppContext` per
/// state, since the bug is specifically about the *same* row moving when its own selection
/// state flips, not about two different rows differing.
#[cfg(test)]
mod graph_selection_render_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Entity, TestAppContext};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Two real commits - `build_graph` yields exactly two rows (indices 0..=1, newest first).
    /// Row 0 is auto-selected on load (`AdeApp`'s own `load_graph`), so row 1 starts genuinely
    /// unselected - exactly the row this test drives through a real selection change.
    fn seed_two_commits(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        std::fs::write(dir.join("a.txt"), "1\n").expect("write a.txt");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "2\n").expect("write a.txt");
        git(dir, &["commit", "-am", "second"]);
    }

    fn open_seeded_graph(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_two_commits(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        (repo, app, cx)
    }

    #[gpui::test]
    fn selecting_a_row_never_shifts_its_own_lane_canvas(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.selected_row),
            Some(0),
            "premise: row 0 is auto-selected on load, so row 1 starts genuinely unselected"
        );

        let bounds_unselected = cx
            .debug_bounds("graph-row-1-lane-canvas")
            .expect("row 1's lane canvas must be painted while unselected");

        app.update(cx, |app, cx| {
            app.select_graph_row(1, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.selected_row),
            Some(1),
            "premise: row 1 really is selected now"
        );

        let bounds_selected = cx
            .debug_bounds("graph-row-1-lane-canvas")
            .expect("row 1's lane canvas must be painted while selected");

        assert_eq!(
            bounds_unselected.origin.x, bounds_selected.origin.x,
            "selecting a row must never shift its own content to the right - the row's real \
             `.border_l_2()` reserves its 2px unconditionally, only its colour should toggle \
             (unselected: {:?}, selected: {:?})",
            bounds_unselected, bounds_selected
        );
        assert_eq!(
            bounds_unselected.size, bounds_selected.size,
            "selecting a row must never resize its own content (unselected: {:?}, selected: \
             {:?})",
            bounds_unselected, bounds_selected
        );
    }
}

/// Real focus-handling regression coverage for the git graph tab, mirroring `crate::root::focus`'s
/// own `*_focus_tests` modules (`palette_focus_tests`, `settings_focus_tests`, `code_focus_tests`).
/// This feature had none at all until an adversarial audit found five distinct reachable
/// dangling-focus paths through it, all now fixed in `AdeApp::open_git_graph`/`leave_graph_tab`
/// above. Positive assertions throughout (`assert_eq!` against a real, specific handle), not just
/// "the wrong thing didn't happen": a genuinely dangling `Window::focus` would still pass a bare
/// `assert_ne!`, which is exactly the false-negative shape `crate::root::focus`'s own module docs
/// warn about.
#[cfg(test)]
mod graph_focus_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Entity, Focusable, TestAppContext};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A real one-commit repo (so `AdeApp::load_graph` succeeds and the Branches panel's filter
    /// row - only rendered once a `Graph` is actually loaded - really paints) plus a real, already-
    /// open, already-focused shell agent (`AdeApp::new_with_settings`'s own default), the way
    /// every `*_focus_tests` helper in this crate seeds its window.
    fn open_seeded(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        (repo, app, cx)
    }

    fn agent_pane_handle(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
    ) -> gpui::FocusHandle {
        app.update(cx, |app, cx| {
            app.agents
                .active()
                .expect("the default agent")
                .pane
                .focus_handle(cx)
        })
    }

    #[gpui::test]
    fn open_git_graph_focuses_the_graph_view_from_a_fresh_window(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();

        let (focused, graph_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.graph_focus_handle.clone())
        });
        assert_eq!(
            focused.as_ref(),
            Some(&graph_handle),
            "opening the graph tab from a fresh window must move real focus onto its own \
             handle"
        );
        assert!(app.read_with(cx, |app, _| app.graph_tab_open && app.graph_tab_active));
    }

    /// Regression: `NewGitGraph` was bound to `mod+shift+G` in `crate::default_key_bindings` but
    /// `root::mod::AdeApp::render`'s `.on_action` chain never registered a handler for it, so the
    /// real dispatch path (walked via `cx.dispatch_action`, exactly like a live keystroke) silently
    /// no-opped - the palette's "Open git graph" row called `Self::open_git_graph` directly and
    /// masked the gap. `cx.dispatch_action` (not a direct `app.open_git_graph(..)` call) is the
    /// point: it proves the action reaches the handler through the same on_action chain a real
    /// keystroke uses, not merely that the handler function itself works.
    #[gpui::test]
    fn mod_shift_g_actually_opens_the_graph_tab_through_real_action_dispatch(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx);
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        cx.dispatch_action(NewGitGraph);
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.graph_tab_open && app.graph_tab_active),
            "mod+shift+G must open the graph tab through the real on_action dispatch path"
        );
    }

    /// Regression for a real, adversarial-audit-found gap (CRITICAL C2): opening the graph tab
    /// while a file tab was focused cleared `open_change` (unrendering the code surface) but only
    /// swept `tree_focus_handle`, not `code_focus_handle` - so `graph_focus`'s captured return
    /// target could be a handle that had *already* stopped being rendered by the time it was
    /// captured, dangling the moment the graph tab later closed back to it.
    #[gpui::test]
    fn opening_the_graph_tab_with_a_file_open_never_captures_the_unrendered_code_focus_handle(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded(cx);
        let file_path = repo.path().join("a.txt");

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        let (focused, code_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.code_focus_handle.clone())
        });
        assert_eq!(
            focused.as_ref(),
            Some(&code_handle),
            "premise: the file view really is focused before the graph tab opens"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_none()),
            "premise: opening the graph tab really did unrender the code surface"
        );

        // Switching back to the (already-active, unchanged) agent must land real focus on its
        // real pane - never on `code_focus_handle`, which is not part of this frame at all.
        let agent_id = app.read_with(cx, |app, _| app.agents.active_id().expect("an agent"));
        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_id, window, cx);
        });
        cx.run_until_parked();

        let pane_handle = agent_pane_handle(&app, cx);
        let focused = app.update_in(cx, |_app, window, cx| window.focused(cx));
        assert_eq!(
            focused.as_ref(),
            Some(&pane_handle),
            "focus must land on the real agent pane, not dangle on the unrendered \
             code_focus_handle the graph tab's own OverlayFocus could otherwise have captured"
        );
    }

    /// Regression for a real, adversarial-audit-found gap (CRITICAL C1): `leave_graph_tab` only
    /// checked `graph_focus_handle`, not the Branches panel's own real text-input surface
    /// (`graph_state.branches_filter_focus_handle`), which is only rendered while the graph tab
    /// is active and can independently hold real keyboard focus.
    #[gpui::test]
    fn leaving_the_graph_tab_from_the_branches_filter_lands_on_the_real_agent_pane(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_graph_right_panel(GraphRightPanel::Branches, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.graph_state.branches_filter_focus_handle, cx);
        });
        cx.run_until_parked();

        let (focused, filter_handle) = app.update_in(cx, |app, window, cx| {
            (
                window.focused(cx),
                app.graph_state.branches_filter_focus_handle.clone(),
            )
        });
        assert_eq!(
            focused.as_ref(),
            Some(&filter_handle),
            "premise: the Branches filter box really is focused"
        );

        let agent_id = app.read_with(cx, |app, _| app.agents.active_id().expect("an agent"));
        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_id, window, cx);
        });
        cx.run_until_parked();

        let pane_handle = agent_pane_handle(&app, cx);
        let focused = app.update_in(cx, |_app, window, cx| window.focused(cx));
        assert_eq!(
            focused.as_ref(),
            Some(&pane_handle),
            "focus must land on the real agent pane, not dangle on the Branches filter's \
             now-unrendered handle"
        );
        assert!(
            !app.read_with(cx, |app, _| app.graph_tab_active),
            "the graph tab must have been genuinely left, not merely re-focused"
        );
    }

    /// Closing the graph tab's `×` while some *other* tab is showing (it is open but not active)
    /// must be a real no-op focus-wise - a second real, adversarial-audit-style scenario:
    /// `leave_graph_tab`'s own early-return for `!graph_tab_active` is what this proves.
    #[gpui::test]
    fn closing_an_inactive_graph_tab_does_not_touch_focus(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        // Spawning a new agent (e.g. from the title bar's Agent menu) does *not* itself
        // switch away from the graph tab - it stays open *and* active, exactly like Settings
        // stays open across the same gesture. Explicitly selecting an agent tab is what leaves
        // it, the same real user action `select_agent`'s own click handler performs.
        let agent_id = app.read_with(cx, |app, _| app.agents.active_id().expect("an agent"));
        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_id, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.graph_tab_open && !app.graph_tab_active),
            "premise: the graph tab is open but an agent tab took over as active"
        );

        let before = app.update_in(cx, |_app, window, cx| window.focused(cx));
        app.update_in(cx, |app, window, cx| {
            app.close_git_graph_tab(window, cx);
        });
        cx.run_until_parked();
        let after = app.update_in(cx, |_app, window, cx| window.focused(cx));

        assert_eq!(
            before, after,
            "closing an inactive graph tab must not move focus at all"
        );
        assert!(!app.read_with(cx, |app, _| app.graph_tab_open));
    }

    #[gpui::test]
    fn middle_clicking_the_graph_tab_closes_it_like_every_other_tab_kind(cx: &mut TestAppContext) {
        // A real user report: the graph tab was the one tab kind that didn't support middle-click
        // close (GitHub issue #26 already wired this for file and session tabs via
        // `on_mouse_down(MouseButton::Middle, ...)` - the graph tab's own `render_graph_tab` had
        // simply never been given the same treatment).
        let (_repo, app, cx) = open_seeded(cx);
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.graph_tab_open));

        let tab_bounds = cx
            .debug_bounds("graph-tab")
            .expect("the graph tab must be painted while open");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Middle,
            position: tab_bounds.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.graph_tab_open),
            "middle-clicking the graph tab must close it, same as a file or session tab"
        );
    }
}
