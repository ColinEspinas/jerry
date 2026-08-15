//! The real GPUI surface for the git graph tab - tab strip entry, toolbar, lane canvas + row
//! list, row `⋯` menu, Push `▾` menu, the Commit/Branches right panel and that panel's own branch
//! right-click menu - plus the `impl AdeApp` glue that opens/closes/loads it. See `super`'s module
//! docs for scope.

use super::*;
use crate::root::widgets::{
    menu_popover_chrome, modal_scrim_bg, render_sidebar_message, render_status_letter, SimpleInput,
};
use crate::settings::widgets;
use crate::sidebar::changes;
use crate::text_history::TextField;
use crate::work_surface::render::{render_dropdown_menu_row, DraggedTab, TabChromeArgs};
use gpui::{uniform_list, KeyDownEvent, Pixels};
use std::ops::Range;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wt_core::graph::{DotKind, ElbowKind, Graph, GraphRow, GraphScope, RefKind};

/// GitHub issue #221: how many further commits each "load more" adds to the walk cap.
///
/// One full `wt_core::graph::DEFAULT_MAX_COMMITS` batch, for a reason specific to how loading
/// more actually works here: `build_graph` has no resumable cursor, so every batch re-walks the
/// history *from the tips* with a bigger cap. Total work to reach `n` commits is therefore
/// quadratic in the number of batches (`n²/2b` commits walked), so a small batch is not merely
/// slower to fill the screen - it makes the whole feature progressively more expensive the
/// further back the user goes. 500 is also the one batch size already known to be acceptable in
/// practice: it is exactly the walk every single graph-tab open already pays for.
///
/// At `theme::graph::ROW`'s 26px, 500 rows is ~13000px of content - many viewports' worth, so
/// combined with [`LOAD_MORE_PREFETCH_ROWS`] below the user should never actually watch it load.
const LOAD_MORE_BATCH: usize = wt_core::graph::DEFAULT_MAX_COMMITS;

/// How close to the last loaded row the visible range has to get before the next batch starts
/// walking - roughly one 1080p viewport of `theme::graph::ROW` rows (~41 fit), so the walk starts
/// about a screen before the user can actually reach the end of the loaded rows.
///
/// Must stay far below [`LOAD_MORE_BATCH`], or a completed batch would land the visible range
/// still inside the trigger zone and immediately start another one.
const LOAD_MORE_PREFETCH_ROWS: usize = 40;

impl AdeApp {
    /// Opens the git graph tab (the tab strip's own entry, the `+` menu's "Git graph" row, the
    /// palette's "Open git graph", `mod+shift+G`, and the status bar's branch cluster all funnel
    /// through this). Idempotent: re-invoking while already open just re-activates it (used by
    /// the tab's own click handler too, so there is exactly one open/activate code path).
    pub(crate) fn open_git_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // GitHub issue #90: a genuinely empty window has no real repo to graph at all - an
        // independent audit found this reachable two real ways with no focused repo (`mod+shift+G`
        // is bound with no key context, so it dispatches regardless, and the palette's own "Open
        // git graph" entry - see `crate::palette::render::AdeApp`'s own gating on this same guard)
        // and two real consequences: `window.focus(&self.graph_focus_handle, cx)` below would
        // dangle, since `graph_focus_handle` is only ever tracked inside `Self::render_center_pane`
        // - part of `Self::render_workspace_body`, never rendered while `Self::render_empty_state`
        // is showing instead (the same class of bug `Self::open_repo_in_current_window`'s own
        // `empty_state_focus_handle` forgetting fixes) - and `Self::load_graph` reads
        // `self.diff_root`, an empty `PathBuf` in this state, which `wt_core::graph::build_graph`
        // hands to `gix::open`, which resolves an empty path relative to the *process's* real
        // working directory - silently showing whatever unrelated repo `jerry` happened to be
        // launched from, a real, confusing (if read-only) wrong-repo display.
        if self.focused_repo().is_none() {
            return;
        }
        // Mirrors `Self::open_settings`'s own defensive top-of-function close: reachable directly
        // (the status bar cluster, the `+` menu, `mod+shift+G`) while the palette also happens to
        // be open, not just via `crate::palette::render::AdeApp::execute_palette_command`'s own
        // "Open git graph" entry (which never hits this branch - the palette closes itself around
        // that call instead, see `Self::run_selected_palette_entry`'s docs).
        if self.palette_open {
            self.close_palette(window, cx);
        }
        // GitHub issue #225: the review tab is the other centre-pane occupant, and it needs the
        // same real teardown here that this function's own `leave_graph_tab` counterparts perform
        // in the opposite direction - see `crate::review::render::AdeApp::leave_review_tab`.
        self.leave_review_tab(window, cx);
        self.review_tab_open = None;

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
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
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
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
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
        // GitHub issue #127: set alongside the real `window.focus` call, not via a `cx.on_focus`
        // subscription - `graph_focus_handle` is only ever `track_focus`'d conditionally
        // (`Self::render_center_pane` renders `Self::render_graph_view` only while
        // `graph_tab_active` is `true`), and a live-tested subscription registered before that
        // first render never actually fired for it. See [`AdeApp::graph_view_focused`]'s own
        // docs for why the row-selection highlight needs this at all.
        self.graph_view_focused = true;

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
        // GitHub issue #242 phase B fix: closing the tab outright while interactive rebase mode
        // was live used to leave `graph_state.rebase` (and any agent this session had paused via
        // "Pause now") stranded - the only real recovery surface (the rebase banner) had just
        // been removed with no way to trigger a real resume. Routes through the same real exit
        // path every other rebase-mode departure now uses.
        if self.graph_state.rebase.is_some() {
            self.leave_rebase_mode(cx);
        }
        self.graph_state.load = GraphLoadState::NotLoaded;
        self.graph_state.row_menu_open = None;
        self.graph_state.push_menu_open = false;
        // GitHub issue #241: the Branches panel's own branch menu is a `graph_tab_active`-gated
        // overlay exactly like the two above, so it is torn down with them.
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
        self.graph_state.commit_files_cache = None;
        // GitHub issue #221: a reopened tab loads from scratch (see this function's own docs), so
        // the incremental walk cap has to start over with it - and dropping the task cancels any
        // "load more" still walking for a tab that no longer exists.
        self.graph_state.loaded_cap = wt_core::graph::DEFAULT_MAX_COMMITS;
        self.graph_state.load_more_in_flight = false;
        self.graph_state.load_more_failed = false;
        self.graph_state._load_more_task = None;
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
        // GitHub issue #127: the tab becoming inactive means `graph_focus_handle` stops being
        // `track_focus`'d at all (see the comment just below), so it definitionally can't be
        // focused any more, regardless of which handle `restore_focus` below lands on next.
        self.graph_view_focused = false;
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
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.graph_focus, fallback, window, cx);
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
        // GitHub issue #241: the same "switch tabs and back reveals a stale overlay with no
        // click at all" gap the comment above fixes for the row/push menus applies identically
        // to the "Create branch here" prompt (also gated on `graph_tab_active` in
        // `crate::root::AdeApp::render`) and to a stale armed Hard-reset confirmation.
        self.graph_state.branch_prompt = None;
        self.graph_state.hard_reset_confirm_armed = None;
        // GitHub issue #241: and identically to the branch menu and its own armed Delete
        // confirmation, for exactly the same "switch tabs and back" reason.
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
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
        // The Branches panel's list is rebuilt from the very graph being reloaded, and the branch
        // the menu targets may not even survive the reload (it can have been renamed or deleted -
        // by this app's own menu, or by anything else touching the repository). Dismissing is the
        // honest answer, the same call `row_menu_open` above makes for the same reason.
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
        // GitHub issue #221: a fresh load starts the incremental "load more" sequence over from
        // the default cap. Dropping the in-flight task cancels any load-more still walking against
        // the *previous* scope, whose result would be a graph for a scope the user has already
        // left; `load_more_failed` is cleared here because a fresh load is also the only real
        // retry path after a failed one.
        self.graph_state.loaded_cap = wt_core::graph::DEFAULT_MAX_COMMITS;
        self.graph_state.load_more_in_flight = false;
        self.graph_state.load_more_failed = false;
        self.graph_state._load_more_task = None;
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

    /// GitHub issue #221 ("Git graph only displays 500 commits"): walks further back and replaces
    /// the loaded graph with the longer walk, keeping the user exactly where they were.
    ///
    /// `wt_core::graph::build_graph` is a one-shot walk with no resumable cursor, so "load more"
    /// is a *re-walk with a bigger cap*, not an append. That is sound because the walk is
    /// deterministic - same tips, same `Sorting::ByCommitTime(NewestFirst)` - and
    /// `wt_core::graph::layout_lanes` is a single forward pass whose output for row `i` depends
    /// only on commits `0..=i`. A bigger cap therefore produces a strict prefix-identical
    /// superset: every already-visible row keeps its index, its lane and its elbows, so the
    /// scroll offset (owned by `uniform_list` itself, untouched here) still points at the same
    /// commits after the swap. `wt_core::graph::graph_walk_is_prefix_stable_across_caps` is the
    /// real test pinning that property.
    ///
    /// Unlike [`Self::load_graph`] this deliberately does *not* reset `selected_row`,
    /// `row_menu_open` or `commit_files_cache`: nothing the user was looking at moved. The two
    /// index-carrying fields are still re-resolved by commit id rather than simply left alone,
    /// because the one row that genuinely *can* shift every index is the synthetic "Uncommitted
    /// changes" row - it is not subject to `max_commits` at all and appears/disappears purely
    /// from a live `is_dirty` check, so a save landing between the two walks would otherwise
    /// silently slide the selection onto the neighbouring commit.
    pub(crate) fn load_more_graph_rows(&mut self, cx: &mut Context<Self>) {
        if self.graph_state.load_more_in_flight || self.graph_state.load_more_failed {
            return;
        }
        // Only a genuinely truncated walk has anything left to load; at the true end of history
        // there is nothing to fetch and nothing to say about it.
        if !self.current_graph().is_some_and(|graph| graph.truncated) {
            return;
        }
        let root = self.diff_root.clone();
        let scope = self.graph_state.scope;
        let next_cap = self.graph_state.loaded_cap.saturating_add(LOAD_MORE_BATCH);
        self.graph_state.load_more_in_flight = true;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::graph::build_graph(&root, scope, next_cap) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A fresh `load_graph` (a scope change, a fetch/pull/push) clears this flag while
                // we were walking; its own, newer graph must win over this now-stale one.
                if !this.graph_state.load_more_in_flight || this.graph_state.scope != scope {
                    return;
                }
                this.graph_state.load_more_in_flight = false;
                match result {
                    Ok(graph) => {
                        let selected_id = this
                            .graph_state
                            .selected_row
                            .and_then(|index| this.current_graph_row(index))
                            .map(|row| row.commit.id.clone());
                        let menu_id = this
                            .graph_state
                            .row_menu_open
                            .and_then(|menu| this.current_graph_row(menu.row_index))
                            .map(|row| row.commit.id.clone());
                        if let Some(id) = selected_id {
                            this.graph_state.selected_row =
                                graph.rows.iter().position(|row| row.commit.id == id);
                        }
                        match menu_id
                            .and_then(|id| graph.rows.iter().position(|row| row.commit.id == id))
                        {
                            Some(index) => {
                                if let Some(menu) = this.graph_state.row_menu_open.as_mut() {
                                    menu.row_index = index;
                                }
                            }
                            // The row it was opened against is genuinely gone from this walk;
                            // leaving the menu pointing at whatever now sits at that index would
                            // aim its Cherry-pick/Revert rows at the wrong commit.
                            None => this.graph_state.row_menu_open = None,
                        }
                        this.graph_state.loaded_cap = next_cap;
                        this.graph_state.load = GraphLoadState::Loaded(graph);
                    }
                    Err(err) => {
                        // The already-loaded rows stay exactly as they are - a failed *extension*
                        // of the walk is no reason to throw away the history already on screen -
                        // but the trigger must stop re-firing, or every frame the user spends
                        // near the bottom spawns another failing walk.
                        this.graph_state.load_more_failed = true;
                        this.graph_state.status_message =
                            Some(format!("loading more commits failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        self.graph_state._load_more_task = Some(task);
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
        // GitHub issue #241: switching to the Commit panel unrenders the very branch rows the
        // branch menu is anchored over, so a still-open popover would be pointing at nothing.
        // Not reachable by clicking today (the menu's own full-window scrim eats the click that
        // would hit the panel toggle first), which is exactly why it is worth closing here rather
        // than relying on that scrim staying full-window forever - the same "the surface this
        // menu belongs to went away" rule `leave_graph_tab` applies.
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
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
            // GitHub issue #241: closing the menu (rather than acting on one of its rows) is
            // itself "some other action" - see `GraphTabState::hard_reset_confirm_armed`'s own
            // docs.
            self.graph_state.hard_reset_confirm_armed = None;
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
    /// - **Clamped inside the window**, via the same `crate::menu::model::clamp_menu_origin` the tree
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
        let (clamped_x, clamped_y) = crate::menu::model::clamp_menu_origin(
            f32::from(origin_x),
            f32::from(origin_y),
            f32::from(theme::graph::ROW_MENU_WIDTH),
            f32::from(theme::graph::ROW_MENU_HEIGHT),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        // An adversarial audit's own finding: this menu and the Push `▾` menu
        // (`Self::toggle_graph_push_menu`) are independent booleans/options with no shared
        // "only one overlay open" invariant, so without this a right-click while the Push menu
        // was open left both painted at once (and this menu's own scrim, which *does*
        // `stop_propagation` on a left-click, then ate the next click meant to dismiss the Push
        // menu). GitHub issue #176 generalised that one hand-added pair into the real shared
        // invariant `AdeApp::close_menu_surfaces_except` now enforces across every menu surface - this
        // call replaces the single `push_menu_open = false` that used to live here, and runs
        // *before* the assignment below so the sweep can't clear what it just set.
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::GraphRow));
        // GitHub issue #241: a freshly (re)opened menu instance never inherits an armed Hard
        // reset confirmation from a previous one - see
        // `GraphTabState::hard_reset_confirm_armed`'s own docs.
        self.graph_state.hard_reset_confirm_armed = None;
        self.graph_state.row_menu_open = Some(GraphRowMenu {
            row_index: index,
            origin_x: px(clamped_x),
            origin_y: px(clamped_y),
        });
        window.focus(&self.graph_focus_handle, cx);
        // GitHub issue #127 - see `Self::open_git_graph`'s own matching comment.
        self.graph_view_focused = true;
        cx.notify();
    }

    /// Opens the Branches panel's own branch context menu for `branch` at `origin_x`/`origin_y` -
    /// the real `event.position` of the right-click that opened it (GitHub issue #241). Mirrors
    /// [`Self::open_graph_row_menu_at`] point for point, including its two
    /// adversarial-audit-found requirements:
    /// - **Clamped inside the window**, via the same `crate::menu::model::clamp_menu_origin`, against
    ///   this menu's own real painted size (`theme::graph::BRANCH_MENU_WIDTH`/`BRANCH_MENU_HEIGHT`,
    ///   pinned by a test); the Branches panel sits at the window's right edge, so an unclamped
    ///   popover would run straight off it for *every* row.
    /// - **Always (re)opens** at the given position, even if a menu for this or another branch was
    ///   already open, so a second right-click never leaves a stale popover at the old position.
    ///
    /// Two deliberate differences from the row menu, both because this menu belongs to the right
    /// sidebar rather than the centre pane:
    /// - it does **not** move keyboard focus. The row menu focuses the graph view because a
    ///   right-click's own `stop_propagation` preempts the graph container's click-to-focus and
    ///   would otherwise leave the menu open with focus nowhere useful; here the equivalent move
    ///   would be to yank focus out of the panel's own filter box (the only focusable thing in
    ///   this panel) and into a different pane entirely, for a menu that is driven purely by
    ///   clicks. Leaving focus alone is the honest behaviour, and nothing here depends on it.
    /// - it is keyed by branch name rather than a row index - see [`GraphBranchMenu`]'s docs.
    pub(crate) fn open_graph_branch_menu_at(
        &mut self,
        branch: String,
        origin_x: gpui::Pixels,
        origin_y: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = window.bounds().size;
        let (clamped_x, clamped_y) = crate::menu::model::clamp_menu_origin(
            f32::from(origin_x),
            f32::from(origin_y),
            f32::from(theme::graph::BRANCH_MENU_WIDTH),
            f32::from(theme::graph::BRANCH_MENU_HEIGHT),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        // GitHub issue #176's shared one-menu-at-a-time invariant - see
        // `Self::open_graph_row_menu_at`'s own call to this for the real bug it closes. Runs
        // *before* the assignment below so the sweep can't clear what it just set.
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::GraphBranch));
        // A freshly (re)opened menu instance never inherits an armed Delete confirmation from a
        // previous one - see `GraphTabState::delete_branch_confirm_armed`'s own docs.
        self.graph_state.delete_branch_confirm_armed = None;
        self.graph_state.branch_menu_open = Some(GraphBranchMenu {
            branch,
            origin_x: px(clamped_x),
            origin_y: px(clamped_y),
        });
        cx.notify();
    }

    pub(crate) fn toggle_graph_push_menu(&mut self, cx: &mut Context<Self>) {
        let opening = !self.graph_state.push_menu_open;
        // GitHub issue #176 - replaces this method's own hand-added "also close the row menu"
        // rule with the shared invariant across all six menu surfaces.
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::GraphPush));
        self.graph_state.push_menu_open = opening;
        cx.notify();
    }

    /// Copies `text` to the real system clipboard - mirrors `crate::sidebar::tree_ops::AdeApp`'s
    /// own `cx.write_to_clipboard` use for "Copy path".
    pub(crate) fn copy_graph_text(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.graph_state.row_menu_open = None;
        // GitHub issue #241: see `GraphTabState::hard_reset_confirm_armed`'s own docs. Shared by
        // the row menu's Copy SHA/subject rows and the branch menu's Copy Branch Name row, so
        // both menus (and both armed confirmations) are dismissed here.
        self.graph_state.hard_reset_confirm_armed = None;
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
        cx.notify();
    }

    /// Real `git fetch` for the current worktree, off the UI thread - GitHub issue #1's own
    /// "Fetch" toolbar button. Updates remote-tracking refs only, so the graph reload afterward
    /// (which recomputes [`GraphTabState::upstream_counts`]) is what actually makes a fetch
    /// *visible*: the ahead/behind counts and any newly-visible remote branch chips.
    pub(crate) fn request_graph_fetch(&mut self, cx: &mut Context<Self>) {
        self.run_graph_remote_op("Fetch", cx, |root| wt_core::remote::fetch(&root));
    }

    /// Real `git pull` (fetch + merge into the current branch) for the current worktree, off the
    /// UI thread - GitHub issue #1's own "Pull" toolbar button. A real merge conflict surfaces
    /// as [`GraphTabState::status_message`] showing git's own error text (see
    /// `wt_core::remote`'s own module docs on why this doesn't attempt to resolve one itself) -
    /// the worktree is left exactly where a real `git pull` on the command line would leave it,
    /// for the user to resolve through the Changes panel or a real terminal.
    pub(crate) fn request_graph_pull(&mut self, cx: &mut Context<Self>) {
        self.run_graph_remote_op("Pull", cx, |root| wt_core::remote::pull(&root));
    }

    /// The Push menu's `force` row click handler - GitHub issue #1's own "push (force with
    /// lease, force, no force)". `force == PushForce::None` (the plain "Push" row) runs
    /// immediately, exactly like Fetch/Pull: it can only ever fast-forward the remote, never
    /// lose anyone's work. `WithLease`/`Force` are real, remote-history-losing operations
    /// (`wt_core::remote::PushForce::Force`'s own docs), so this applies the same two-click
    /// confirmation `crate::rail::render::AdeApp::request_prune` uses for worktree removal: the
    /// first click on a force row only arms [`GraphTabState::push_force_confirm_armed`] and
    /// re-labels the row (see `Self::render_graph_push_menu`), without pushing anything; a
    /// second click on that *same* force value is what actually runs [`wt_core::remote::push`].
    pub(crate) fn request_graph_push(
        &mut self,
        force: wt_core::remote::PushForce,
        cx: &mut Context<Self>,
    ) {
        use wt_core::remote::PushForce;

        if force != PushForce::None && self.graph_state.push_force_confirm_armed != Some(force) {
            self.graph_state.push_force_confirm_armed = Some(force);
            self.graph_state.status_message = Some(match force {
                PushForce::WithLease => "click Force with lease again to really push".to_string(),
                PushForce::Force => "click Force again to really push".to_string(),
                PushForce::None => unreachable!("guarded above"),
            });
            cx.notify();
            return;
        }
        self.graph_state.push_force_confirm_armed = None;

        let action = match force {
            PushForce::None => "Push",
            PushForce::WithLease => "Force-with-lease push",
            PushForce::Force => "Force push",
        };
        self.run_graph_remote_op(action, cx, move |root| wt_core::remote::push(&root, force));
    }

    /// The row menu's "Cherry-pick" action - GitHub issue #1's own "cherry pick". Applies
    /// `sha`'s changes as a new commit on top of the current worktree's branch
    /// (`wt_core::rewrite::cherry_pick`). A real conflict surfaces through
    /// [`Self::run_graph_remote_op`]'s own status-message path exactly like a conflicting
    /// [`Self::request_graph_pull`] does: the worktree is left in the real conflicted state for
    /// the user to resolve, not silently rolled back.
    pub(crate) fn request_graph_cherry_pick(&mut self, sha: String, cx: &mut Context<Self>) {
        // GitHub issue #241: any other row-menu action disarms a previously-armed Hard reset
        // confirmation - see [`GraphTabState::hard_reset_confirm_armed`]'s own docs.
        self.graph_state.hard_reset_confirm_armed = None;
        self.run_graph_remote_op("Cherry-pick", cx, move |root| {
            wt_core::rewrite::cherry_pick(&root, &sha)
        });
    }

    /// The row menu's "Revert" action - GitHub issue #1's own "revert commit". Creates a new
    /// commit undoing `sha`'s changes (`wt_core::rewrite::revert`); see
    /// [`Self::request_graph_cherry_pick`]'s own docs on real-conflict handling, which applies
    /// identically here.
    pub(crate) fn request_graph_revert(&mut self, sha: String, cx: &mut Context<Self>) {
        // GitHub issue #241: see [`Self::request_graph_cherry_pick`]'s own comment above.
        self.graph_state.hard_reset_confirm_armed = None;
        self.run_graph_remote_op("Revert", cx, move |root| {
            wt_core::rewrite::revert(&root, &sha)
        });
    }

    /// The row menu's "Check out" action (GitHub issue #241). Moves `HEAD` (detached) onto `sha`
    /// in the focused worktree (`wt_core::checkout::checkout`) - never the repository's main
    /// checkout, the same "current worktree" resolution [`Self::request_graph_cherry_pick`] and
    /// friends already use via `Self::diff_root`. A genuinely conflicting checkout (uncommitted
    /// changes that would be overwritten) surfaces through [`Self::run_graph_remote_op`]'s own
    /// status-message path with git's own real refusal text - this makes no attempt to pre-check
    /// or second-guess a dirty worktree itself; see `wt_core::checkout::checkout`'s own docs.
    pub(crate) fn request_graph_checkout(&mut self, sha: String, cx: &mut Context<Self>) {
        self.graph_state.hard_reset_confirm_armed = None;
        self.run_graph_remote_op("Check out", cx, move |root| {
            wt_core::checkout::checkout(&root, &sha)
        });
    }

    /// Opens the row menu's "Create branch here" prompt (GitHub issue #241) - a small, hand-
    /// rolled inline text input (mirrors `crate::root::new_file::AdeApp::start_new_file`'s own
    /// shape; see [`state::GraphBranchPrompt`]'s own docs for why there's no separate
    /// modal-dialog subsystem behind it). Closes the row `⋯` menu it was clicked from - the
    /// prompt replaces it as a focus-owning overlay, the same relationship the "New file" prompt
    /// has with whatever it was opened from.
    pub(crate) fn start_graph_create_branch(
        &mut self,
        sha: String,
        short_sha: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.hard_reset_confirm_armed = None;
        self.graph_state.row_menu_open = None;
        self.open_graph_branch_prompt(
            state::GraphBranchPromptKind::CreateAt {
                sha,
                short_sha,
                subject,
            },
            TextField::new(),
            window,
            cx,
        );
    }

    /// Opens the branch menu's "Rename Branch…" prompt (GitHub issue #241) - the *same* prompt
    /// [`Self::start_graph_create_branch`] opens, in [`state::GraphBranchPromptKind::Rename`]
    /// mode (see [`state::GraphBranchPrompt`]'s own docs on why there is deliberately only one).
    ///
    /// Pre-filled with the branch's real current name via `TextField::seeded`, so a rename starts
    /// from what the branch is actually called rather than an empty box - the one real difference
    /// from the create case beyond which `wt_core::checkout` call Enter ends up making.
    pub(crate) fn start_graph_rename_branch(
        &mut self,
        old_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.delete_branch_confirm_armed = None;
        self.graph_state.branch_menu_open = None;
        let seeded = TextField::seeded(&old_name);
        self.open_graph_branch_prompt(
            state::GraphBranchPromptKind::Rename { old_name },
            seeded,
            window,
            cx,
        );
    }

    /// Shared open path for both prompt kinds: replaces the text field wholesale (empty for a
    /// create, seeded for a rename) and moves real keyboard focus onto the prompt, which owns it
    /// until Enter or Escape.
    fn open_graph_branch_prompt(
        &mut self,
        kind: state::GraphBranchPromptKind,
        name: TextField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.branch_prompt = Some(state::GraphBranchPrompt { kind, error: None });
        self.graph_state.branch_prompt_name = name;
        window.focus(&self.graph_state.branch_prompt_focus_handle, cx);
        cx.notify();
    }

    /// Closes the branch-name prompt without creating or renaming anything - Escape, or a click
    /// on its own scrim. Mirrors `crate::root::new_file::AdeApp::cancel_new_file`'s own shape,
    /// simpler here since the prompt only ever opens while the graph tab is focused: focus always
    /// returns to [`AdeApp::graph_focus_handle`].
    pub(crate) fn cancel_graph_branch_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph_state.branch_prompt.take().is_some() {
            window.focus(&self.graph_focus_handle, cx);
            cx.notify();
        }
    }

    /// Enter on the branch-name prompt - runs whichever real `wt_core::checkout` mutation this
    /// prompt was opened for ([`state::GraphBranchPromptKind`]).
    ///
    /// The only hand-rolled validation here is "not empty" - just enough to avoid a
    /// clearly-broken `git checkout -b '' <sha>` / `git branch -m <old> ''` invocation. A name
    /// colliding with an existing branch, or one git rejects as invalid, is deliberately *not*
    /// pre-checked here and instead surfaces as git's own real error through
    /// [`Self::run_graph_remote_op`]'s status-message path, exactly like every other menu
    /// mutation's real-conflict handling.
    ///
    /// A rename whose "new" name is byte-for-byte the branch's current name is also left to git:
    /// `git branch -m <name> <name>` really does succeed as a no-op, which is an honest outcome
    /// for a user who confirmed a rename without changing anything.
    pub(crate) fn commit_graph_branch_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = self.graph_state.branch_prompt.clone() else {
            return;
        };
        let name = self
            .graph_state
            .branch_prompt_name
            .as_str()
            .trim()
            .to_string();
        if name.is_empty() {
            if let Some(open) = self.graph_state.branch_prompt.as_mut() {
                open.error = Some("branch name can't be empty".to_string());
            }
            cx.notify();
            return;
        }
        self.graph_state.branch_prompt = None;
        window.focus(&self.graph_focus_handle, cx);
        match prompt.kind {
            state::GraphBranchPromptKind::CreateAt { sha, .. } => {
                self.request_graph_create_branch(sha, name, cx)
            }
            state::GraphBranchPromptKind::Rename { old_name } => {
                self.request_graph_rename_branch(old_name, name, cx)
            }
        }
    }

    /// The real `wt_core::checkout::create_branch_at` call behind
    /// [`Self::commit_graph_branch_prompt`] - split out so tests can drive it directly without
    /// going through the prompt's own focus/window plumbing, matching
    /// [`Self::request_graph_cherry_pick`] and friends.
    pub(crate) fn request_graph_create_branch(
        &mut self,
        sha: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.hard_reset_confirm_armed = None;
        self.run_graph_remote_op("Create branch", cx, move |root| {
            wt_core::checkout::create_branch_at(&root, &name, &sha)
        });
    }

    /// The real `wt_core::checkout::rename_branch` call behind
    /// [`Self::commit_graph_branch_prompt`]'s rename path (GitHub issue #241) - split out for the
    /// same reason [`Self::request_graph_create_branch`] is, and running in the focused worktree
    /// like every other graph mutation (a branch rename is repository-wide either way, but the
    /// invocation still has to happen *somewhere*, and that somewhere is never the main checkout
    /// unless it happens to be the focused one).
    ///
    /// A collision with an existing name, or a name git rejects outright, surfaces through
    /// [`Self::run_graph_remote_op`]'s status-message path with git's own real error text.
    pub(crate) fn request_graph_rename_branch(
        &mut self,
        old_name: String,
        new_name: String,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.delete_branch_confirm_armed = None;
        self.run_graph_remote_op("Rename branch", cx, move |root| {
            wt_core::checkout::rename_branch(&root, &old_name, &new_name)
        });
    }

    /// The branch menu's "Checkout Branch" action (GitHub issue #241) - `git switch -- <branch>`
    /// in the focused worktree (`wt_core::checkout::checkout_branch`), landing on the *branch*
    /// with `HEAD` attached, not a detached commit.
    ///
    /// Deliberately not `wt_core::checkout::checkout` (the row menu's own "Check out"): that
    /// function is safe only because every one of *its* callers passes a commit id resolved from
    /// this app's own graph, never user-typed or taken from a branch listing - see its own docs.
    /// This action's `branch` comes from the Branches panel's own list instead, so it needs
    /// `checkout_branch`'s `--`-guarded implementation; see that function's docs for the real,
    /// live-reproduced flag-injection failure mode reusing plain `checkout` here would reopen.
    ///
    /// A branch already checked out in another worktree, or uncommitted changes that would be
    /// overwritten, are git's own refusals to make - surfaced verbatim through
    /// [`Self::run_graph_remote_op`], never pre-checked here.
    pub(crate) fn request_graph_branch_checkout(&mut self, branch: String, cx: &mut Context<Self>) {
        self.graph_state.delete_branch_confirm_armed = None;
        self.run_graph_remote_op("Check out", cx, move |root| {
            wt_core::checkout::checkout_branch(&root, &branch)
        });
    }

    /// The branch menu's "Push Branch…" action (GitHub issue #241) - a plain, never-forced push
    /// of that specific branch (`wt_core::remote::push_branch`), regardless of what is checked
    /// out in the focused worktree.
    ///
    /// Deliberately no force variant on this menu at all: the toolbar's own `Push ▾` menu is
    /// where the two remote-history-losing postures live, behind the two-click confirmation
    /// [`GraphTabState::push_force_confirm_armed`] documents. A plain push can only ever
    /// fast-forward the remote, so like that menu's own "Push" row it runs on a single click.
    pub(crate) fn request_graph_push_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        self.graph_state.delete_branch_confirm_armed = None;
        self.run_graph_remote_op("Push branch", cx, move |root| {
            wt_core::remote::push_branch(&root, &branch, wt_core::remote::PushForce::None)
        });
    }

    /// The branch menu's "Delete Branch…" action (GitHub issue #241) - real `git branch -d`
    /// (`wt_core::checkout::delete_branch`), behind the same two-click confirmation
    /// [`Self::request_graph_reset`]'s own "Hard" row uses: the first click on a given branch's
    /// Delete row only arms [`GraphTabState::delete_branch_confirm_armed`] and re-labels the row,
    /// without deleting anything; a second click on that *same* branch's Delete row is what
    /// actually runs the delete. Clicking Delete on a *different* branch arms that branch instead
    /// of inheriting the previous arm.
    ///
    /// Nothing about the branch is pre-validated: git itself refuses an unmerged branch and a
    /// branch checked out in any worktree, and its own refusal text is what the status line shows
    /// (see `wt_core::checkout::delete_branch`'s docs).
    ///
    /// The single-flight guard is checked **here**, before the confirmation is touched, rather
    /// than being left to [`Self::run_graph_remote_op`]'s own identical check at the far end (an
    /// adversarial audit's finding): that later check returns without running anything, but by
    /// then this method has already disarmed - so a confirmed second click landing while some
    /// other graph operation was still in flight silently deleted nothing, cleared the
    /// confirmation, and left the menu open with a stale status line, i.e. a dead click. Refusing
    /// up front keeps the arm intact, so the click the user is about to repeat still counts.
    pub(crate) fn request_graph_delete_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        if self.graph_state.remote_op_in_flight {
            return;
        }
        if self.graph_state.delete_branch_confirm_armed.as_deref() != Some(branch.as_str()) {
            self.graph_state.status_message =
                Some(format!("click Delete again to really delete {branch}"));
            self.graph_state.delete_branch_confirm_armed = Some(branch);
            cx.notify();
            return;
        }
        self.graph_state.delete_branch_confirm_armed = None;
        self.run_graph_remote_op("Delete branch", cx, move |root| {
            wt_core::checkout::delete_branch(&root, &branch)
        });
    }

    /// The row menu's "Soft"/"Mixed"/"Hard" reset actions (GitHub issue #241) - real `git reset`
    /// against the focused worktree's current branch (`wt_core::checkout::reset`).
    ///
    /// `Soft`/`Mixed` run immediately on a single click - neither ever discards uncommitted work
    /// (see `wt_core::checkout::ResetMode`'s own docs on what each really does), so like the Push
    /// menu's plain "Push" row they need no confirmation. `Hard` genuinely discards uncommitted
    /// changes and detaches whatever commits sat after `sha`, so it follows the exact two-click
    /// discipline [`Self::request_graph_push`] already established for its own Force rows: the
    /// first click on a given commit's "Hard" row only arms
    /// [`GraphTabState::hard_reset_confirm_armed`] and re-labels the row, without resetting
    /// anything; a second click on that *same* commit's "Hard" row is what actually runs the
    /// reset. Any other mode, or "Hard" on a *different* commit, disarms rather than carrying a
    /// stale confirmation over onto a reset the user never confirmed for that commit.
    pub(crate) fn request_graph_reset(
        &mut self,
        mode: wt_core::checkout::ResetMode,
        sha: String,
        cx: &mut Context<Self>,
    ) {
        use wt_core::checkout::ResetMode;

        // Same up-front single-flight guard, for the same reason, as
        // [`Self::request_graph_delete_branch`]'s own - see that method's docs for the dead-click
        // this ordering closes. Soft/Mixed behave exactly as before either way (they arm nothing,
        // and `run_graph_remote_op` would have refused them at the far end regardless).
        if self.graph_state.remote_op_in_flight {
            return;
        }
        if mode == ResetMode::Hard
            && self.graph_state.hard_reset_confirm_armed.as_deref() != Some(sha.as_str())
        {
            self.graph_state.hard_reset_confirm_armed = Some(sha);
            self.graph_state.status_message = Some("click Hard again to really reset".to_string());
            cx.notify();
            return;
        }
        self.graph_state.hard_reset_confirm_armed = None;

        let action = match mode {
            ResetMode::Soft => "Soft reset",
            ResetMode::Mixed => "Mixed reset",
            ResetMode::Hard => "Hard reset",
        };
        self.run_graph_remote_op(action, cx, move |root| {
            wt_core::checkout::reset(&root, mode, &sha)
        });
    }

    /// Shared plumbing behind [`Self::request_graph_fetch`]/[`Self::request_graph_pull`]/
    /// [`Self::request_graph_push`]: guards against a double-click starting a second, overlapping
    /// git subprocess (`GraphTabState::remote_op_in_flight`), runs `op` on the background
    /// executor (every `wt_core::remote` function performs real blocking I/O), then surfaces a
    /// real success or a real git error message and reloads the graph either way - a fetch/pull/
    /// push always changes what the graph/ahead-behind counts should show, success or not (a
    /// failed pull can still have fetched new remote-tracking data, for one).
    fn run_graph_remote_op(
        &mut self,
        action: &'static str,
        cx: &mut Context<Self>,
        op: impl FnOnce(std::path::PathBuf) -> Result<(), wt_core::Error> + Send + 'static,
    ) {
        if self.graph_state.remote_op_in_flight {
            return;
        }
        let root = self.diff_root.clone();
        self.graph_state.remote_op_in_flight = true;
        self.graph_state.push_menu_open = false;
        self.graph_state.row_menu_open = None;
        // GitHub issue #241: the Branches panel's own branch menu is dismissed by acting on it,
        // exactly like the row menu above - one place, so every branch action gets it (and its
        // armed-Delete disarm) for free rather than each remembering to.
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
        self.graph_state.status_message = Some(format!("{action}\u{2026}"));
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.graph_state.remote_op_in_flight = false;
                this.graph_state.status_message = Some(match result {
                    Ok(()) => action.to_string(),
                    Err(err) => format!("{action} failed: {err}"),
                });
                this.load_graph(cx);
            });
        });
        self.graph_state._remote_op_task = Some(task);
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
        // GitHub issue #27's "solid mid-keystroke" - see `crate::rail::render::AdeApp::
        // handle_filter_key_down`'s identical reasoning. Missing here (GitHub issue #45) is
        // exactly why this field never blinked in the first place.
        self.reset_caret_blink(cx);
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

    /// The "Create branch here" prompt's key handler - append/backspace/Enter (create)/Escape
    /// (cancel), mirroring `crate::root::new_file::AdeApp::handle_new_file_key_down`'s own
    /// minimal shape (GitHub issue #241).
    pub(in crate::graph_view) fn handle_graph_branch_prompt_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.cancel_graph_branch_prompt(window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.commit_graph_branch_prompt(window, cx);
                cx.stop_propagation();
            }
            "backspace" => {
                if self.graph_state.branch_prompt.is_some() {
                    self.graph_state.branch_prompt_name.pop(Instant::now());
                    if let Some(open) = self.graph_state.branch_prompt.as_mut() {
                        open.error = None;
                    }
                    self.reset_caret_blink(cx);
                    cx.notify();
                    cx.stop_propagation();
                }
            }
            _ => {
                if let Some(text) = keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty())
                {
                    if self.graph_state.branch_prompt.is_some() {
                        self.graph_state
                            .branch_prompt_name
                            .push_str(text, Instant::now());
                        if let Some(open) = self.graph_state.branch_prompt.as_mut() {
                            open.error = None;
                        }
                        self.reset_caret_blink(cx);
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }
        }
    }

    /// `Ctrl/Cmd+Z` inside the "Create branch here" prompt (GitHub issue #17's per-widget text
    /// undo, GitHub issue #241).
    pub(in crate::graph_view) fn handle_graph_branch_prompt_text_undo(
        &mut self,
        _: &crate::root::TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph_state.branch_prompt_name.undo() {
            if let Some(open) = self.graph_state.branch_prompt.as_mut() {
                open.error = None;
            }
            cx.notify();
        }
    }

    /// `Ctrl/Cmd+Shift+Z` / `Ctrl+Y` inside the "Create branch here" prompt - the mirror of
    /// [`Self::handle_graph_branch_prompt_text_undo`].
    pub(in crate::graph_view) fn handle_graph_branch_prompt_text_redo(
        &mut self,
        _: &crate::root::TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.graph_state.branch_prompt_name.redo() {
            if let Some(open) = self.graph_state.branch_prompt.as_mut() {
                open.error = None;
            }
            cx.notify();
        }
    }

    /// The branch-name prompt: a scrim + small centered panel (GitHub issue #241), exactly the
    /// shape `crate::root::new_file::AdeApp::render_new_file_prompt` already established for this
    /// app's one other hand-rolled "prompt for a name" - transparent-to-nothing modal scrim,
    /// `.occlude()`d panel that stops its own click from bubbling up and dismissing it. Assumes
    /// `Self::graph_state.branch_prompt` is `Some` - the caller (`crate::root::AdeApp::render`)
    /// only renders this when it is.
    ///
    /// One panel for both kinds ([`state::GraphBranchPrompt`]): only the title, the subtitle line
    /// (the target commit for a create, the branch's current name for a rename) and the footer
    /// hint differ - everything else, including the real text field and its caret, is literally
    /// the same element tree.
    pub(crate) fn render_graph_branch_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = self.graph_state.branch_prompt.clone();
        let name = self.graph_state.branch_prompt_name.as_str().to_string();
        let has_name = !name.is_empty();
        let is_rename = matches!(
            prompt.as_ref().map(|prompt| &prompt.kind),
            Some(state::GraphBranchPromptKind::Rename { .. })
        );
        let title = if is_rename {
            "Rename branch"
        } else {
            "Create branch here"
        };
        let subtitle = match prompt.as_ref().map(|prompt| &prompt.kind) {
            Some(state::GraphBranchPromptKind::CreateAt {
                short_sha, subject, ..
            }) => format!("{short_sha} \u{b7} {subject}"),
            Some(state::GraphBranchPromptKind::Rename { old_name }) => old_name.clone(),
            None => String::new(),
        };
        let footer = if is_rename {
            "enter to rename \u{b7} esc to cancel"
        } else {
            "enter to create \u{b7} esc to cancel"
        };
        div()
            .id("graph-branch-prompt-scrim")
            .absolute()
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .bg(modal_scrim_bg())
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.cancel_graph_branch_prompt(window, cx);
            }))
            .child(
                div()
                    .id("graph-branch-prompt-panel")
                    .track_focus(&self.graph_state.branch_prompt_focus_handle)
                    .key_context("text-input")
                    .on_action(cx.listener(Self::handle_graph_branch_prompt_text_undo))
                    .on_action(cx.listener(Self::handle_graph_branch_prompt_text_redo))
                    .on_key_down(cx.listener(Self::handle_graph_branch_prompt_key_down))
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .w(px(320.0))
                    .p(px(12.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(11.5))
                            .text_color(theme::text::HEADING)
                            .child(title),
                    )
                    .child(
                        div()
                            .debug_selector(|| "graph-branch-prompt-subtitle".to_string())
                            .font(font(theme::font::MONO))
                            .text_size(px(9.5))
                            .text_color(theme::text::FAINTER)
                            .child(subtitle),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(theme::radius::CHIP)
                            .bg(theme::surface::SEGMENT_TRACK)
                            .flex()
                            .items_center()
                            // No decorative gap before the caret - see
                            // `crate::rail::render::AdeApp::render_rail_filter_row`'s own
                            // comment for why (live report: it read as a gap between the
                            // typed text and where it's actually being typed).
                            .font(font(theme::font::MONO))
                            .text_size(px(11.5))
                            .text_color(theme::text::BODY)
                            .when(!has_name, |el| {
                                el.child(self.render_simple_input_caret(
                                    "graph-branch-prompt-caret",
                                    &self.graph_state.branch_prompt_focus_handle,
                                ))
                            })
                            .child(if has_name {
                                name
                            } else {
                                "branch-name".to_string()
                            })
                            .when(has_name, |el| {
                                el.child(self.render_simple_input_caret(
                                    "graph-branch-prompt-caret",
                                    &self.graph_state.branch_prompt_focus_handle,
                                ))
                            }),
                    )
                    .when_some(prompt.and_then(|prompt| prompt.error), |el, error| {
                        el.child(
                            div()
                                .font(font(theme::font::SANS))
                                .text_size(px(10.5))
                                .text_color(theme::status::FAIL)
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(10.0))
                            .text_color(theme::text::GHOST)
                            .child(footer),
                    ),
            )
    }
}

/// The tab strip's own "git graph" entry - a fourth, independent parallel slot alongside agent
/// tabs and file tabs, mirroring `crate::work_surface::render::render_tab_strip`'s existing
/// two-collection shape rather than unifying into one `Tab` enum (see that function's docs, and
/// this project's own note that a forced unification wasn't the right call here). Rendered only
/// while `AdeApp::graph_tab_open` is `true`.
pub(crate) fn render_graph_tab(app: &AdeApp, cx: &mut Context<AdeApp>) -> gpui::AnyElement {
    let is_active = app.graph_tab_active;
    let colors = work_surface::tab_colors(is_active);
    let close_color = if is_active {
        theme::text::DIMMER
    } else {
        theme::text::DISABLED
    };
    let tab_ref = work_surface::TabRef::Graph;
    let drag_value = DraggedTab::Graph {
        label: "Git graph".to_string(),
    };

    let close_button = app.render_tab_close_button(
        "close-graph-tab",
        close_color,
        None,
        |this, window, cx| {
            this.close_git_graph_tab(window, cx);
        },
        cx,
    );
    let content: Vec<gpui::AnyElement> = vec![
        render_graph_tab_chip().into_any_element(),
        div()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(app.ui_text_size(11.0))
            .text_color(colors.label)
            .child("Git graph")
            .into_any_element(),
        close_button.into_any_element(),
    ];

    app.render_tab_chrome(
        TabChromeArgs {
            outer_id: "graph-tab".into(),
            hit_id: "graph-tab-hit".into(),
            tab_ref,
            drag_value,
            is_active,
            content,
            // Middle-click closes the graph tab too (GitHub issue #26) - the same real
            // `close_git_graph_tab` teardown the `×` button already uses, matching file/agent
            // tabs.
            on_middle_click: Box::new(|this, window, cx| {
                this.close_git_graph_tab(window, cx);
            }),
            on_activate: Box::new(|this, window, cx| {
                this.open_git_graph(window, cx);
            }),
            // `middle_clicking_the_graph_tab_closes_it_like_every_other_tab_kind`'s own real
            // mouse-position simulation (`cx.debug_bounds("graph-tab")`) needs this - the only
            // tab-kind test that simulates a real click rather than calling the close handler
            // directly.
            debug_selector: Some("graph-tab"),
        },
        cx,
    )
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
        // GitHub issue #242 phase B: the graph pane's own interactive-rebase mode entirely
        // replaces the ordinary toolbar/commit-list body while active - see
        // `crate::graph_view::rebase_render`'s own module docs.
        if self.graph_state.rebase.is_some() {
            return self.render_rebase_view(cx);
        }
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

    /// Toolbar (design spec §4): `HEAD` branch/chip/counts, the `All | Worktrees | Current` scope
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
                        this.request_graph_fetch(cx);
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
                    this.request_graph_pull(cx);
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
            widgets::ChoiceOption::new("Worktrees"),
            widgets::ChoiceOption::new("Current"),
        ];
        let selected = match self.graph_state.scope {
            GraphScope::All => "All",
            GraphScope::Worktrees => "Worktrees",
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
                    1 => GraphScope::Worktrees,
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
                menu_popover_chrome(
                    div()
                        .id("graph-push-menu-popover")
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y + bounds.size.height + px(2.0))
                        .w(theme::graph::PUSH_MENU_WIDTH)
                        .py(px(4.0)),
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .child(
                    render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::BLUE_FG.into(),
                        theme::button::BLUE_BG.into(),
                        "Push",
                        "fast-forwards the remote branch".to_string(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.request_graph_push(wt_core::remote::PushForce::None, cx);
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::DANGER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Force with lease",
                        if self.graph_state.push_force_confirm_armed
                            == Some(wt_core::remote::PushForce::WithLease)
                        {
                            "click again to really push".to_string()
                        } else {
                            "aborts if the remote moved".to_string()
                        },
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.request_graph_push(wt_core::remote::PushForce::WithLease, cx);
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::DANGER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Force",
                        if self.graph_state.push_force_confirm_armed
                            == Some(wt_core::remote::PushForce::Force)
                        {
                            "click again to really push".to_string()
                        } else {
                            "overwrites the remote unconditionally".to_string()
                        },
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.request_graph_push(wt_core::remote::PushForce::Force, cx);
                        },
                    )),
                ),
            )
    }

    /// The row list - a real `gpui::uniform_list` (GitHub issue #218: "when displaying a big git
    /// history the git graph is laggy").
    ///
    /// **Scrolling lives in the list itself, not in a wrapper.** Until this fix every one of the
    /// up to `wt_core::graph::DEFAULT_MAX_COMMITS` (500) loaded rows was built, laid out and
    /// painted on *every single frame*, inside a plain `div().overflow_y_scroll()` - including
    /// all the ones scrolled off screen (at `theme::graph::ROW`'s 26px, a 1080p window shows on
    /// the order of 30 of them), each carrying a `gpui::canvas` lane painter, its ref chips and
    /// a `⋯` trigger button. That is structurally the same per-frame cost
    /// `crate::sidebar::render::AdeApp::render_file_tree` was carrying before it was virtualized,
    /// where it measured as ~72% of a whole `Window::draw` - that number is the file tree's, on
    /// its own row shape, not a measurement of this list; what is measured here is
    /// `super::render::graph_virtualization_tests`' own before/after, i.e. that a row far below
    /// the viewport stopped being built at all. This method deliberately follows that one's
    /// structure rather than inventing a second pattern.
    ///
    /// Two things about `uniform_list` are load-bearing:
    /// - its default `ListSizingBehavior::Auto` gives it zero intrinsic height, so every pixel
    ///   of its height comes from the `.flex_1().min_h_0()` below - drop either and the list
    ///   renders zero rows, with no panic and no warning;
    /// - it sets its own `overflow.y = Scroll` and owns the scroll offset, so it needs no
    ///   `overflow_y_scroll()` wrapper and must not be given one. (Re-adding one here was tried
    ///   directly and did *not* defeat the virtualization the way `render_file_tree`'s docs warn
    ///   it can - the `.flex_1().min_h_0()` still resolved against the wrapper's definite height
    ///   - so this is redundancy to avoid, not a live trap on this particular call site.)
    ///
    /// Its one real requirement - a fixed row height - is already satisfied: every commit row is
    /// exactly `theme::graph::ROW` tall (see [`Self::render_graph_row`]), and so is the trailing
    /// "loading more commits" row ([`render_graph_load_more_row`]), which is a genuine item of
    /// this list rather than a sibling appended outside it.
    ///
    /// **The visible range is also the "load more" trigger** (GitHub issue #221). The range
    /// `uniform_list` hands the row builder every frame already *is* the scroll position, so
    /// nothing extra needs measuring: once it reaches within [`LOAD_MORE_PREFETCH_ROWS`] of the
    /// last loaded row, [`Self::load_more_graph_rows`] walks further back. That function is
    /// single-flight guarded (`GraphTabState::load_more_in_flight`), which it has to be - this
    /// closure runs several times per frame (`uniform_list` calls it once at `0..1` to measure the
    /// row height, again during layout, then once more with the real visible range) and on every
    /// frame the user lingers near the bottom.
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
        // Resolved once per frame rather than inside the row builder: `uniform_list` invokes that
        // closure several times per frame (measure, prepaint, then the real visible range), and
        // a wall-clock read per call could hand two rows of the same frame different "now"s.
        let now = unix_now();

        let list = uniform_list(
            "graph-rows",
            graph_item_count(graph, self.graph_state.load_more_in_flight),
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                // Re-resolved from `this` rather than captured: the closure is `'static` and
                // cannot borrow the `&Graph` this method was handed, and re-reading it means a
                // reload that replaced the whole graph between this frame's `item_count` read and
                // this call renders fewer rows instead of indexing a stale snapshot. Mirrors
                // `crate::sidebar::render::AdeApp::render_changes_rows`'s identical re-resolve -
                // and it costs no `GraphRow` clones at all, which capturing would have.
                let Some(graph) = this.current_graph() else {
                    return Vec::new();
                };
                // GitHub issue #221. Copied out now because the `&Graph` borrow above has to end
                // before the `&mut self` "load more" call at the bottom of this closure.
                let row_count = graph.rows.len();
                let loading_more = this.graph_state.load_more_in_flight;
                // Clamped rather than trusted, and `start` against `end` rather than only against
                // the length, so a divergence degrades to "renders fewer rows" instead of
                // panicking on an inverted range.
                let end = range.end.min(graph_item_count(graph, loading_more));
                let start = range.start.min(end);
                let items = (start..end)
                    .map(|index| match graph.rows.get(index) {
                        Some(row) => this
                            .render_graph_row(index, row, graph.lane_count, now, cx)
                            .into_any_element(),
                        // The one index past the last row, which `graph_item_count` only hands
                        // out while a real "load more" walk is genuinely in flight.
                        None => render_graph_load_more_row().into_any_element(),
                    })
                    .collect::<Vec<_>>();
                // Fired *after* this frame's rows are built, so the graph they were built from is
                // the one that was actually measured. `load_more_graph_rows` is a no-op unless the
                // walk really was truncated and nothing is already in flight.
                if end + LOAD_MORE_PREFETCH_ROWS >= row_count {
                    this.load_more_graph_rows(cx);
                }
                items
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&self.graph_state.rows_scroll_handle);

        // GitHub issue #142. The scrollbar is a *sibling* of the list inside this non-scrolling
        // `.relative()` wrapper, never a child of it - see
        // `crate::sidebar::render::AdeApp::render_file_tree`'s own docs for why a scrollbar
        // painted as a scrolling element's own child scrolls away with the content.
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .children(self.render_vertical_scrollbar(
                "graph-rows-scrollbar",
                &self.graph_state.rows_scroll_handle,
                &[],
                cx,
            ))
            .into_any_element()
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
        // GitHub issue #127: the selected-row highlight (background + left edge) must clear once
        // real keyboard focus genuinely moves away from the graph view, not stay lit forever
        // against whatever row was last selected. `graph_view_focused` is a plain bool set
        // explicitly alongside every real `window.focus(&self.graph_focus_handle, ...)`/
        // graph-tab-exit call site, rather than a live `FocusHandle::is_focused` check here,
        // since this render call chain never carries a real `&Window` - see that field's own
        // docs for why.
        let selected = self.graph_view_focused && self.graph_state.selected_row == Some(index);
        let is_working_tree = row.commit.id.is_empty();
        let relative = if row.commit.id.is_empty() {
            "now".to_string()
        } else {
            relative_time(row.commit.committer_time_unix, now_unix)
        };

        div()
            .id(("graph-row", index))
            .debug_selector(move || format!("graph-row-{index}"))
            .relative()
            .flex()
            .items_center()
            .w_full()
            .h(theme::graph::ROW)
            .cursor_pointer()
            // No bottom border at all - a real, reported design bug, not the row's own intended
            // separator: this row used to carry a permanent `border_b_1()` alongside a
            // conditional `border_l_2()` for its selection edge, and because GPUI's
            // `Style::border_color` is one shared value for every edge of a single element
            // (confirmed directly in `gpui`'s own `style.rs`: `border_color: Option<Hsla>`, not
            // per-edge), selecting a row silently recoloured the bottom edge to the selection
            // colour too - a real border appearing along the bottom on selection, not merely an
            // intended static separator. The correct fix is not "give the bottom edge its own
            // colour" - it is "there is no bottom border here at all, only the left selection
            // edge below", a real, separate child element: `left_0().top_0().bottom_0()`
            // reserves the same 2px of space whether or not this row is selected (only its own
            // `bg` colour toggles), the same "always paint the box, only recolour it" convention
            // the tab strip's own selection underline already uses
            // (`crate::work_surface::render::AdeApp::render_tab_chrome`'s
            // `div().flex_none().w_full().h(px(1.0)).bg(colors.underline)`).
            .child(
                div()
                    .debug_selector(move || format!("graph-row-{index}-selection-edge"))
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(if selected {
                        theme::border::SELECTED_EDGE.into()
                    } else {
                        work_surface::TRANSPARENT
                    }),
            )
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
            // The working-tree row's own union figure (GitHub issue #287) - `Jerry.dc.html`'s
            // `Git graph` state carries `13 files · +319 −145` in this slot, between the subject
            // and the author column. Read straight off the uncommitted change set, so it is the
            // same number the Uncommitted section's header states rather than a second count of
            // the same working tree. A commit row has no note: what a commit changed is its own
            // diff, one click away, and repeating a figure per row is what
            // `REVISION-2026-07-31.md` §4 removed from these columns in the first place.
            .children(is_working_tree.then(|| self.render_graph_working_tree_note()))
            .child(
                div()
                    .debug_selector(move || format!("graph-row-{index}-author"))
                    .w(px(88.0))
                    .px(px(4.0))
                    .truncate()
                    .flex()
                    .items_center()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::text::DIM)
                    // Audit I4: the row used to pin one agent (`s: 's3'`) to the whole working
                    // tree, which a shared worktree makes a plain falsehood. It carries the `by`
                    // union instead - every agent (and `you`) really on record for something dirty
                    // in this checkout - and a commit row keeps git's own author name, which is
                    // the honest answer for a commit and the only one git has.
                    .when(is_working_tree, |el| {
                        el.children(self.render_author_chip_strip(
                            "graph-working-tree",
                            &crate::provenance::render::chip_authors_for(
                                &self.uncommitted_change_set,
                            ),
                        ))
                    })
                    .when(!is_working_tree, |el| {
                        el.child(row.commit.author_name.clone())
                    }),
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

    /// The working-tree row's `N files · +A −B` note - the real union figure for everything dirty
    /// in this checkout, whoever wrote it.
    ///
    /// Every number comes from `AdeApp::uncommitted_change_set`, which is one row per path
    /// (`crate::provenance::change_set`), so the file count here is the worktree's own and not the
    /// sum of per-agent counts - a file two agents share is one dirty file, and
    /// `REVISION-2026-07-31.md` §4's "honest arithmetic" is exactly that those two figures differ.
    fn render_graph_working_tree_note(&self) -> impl IntoElement {
        let stat = self.uncommitted_change_set.total();
        div()
            .debug_selector(|| "graph-working-tree-note".to_string())
            .flex_none()
            .mr(px(10.0))
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::FAINTER)
            .child(format!(
                "{} \u{b7} +{} \u{2212}{}",
                crate::root::plural::count(self.uncommitted_change_set.len(), "file", None),
                stat.added,
                stat.removed
            ))
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
                // GitHub issue #241: see `GraphTabState::hard_reset_confirm_armed`'s own docs.
                this.graph_state.hard_reset_confirm_armed = None;
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
                    // GitHub issue #241: see `GraphTabState::hard_reset_confirm_armed`'s own
                    // docs - `Self::open_graph_row_menu_at` would disarm too if this lands on
                    // another row and reopens fresh right after, but a right-click on empty
                    // space only ever reaches this dismiss.
                    this.graph_state.hard_reset_confirm_armed = None;
                    cx.notify();
                }),
            )
            .child(
                menu_popover_chrome(
                    div()
                        .id("graph-row-menu-popover")
                        .debug_selector(|| "graph-row-menu-popover".to_string())
                        .absolute()
                        .left(menu.origin_x)
                        .top(menu.origin_y)
                        .w(theme::graph::ROW_MENU_WIDTH)
                        .py(px(4.0)),
                    theme::shadow::MENU,
                )
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
                .child(
                    render_dropdown_menu_row(
                        "\u{2713}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Check out",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_checkout(sha.clone(), cx);
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "+",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Create branch here",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        let short_sha = short_sha.clone();
                        let subject = subject.clone();
                        move |this, _event: &ClickEvent, window, cx| {
                            this.start_graph_create_branch(
                                sha.clone(),
                                short_sha.clone(),
                                subject.clone(),
                                window,
                                cx,
                            );
                        }
                    })),
                )
                .child(render_graph_row_menu_header("Apply"))
                .child(
                    render_dropdown_menu_row(
                        "\u{2398}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Cherry-pick",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_cherry_pick(sha.clone(), cx);
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{21b6}",
                        theme::button::AMBER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Revert",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_revert(sha.clone(), cx);
                        }
                    })),
                )
                .child(
                    // GitHub issue #241: one rebase entry, not two. This opens the interactive
                    // Planning banner (`AdeApp::enter_rebase_mode`), whose own one-click
                    // `Start rebase` covers the "just replay it, don't edit anything" case that
                    // used to have a separate, immediately-running row of its own.
                    render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Rebase onto this commit",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.enter_rebase_mode(index, cx);
                        },
                    )),
                )
                .child(render_graph_row_menu_header("Reset"))
                .child(
                    render_dropdown_menu_row(
                        "\u{21ba}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Soft",
                        "keeps changes staged".to_string(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_reset(
                                wt_core::checkout::ResetMode::Soft,
                                sha.clone(),
                                cx,
                            );
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{21ba}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Mixed",
                        "keeps changes unstaged".to_string(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_reset(
                                wt_core::checkout::ResetMode::Mixed,
                                sha.clone(),
                                cx,
                            );
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{21ba}",
                        theme::button::DANGER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Hard",
                        if self.graph_state.hard_reset_confirm_armed.as_deref()
                            == Some(sha.as_str())
                        {
                            "click again to really reset".to_string()
                        } else {
                            "discards uncommitted changes".to_string()
                        },
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let sha = sha.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_reset(
                                wt_core::checkout::ResetMode::Hard,
                                sha.clone(),
                                cx,
                            );
                        }
                    })),
                )
                .child(render_graph_row_menu_header("Copy"))
                .child(
                    render_dropdown_menu_row(
                        "#",
                        theme::text::SECONDARY.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Copy SHA",
                        short_sha,
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.copy_graph_text(sha.clone(), cx);
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{ab}",
                        theme::text::SECONDARY.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Copy subject",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.copy_graph_text(subject.clone(), cx);
                        },
                    )),
                )
                .child(
                    div()
                        .px(px(11.0))
                        .pt(px(4.0))
                        .font(font(theme::font::SANS))
                        .text_size(px(9.5))
                        .text_color(theme::text::GHOSTER)
                        .child(
                            "check out, branch, rebase, and reset all run in the focused \
                             worktree, never the main checkout",
                        ),
                ),
            )
            .into_any_element()
    }

    /// The Branches panel's own branch right-click context menu (GitHub issue #241) - the seven
    /// real actions VSCode's Git Graph extension offers on a local branch, scoped to exactly
    /// those: Checkout / Rename / Delete, then Merge / Rebase, then Push, then Copy.
    ///
    /// Structurally the row `⋯` menu's twin ([`Self::render_graph_row_menu`]) and deliberately
    /// so: the same `menu_popover_chrome` panel, the same [`render_dropdown_menu_row`] rows and
    /// group headers, and the same scrim contract, including both of that menu's own
    /// adversarial-audit-found fixes:
    /// - the scrim's left-click dismiss `cx.stop_propagation()`s, so a click that dismisses this
    ///   menu can never *also* re-trigger whatever sits underneath it in the same `MouseUpEvent`;
    /// - the popover itself `.occlude()`s, so a right-click inside its own bounds can never fall
    ///   through to the branch row it is painted over and retarget the menu to that branch.
    ///
    /// A right-click on the scrim (outside the popover) deliberately does *not* stop propagation,
    /// so it dismisses and then still reaches whichever branch row it landed on, opening fresh
    /// there in the same click - see the row menu's own matching comment.
    ///
    /// Anchored to the position captured once at open time
    /// ([`GraphBranchMenu::origin_x`]/`origin_y`), never recomputed here, and keyed by branch
    /// *name*: the panel's list is rebuilt and re-filtered constantly, so a row index would go
    /// stale while an index-keyed menu stayed open.
    pub(crate) fn render_graph_branch_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.graph_state.branch_menu_open.clone() else {
            return gpui::Empty.into_any_element();
        };
        let branch = menu.branch.clone();
        let delete_armed =
            self.graph_state.delete_branch_confirm_armed.as_deref() == Some(branch.as_str());
        // The branch the merge would land in, read off the same real `wt_core::list_worktrees`
        // data the rest of the app uses for the focused worktree - never a guess, and honestly
        // absent (no sub-label at all) when the worktree is on a detached `HEAD`.
        let current_branch = self
            .worktrees
            .iter()
            .find(|item| item.path == self.diff_root)
            .and_then(|item| item.branch.clone());

        div()
            .id("graph-branch-menu-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.graph_state.branch_menu_open = None;
                this.graph_state.delete_branch_confirm_armed = None;
                cx.notify();
            }))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _event: &gpui::MouseDownEvent, _window, cx| {
                    this.graph_state.branch_menu_open = None;
                    this.graph_state.delete_branch_confirm_armed = None;
                    cx.notify();
                }),
            )
            .child(
                menu_popover_chrome(
                    div()
                        .id("graph-branch-menu-popover")
                        .debug_selector(|| "graph-branch-menu-popover".to_string())
                        .absolute()
                        .left(menu.origin_x)
                        .top(menu.origin_y)
                        .w(theme::graph::BRANCH_MENU_WIDTH)
                        .py(px(4.0)),
                    theme::shadow::MENU,
                )
                .occlude()
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .child(render_graph_row_menu_header("Branch"))
                .child(
                    render_dropdown_menu_row(
                        "\u{2713}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Checkout Branch",
                        branch.clone(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_branch_checkout(branch.clone(), cx);
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{270e}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Rename Branch\u{2026}",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, window, cx| {
                            this.start_graph_rename_branch(branch.clone(), window, cx);
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{2715}",
                        theme::button::DANGER_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Delete Branch\u{2026}",
                        if delete_armed {
                            "click again to really delete".to_string()
                        } else {
                            "refused if it has unmerged commits".to_string()
                        },
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_delete_branch(branch.clone(), cx);
                        }
                    })),
                )
                .child(render_graph_row_menu_header("Integrate"))
                .child(
                    render_dropdown_menu_row(
                        "\u{2193}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Merge into current branch\u{2026}",
                        current_branch.clone().unwrap_or_default(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, window, cx| {
                            this.start_merge_from_graph_branch(branch.clone(), window, cx);
                        }
                    })),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Rebase current branch on Branch\u{2026}",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.enter_rebase_mode_onto_branch(branch.clone(), cx);
                        }
                    })),
                )
                .child(render_graph_row_menu_header("Remote"))
                .child(
                    render_dropdown_menu_row(
                        "\u{2191}",
                        theme::button::BLUE_FG.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Push Branch\u{2026}",
                        "fast-forwards the remote branch".to_string(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.request_graph_push_branch(branch.clone(), cx);
                        }
                    })),
                )
                .child(render_graph_row_menu_header("Copy"))
                .child(
                    render_dropdown_menu_row(
                        "\u{ab}",
                        theme::text::SECONDARY.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Copy Branch Name to Clipboard",
                        String::new(),
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener({
                        let branch = branch.clone();
                        move |this, _event: &ClickEvent, _window, cx| {
                            this.copy_graph_text(branch.clone(), cx);
                        }
                    })),
                )
                .child(
                    div()
                        .px(px(11.0))
                        .pt(px(4.0))
                        .font(font(theme::font::SANS))
                        .text_size(px(9.5))
                        .text_color(theme::text::GHOSTER)
                        .child(
                            "check out, merge, rebase, push and delete all run in the focused \
                             worktree, never the main checkout",
                        ),
                ),
            )
            .into_any_element()
    }

    pub(in crate::graph_view) fn current_graph_row(&self, index: usize) -> Option<&GraphRow> {
        match &self.graph_state.load {
            GraphLoadState::Loaded(graph) => graph.rows.get(index),
            _ => None,
        }
    }

    /// The right panel while the graph tab is focused - replaces Files/Changes with Commit/
    /// Branches (design spec §5). Called from `crate::sidebar::render::AdeApp::
    /// render_right_sidebar` whenever `graph_tab_active` is `true`.
    pub(crate) fn render_graph_right_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // GitHub issue #242 phase B: the interactive-rebase mode's own Result panel (design spec
        // §1.6) replaces the ordinary Commit/Branches toggle entirely - short-circuiting here,
        // before `options`/`toggle` are even built, keeps every existing
        // `set_graph_right_panel(GraphRightPanel::Branches, ...)` call site (forcing the
        // Branches tab open on unrelated flows) working unchanged once rebase mode is left.
        if self.graph_state.rebase.is_some() {
            return self.render_rebase_result_panel(cx);
        }
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
            GraphRightPanel::Commit => self.render_graph_commit_panel(cx),
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

    fn render_graph_commit_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
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

        let panel = div()
            .id("graph-commit-panel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.graph_state.commit_panel_scroll_handle)
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
                    .child(
                        render_graph_footer_action_button(
                            "graph-commit-panel-cherry-pick",
                            "Cherry-pick",
                            theme::button::BLUE_FG.into(),
                        )
                        .on_click(cx.listener({
                            let sha = row.commit.id.clone();
                            move |this, _event: &ClickEvent, _window, cx| {
                                this.request_graph_cherry_pick(sha.clone(), cx);
                            }
                        })),
                    )
                    .child(
                        render_graph_footer_action_button(
                            "graph-commit-panel-revert",
                            "Revert",
                            theme::button::AMBER_FG.into(),
                        )
                        .on_click(cx.listener({
                            let sha = row.commit.id.clone();
                            move |this, _event: &ClickEvent, _window, cx| {
                                this.request_graph_revert(sha.clone(), cx);
                            }
                        })),
                    ),
            );
        // GitHub issue #142.
        div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .child(panel)
            .children(self.render_vertical_scrollbar(
                "graph-commit-panel-scrollbar",
                &self.graph_state.commit_panel_scroll_handle,
                &[],
                cx,
            ))
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
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("graph-branches-list")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.graph_state.branches_scroll_handle)
                            .children(branches.into_iter().map(|(name, kind, is_head, lane)| {
                                render_graph_branch_row(name, kind, is_head, lane, cx)
                            })),
                    )
                    // GitHub issue #142.
                    .children(self.render_vertical_scrollbar(
                        "graph-branches-scrollbar",
                        &self.graph_state.branches_scroll_handle,
                        &[],
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_graph_branches_filter_row(
        &self,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            // Caret placement, the empty/typed ordering and the flex structure around them all
            // through the one helper that owns them - see
            // `AdeApp::render_simple_input_row`'s own docs.
            .child(self.render_simple_input_row(SimpleInput {
                caret_selector: "graph-branches-filter-caret".into(),
                text_selector: "graph-branches-filter-text".into(),
                focus_handle: Some(&self.graph_state.branches_filter_focus_handle),
                text: self.graph_state.branches_filter.as_str(),
                placeholder: "filter branches",
                font: theme::font::MONO,
                text_size: px(10.5),
                text_color: theme::text::DIM,
                placeholder_color: theme::text::GHOST,
            }))
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

/// How many items [`AdeApp::render_graph_rows`]' `uniform_list` has: one per loaded commit row,
/// plus a trailing row while - and only while - a real "load more" walk is in flight. Shared by
/// the `item_count` argument and the row builder's own clamp so the two can never disagree about
/// where that row lives.
///
/// Note what is *not* here any more (GitHub issue #221): the walk being truncated no longer adds
/// an item of its own. A truncated walk is not a fact worth a permanent line of chrome - it is
/// simply history that has not been walked yet, and scrolling to it loads it.
fn graph_item_count(graph: &Graph, load_more_in_flight: bool) -> usize {
    graph.rows.len() + usize::from(load_more_in_flight)
}

/// The trailing "loading more commits" row, shown only while a real
/// [`AdeApp::load_more_graph_rows`] walk is genuinely running (GitHub issue #221) - it replaces
/// the former "showing the first 500 commits" notice, which stated a limit that no longer exists.
///
/// Deliberately the same "loading …" wording, font and colour the whole-tab loading state uses
/// (`AdeApp::render_graph_view`) rather than a new visual language, but left-aligned on the row
/// grid like the notice it replaces instead of centred, so it reads as the next item in the list
/// it is extending.
///
/// The final *item* of the row list rather than a sibling below it, which is what keeps it
/// scrolling with the rows it is talking about. That makes it subject to `uniform_list`'s fixed
/// row height, which is sized from item 0 alone, so it carries the same `theme::graph::ROW`
/// height every commit row does - a taller row would simply be clipped, with no panic and no
/// warning.
fn render_graph_load_more_row() -> impl IntoElement {
    div()
        .debug_selector(|| "graph-rows-load-more".to_string())
        .flex()
        .items_center()
        .w_full()
        .h(theme::graph::ROW)
        .px(px(12.0))
        .font(font(theme::font::SANS))
        .text_size(px(11.5))
        .text_color(theme::text::FAINT)
        .child("loading more commits\u{2026}")
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

/// The commit detail panel's own Cherry-pick/Revert buttons - a real, clickable twin of the
/// row menu's "Apply" section rows (`Self::render_graph_row_menu`) for the currently-selected
/// commit, since a commit already open in this panel is a real, common place to want the same
/// action from without reopening the `⋯` menu.
fn render_graph_footer_action_button(
    id: &'static str,
    label: &'static str,
    color: gpui::Rgba,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .cursor_pointer()
        .px(px(10.0))
        .py(px(5.0))
        .rounded(theme::radius::BUTTON)
        .border_1()
        .border_color(theme::border::BUTTON)
        .hover(move |el| el.bg(theme::surface::ROW_HOVER_ALT))
        .font(font(theme::font::SANS))
        .text_size(px(10.5))
        .text_color(color)
        .child(label)
}

/// One "Files changed" row - the change-row visual's spirit (git's own status letter + path),
/// simplified since a historical commit has no review checkbox or per-file stat counts to show.
///
/// The commit file list is the third of the three places `STAGE-A-CHANGELOG.md` §4j names as
/// having carried the old word badge, and like the Uncommitted rows it is a *list*, so the letter
/// sits in its fixed column ahead of the directory and every filename below it starts on the same
/// x - `Jerry.dc.html`'s own `gFiles` rows, in that order.
fn render_graph_file_row(file: wt_core::graph::CommitFileChange) -> impl IntoElement {
    let (dir, name) = changes::split_dir_name(&file.path);
    let letter = changes::status_letter(file.status);
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(theme::band::CHANGE_ROW)
        .child(
            render_status_letter(
                gpui::SharedString::from(format!("graph-file-status-{}", file.path.display())),
                letter,
                px(10.0),
            )
            .debug_selector({
                let path = file.path.clone();
                move || format!("graph-file-status-{}-{}", path.display(), letter.glyph())
            }),
        )
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
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .truncate()
                .font(font(theme::font::MONO))
                .text_size(px(11.5))
                .text_color(theme::text::STRONG)
                .child(name),
        )
}

/// One Branches-panel row (design spec §5): a lane-coloured dot, the branch name, and a `HEAD`
/// badge when it is the checked-out one.
///
/// Right-clicking it opens the branch context menu (GitHub issue #241), mirroring
/// `AdeApp::render_graph_row`'s own right-click handler exactly: `cx.stop_propagation()` so the
/// click reaches no ancestor handler, and the anchor resolved once from the real
/// `event.position`. The menu itself is a single window-level overlay
/// ([`AdeApp::render_graph_branch_menu`]) keyed by branch name, not a per-row child - a per-row
/// popover would be clipped by the panel's own scroll container and would repaint with the row.
fn render_graph_branch_row(
    name: String,
    kind: RefKind,
    is_head: bool,
    lane: usize,
    cx: &mut Context<AdeApp>,
) -> impl IntoElement {
    let dot_color: gpui::Rgba = if matches!(kind, RefKind::LocalBranch) {
        lane_color(lane)
    } else {
        theme::graph::BRANCH_NO_LANE_DOT.into()
    };
    div()
        .id(format!("graph-branch-row-{name}"))
        .debug_selector({
            let name = name.clone();
            move || format!("graph-branch-row-{name}")
        })
        // Gated on the row really being a *local* branch, the same way the commit row's own
        // right-click is gated on not being the synthetic working-tree row: every action this
        // menu offers (checkout, rename, delete, merge, rebase, push) is a local-branch
        // operation. The panel only ever lists local branches today
        // (`Self::render_graph_branches_panel` filters on `RefKind::LocalBranch`), so this is a
        // guard against that changing, not a live case - a remote branch row would silently get
        // a menu of operations that make no sense for it.
        .when(matches!(kind, RefKind::LocalBranch), |el| {
            el.on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener({
                    let name = name.clone();
                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_graph_branch_menu_at(
                            name.clone(),
                            event.position.x,
                            event.position.y,
                            window,
                            cx,
                        );
                    }
                }),
            )
        })
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

/// The order `render_graph_lane_canvas` paints one row's elbows in, paired with each elbow's own
/// index in `row.elbows` so its `debug_selector` tag stays tied to the layout's own numbering
/// rather than to paint position.
///
/// Every elbow of one kind in one row shares a single `waist_y` (see [`elbow_geometry`] - it is a
/// function of the row height and the kind, nothing else), so *N* elbows landing on the same lane
/// all paint their horizontal stroke on the very same pixel row, and their spans nest: the elbow
/// from the furthest lane covers the whole stretch every nearer one occupies, its own arc columns
/// included.
///
/// In `row.elbows`' own order that is fatal. `wt_core::graph::layout_lanes` Step 2 emits
/// `Converging` elbows by ascending lane, so the *furthest* lane is painted **last** and wipes out
/// every nearer elbow outright - not just its bridge but the arc that turns that lane's line onto
/// the waist. This repository's own history renders exactly that: master's `HEAD` is the shared
/// parent of eight worktree branches, so its row carries eight `Converging` elbows, and it painted
/// as one flat full-width bar in lane 8's colour with the other seven lanes' lines stopping dead at
/// the top of the row - a boxy rectangle where eight separate coloured curves belong. The
/// single-elbow geometry was never wrong (`elbow_geometry_tests` covers one elbow exhaustively and
/// all of it still passes); what was missing is that a row can hold more than one.
///
/// Painting longest span first and shortest last nests them the other way up, which is the shape a
/// commit graph actually wants: the horizontal changes colour at every lane it crosses, each lane's
/// own arc sits on top of the strokes running past it, and the innermost elbow owns the arc that
/// lands on the dot. `sort_by_key` is stable, so equal spans keep the layout's own order.
fn elbow_paint_order(elbows: &[wt_core::graph::Elbow]) -> Vec<(usize, wt_core::graph::Elbow)> {
    let mut ordered: Vec<(usize, wt_core::graph::Elbow)> =
        elbows.iter().copied().enumerate().collect();
    ordered.sort_by_key(|(_, elbow)| std::cmp::Reverse(elbow.from_lane.abs_diff(elbow.to_lane)));
    ordered
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
        // Pinned to the row's own top edge, never centred in it. `render_graph_row` used to carry
        // a permanent `border_b_1()` (removed - see that function's own docs for why a real
        // border there was itself the bug), and GPUI's taffy layout is *border-box*
        // (`Style::to_taffy`, `vendor/zed/crates/gpui/src/taffy.rs`, never sets `box_sizing`, so
        // taffy's own `BoxSizing::BorderBox` default applies) - so with that border present the
        // row was 26 tall on the outside but its content box was only 25, while this canvas is a
        // full `ROW` = 26. Under the row's `.items_center()` that centred to `(25 - 26) / 2` =
        // **-0.5px**, and every horizontal 1px stroke in here - the elbow bridge's `h(px(1.0))`
        // and both curve boxes' `border_t_1()`/`border_b_1()` - then landed on a half-pixel
        // boundary and rendered smeared across two physical pixel rows at half intensity, while
        // the vertical lane lines (x is untouched) stayed crisp. That was the reported "the lines
        // and elbows do not align correctly vertically", and it is also why four earlier rounds
        // of 1px elbow-seam fixes each only half-worked: the geometry was computed on an integer
        // grid and every test measured that integer grid, but the whole canvas was painted half a
        // pixel off it.
        //
        // `self_start()` is left in place now that the row's content box and border box are the
        // same 26px (no border on any edge at all): it costs nothing, and it keeps this position
        // correct - on whole pixels, `ROW` apart between consecutive rows - independent of
        // whatever the row's own box model happens to be, rather than silently relying on content
        // box and canvas height staying equal.
        .self_start()
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

    for (elbow_index, elbow) in elbow_paint_order(&row.elbows) {
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
    /// real commit rows (indices 0..=2, newest first), with no "Working tree" row to
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
    /// so unlike `crate::menu::model::menu_height` it has no analytical formula to
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
        // `leaving_the_graph_tab_from_the_branches_filter_lands_on_the_real_agent_pane` (in
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
        let (expected_x, expected_y) = crate::menu::model::clamp_menu_origin(
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

    /// GitHub issue #176 generalised the pairwise rule above into one shared invariant across all
    /// six menu surfaces (`crate::root::menus`). This is the part that pair could never cover: a
    /// real right-click on a graph row must also close a menu that has nothing to do with the
    /// graph - here the tab strip's `+`, whose own scrim ignores right-clicks entirely and so used
    /// to stay painted right alongside the row menu.
    #[gpui::test]
    fn opening_the_row_menu_closes_an_open_plus_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dropdown-menu-row-Git graph").is_some(),
            "premise: the + menu must really be painted before the right-click"
        );

        let row0 = cx.debug_bounds("graph-row-0").expect("row 0 painted");
        right_click(cx, row0.center());

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_some(),
                "premise: the right-click must really open the graph row menu"
            );
            assert!(
                !app.plus_menu_open,
                "and must close the + menu - two popovers open at once is GitHub issue #176"
            );
        });
        assert!(
            cx.debug_bounds("dropdown-menu-row-Git graph").is_none(),
            "the + menu must really stop painting, not merely have its flag cleared"
        );
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

        // Calls `leave_graph_tab` directly, not through `select_agent` - `select_agent` can
        // also route through `Self::select_worktree` (when the target agent belongs to a
        // worktree not already selected), which calls `Self::load_graph` unconditionally on its
        // own and would clear `row_menu_open` for an unrelated reason, confounding what this test
        // means to isolate: `leave_graph_tab`'s *own* clear, for the plain "leave the tab, same
        // worktree, same agent, tab was already loaded" path.
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

    /// The x-range one whole elbow paints on its own `waist_y` row. Both curve boxes contribute
    /// their *full* box width, not just the straight part of the border: a rounded corner's arc
    /// still terminates on the waist row at the far end of the corner square, so the whole width is
    /// ink. The bridge is included for completeness - it always sits between the two boxes.
    fn waist_ink(geo: &ElbowGeometry) -> (Pixels, Pixels) {
        let left = geo.entry.left.min(geo.exit.left).min(geo.straight.left);
        let right = geo
            .entry
            .right()
            .max(geo.exit.right())
            .max(geo.straight.left + geo.straight.width);
        (left, right)
    }

    fn column(x: Pixels) -> usize {
        f32::from(x).round() as usize
    }

    #[test]
    fn every_elbow_in_a_crowded_row_keeps_its_own_lanes_arc_visible() {
        // The real regression test for a row carrying *several* elbows at once - the case every
        // other test in this module misses, because they all build exactly one elbow.
        //
        // `elbow_geometry`'s `waist_y` depends only on the row height and the elbow kind, so every
        // elbow of one kind in one row paints its horizontal stroke on the very same pixel row, and
        // their spans nest: an elbow reaching lane 8 covers every column an elbow reaching lane 1
        // occupies, that nearer elbow's own arc included. Painted in `row.elbows`' own order
        // (`layout_lanes` emits `Converging` by ascending lane) the furthest one lands last and
        // erases all the rest - which is exactly what this repository's own history rendered:
        // master's `HEAD` is the shared parent of eight worktree branches, and its row painted as
        // one flat full-width bar in the highest lane's colour with seven lanes' lines stopping
        // dead at the top of the row, instead of eight separate coloured curves.
        //
        // The invariant that has to hold, stated on painted pixels rather than on the sort itself:
        // after the whole row is painted in `elbow_paint_order`, the column of each elbow's own
        // *far* lane - the branch end that has to visibly turn onto the waist - still belongs to
        // that elbow. Under the old order every lane but the furthest fails this.
        for (kind, far_lanes, dot_lane) in [
            // Eight branch tips sharing one ancestor: this repository's own real `HEAD` row.
            (
                ElbowKind::Converging,
                vec![1usize, 2, 3, 4, 5, 6, 7, 8],
                0usize,
            ),
            // The mirror: an octopus merge opening several lanes from one dot.
            (ElbowKind::Diverging, vec![1, 2, 3, 4], 0),
            // Non-contiguous lanes, and one adjacent-lane elbow whose two boxes overlap.
            (ElbowKind::Converging, vec![1, 3, 6], 0),
        ] {
            let elbows: Vec<wt_core::graph::Elbow> = far_lanes
                .iter()
                .map(|&far| match kind {
                    ElbowKind::Converging => wt_core::graph::Elbow {
                        from_lane: far,
                        to_lane: dot_lane,
                        kind,
                    },
                    ElbowKind::Diverging => wt_core::graph::Elbow {
                        from_lane: dot_lane,
                        to_lane: far,
                        kind,
                    },
                })
                .collect();

            let lane_count = far_lanes.iter().max().expect("at least one lane") + 1;
            let mut owner: Vec<Option<usize>> =
                vec![None; column(graph_lane_canvas_width(lane_count))];
            for (index, elbow) in elbow_paint_order(&elbows) {
                let geo = elbow_geometry(
                    elbow.kind,
                    lane_x(elbow.from_lane),
                    lane_x(elbow.to_lane),
                    ROW_H,
                );
                let (left, right) = waist_ink(&geo);
                for cell in owner.iter_mut().take(column(right)).skip(column(left)) {
                    *cell = Some(index);
                }
            }

            for (index, (elbow, &far)) in elbows.iter().zip(far_lanes.iter()).enumerate() {
                assert_eq!(
                    owner[column(lane_x(far))],
                    Some(index),
                    "{kind:?} row {far_lanes:?}: lane {far}'s own arc column was painted over by \
                     elbow {:?} - that lane's line ends in mid-air instead of curving onto the \
                     waist, which is the boxy full-width bar this ordering exists to prevent",
                    owner[column(lane_x(far))]
                        .map(|winner| elbows[winner])
                        .expect("some elbow must own the column"),
                );
                let _ = elbow;
            }
        }
    }

    #[test]
    fn the_paint_order_keeps_each_elbows_own_layout_index_for_its_debug_tag() {
        // Reordering must not renumber the `graph-row-N-elbow-K-...` selectors: `K` is the elbow's
        // index in `GraphRow::elbows` (what `graph_elbow_render_tests` looks paint bounds up by),
        // not its position in the paint sequence.
        let elbows = vec![
            wt_core::graph::Elbow {
                from_lane: 1,
                to_lane: 0,
                kind: ElbowKind::Converging,
            },
            wt_core::graph::Elbow {
                from_lane: 5,
                to_lane: 0,
                kind: ElbowKind::Converging,
            },
        ];
        let ordered = elbow_paint_order(&elbows);
        assert_eq!(
            ordered.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![1, 0],
            "the widest elbow paints first, but both keep their own layout index"
        );
        for (index, elbow) in ordered {
            assert_eq!(elbow, elbows[index]);
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

    /// The real regression test for the reported "the lines and elbows do not align correctly
    /// vertically", and for a whole class of subpixel bugs the sibling tiling test below cannot
    /// see: it only ever compares one lane canvas against *another* lane canvas, so a constant
    /// offset shared by every row passes it untouched.
    ///
    /// `render_graph_row` used to be `.h(ROW).border_b_1()`, and GPUI's taffy layout is
    /// border-box (`Style::to_taffy` in `vendor/zed/crates/gpui/src/taffy.rs` never sets
    /// `box_sizing`, so taffy's `BoxSizing::BorderBox` default applies) - so with that border
    /// present the row's *content* box was 25px while the lane canvas is a full `ROW` = 26px.
    /// The row's own `.items_center()` then centred it to `(25 - 26) / 2` = -0.5px, and every
    /// horizontal 1px stroke in the canvas - the elbow's straight bridge and both curve boxes'
    /// horizontal borders - landed on a half-pixel boundary, rendering smeared over two physical
    /// pixel rows while the vertical lane lines stayed crisp. The row's bottom border is gone now
    /// (a real, reported design bug in its own right - see `render_graph_row`'s own docs), so
    /// content box and border box are equal and this could no longer occur even without
    /// `render_graph_lane_canvas`'s own `.self_start()` - which stays regardless, since it costs
    /// nothing and keeps this correct independent of the row's own box model.
    ///
    /// Asserted on the canvas' offset *within its own row*, and on the offset being a whole
    /// pixel - the two facts the centring broke, neither of which any existing test looked at.
    #[gpui::test]
    fn every_lane_canvas_sits_on_whole_pixels_at_its_own_rows_top_edge(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded(cx, seed_converging_history);
        let row_count = app.read_with(cx, |app, _| {
            let GraphLoadState::Loaded(graph) = &app.graph_state.load else {
                panic!("graph must have loaded");
            };
            graph.rows.len()
        });
        assert!(
            row_count >= 4,
            "sanity check: this fixture must produce several real rows, got {row_count}"
        );

        for index in 0..row_count {
            let row_selector: &'static str =
                Box::leak(format!("graph-row-{index}").into_boxed_str());
            let canvas_selector: &'static str =
                Box::leak(format!("graph-row-{index}-lane-canvas").into_boxed_str());
            let row = cx
                .debug_bounds(row_selector)
                .unwrap_or_else(|| panic!("{row_selector} must be painted"));
            let canvas = cx
                .debug_bounds(canvas_selector)
                .unwrap_or_else(|| panic!("{canvas_selector} must be painted"));

            assert_eq!(
                canvas.origin.y, row.origin.y,
                "row {index}'s lane canvas must start on its row's own top edge, not be centred \
                 in the shorter content box the row's bottom border leaves behind"
            );
            let y = f32::from(canvas.origin.y);
            assert_eq!(
                y,
                y.round(),
                "row {index}'s lane canvas sits at {y}, off the whole-pixel grid - every 1px \
                 horizontal stroke inside it renders smeared across two physical pixel rows"
            );
        }
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
    /// line an earlier round fixed), and neither is visible from geometry alone - it depends on
    /// the row's own real flex layout, not just the two numbers this test's own docs cite.
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

    /// `STAGE-A-CHANGELOG.md` §4j's third site: "Applied in all three places that carried a
    /// badge: the Uncommitted rows, the file header above the diff, and **the commit file list**."
    /// The selected commit here really touched `a.txt` and nothing else, so its one row must
    /// carry git's own `M`.
    #[gpui::test]
    fn the_commit_file_list_carries_gits_own_status_letter(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        // Row 0 is the newest commit (`second`), which modified `a.txt`.
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.selected_row),
            Some(0),
            "premise: the newest commit is selected on load"
        );
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("graph-file-status-a.txt-M").is_some(),
            "the commit's own file list states what git did to that file, in the same fixed \
             column the Uncommitted rows use - not the `new`/`del` word pill it replaced"
        );
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

    /// The reported "double border left and bottom" - GPUI's `Style::border_color` is one
    /// shared value for every edge of a single element (confirmed directly in `gpui`'s own
    /// `style.rs`: `border_color: Option<Hsla>`, not per-edge), so a row using both
    /// `border_b_1()` (its own permanent separator) and a conditional `border_l_2()` (its
    /// selection edge) on the *same* div used to have the second `.border_color()` call
    /// silently overwrite the first - recolouring the bottom separator to the selection colour
    /// too, on every selected row, everywhere this pattern was used. The fix is a real, separate
    /// child element for the left edge (`Self::render_graph_row`'s own docs). This proves that
    /// child exists and is genuinely unconditional - painted with the same bounds whether or not
    /// the row is selected, only its colour differs (which this test harness has no way to
    /// inspect directly, but a `debug_bounds` miss on the unselected row would mean the child is
    /// still conditionally created, i.e. this fix regressed back to the broken pattern).
    #[gpui::test]
    fn the_selection_edge_is_a_real_element_painted_regardless_of_selection(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.selected_row),
            Some(0),
            "premise: row 0 is auto-selected on load, so row 1 starts genuinely unselected"
        );

        let edge_unselected = cx.debug_bounds("graph-row-1-selection-edge").expect(
            "the selection-edge child must be painted even while row 1 is unselected - if \
                 it's `None` here, the edge is still only created `.when(selected, ...)`, the \
                 exact regression this test exists to catch",
        );

        app.update(cx, |app, cx| {
            app.select_graph_row(1, cx);
        });
        cx.run_until_parked();

        let edge_selected = cx
            .debug_bounds("graph-row-1-selection-edge")
            .expect("the selection-edge child must still be painted while row 1 is selected");

        assert_eq!(
            edge_unselected.origin, edge_selected.origin,
            "the selection edge's own position must never move - only its colour toggles \
             (unselected: {:?}, selected: {:?})",
            edge_unselected, edge_selected
        );
        assert_eq!(
            edge_unselected.size, edge_selected.size,
            "the selection edge's own size must never change (unselected: {:?}, selected: {:?})",
            edge_unselected, edge_selected
        );
    }

    /// GitHub issue #127: `AdeApp::graph_view_focused` - the bool `Self::render_graph_row`'s own
    /// selected-row highlight reads, since that render call chain never carries a real `&Window`
    /// to check `FocusHandle::is_focused` against directly - is set explicitly alongside every
    /// real `window.focus(&self.graph_focus_handle, ...)`/graph-tab-exit call site
    /// (`Self::open_git_graph`, `Self::open_graph_row_menu_at`, `Self::leave_graph_tab`), not via
    /// a `cx.on_focus` subscription - one was tried and, live-tested, never fired for
    /// `graph_focus_handle`, since it's only ever `track_focus`'d conditionally
    /// (`Self::render_center_pane` renders `Self::render_graph_view` only while `graph_tab_active`
    /// is `true`). Direct coverage of every site that flips the bool.
    #[gpui::test]
    fn graph_view_focused_tracks_real_focus_and_blur_of_the_graph_view(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        assert!(
            app.read_with(cx, |app, _| app.graph_view_focused),
            "premise: opening the graph tab genuinely focuses the graph view"
        );

        app.update_in(cx, |app, window, cx| {
            app.leave_graph_tab(window, cx);
        });
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.graph_view_focused),
            "leaving the graph tab must clear it"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.graph_view_focused),
            "reopening the graph tab must set it again"
        );

        app.update_in(cx, |app, window, cx| {
            app.leave_graph_tab(window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_graph_row_menu_at(0, px(0.0), px(0.0), window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.graph_view_focused),
            "opening a row's context menu must also set it, since it explicitly refocuses \
             graph_focus_handle to fix keyboard focus after a right-click's own stop_propagation"
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
        // close (GitHub issue #26 already wired this for file and agent tabs via
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
            "middle-clicking the graph tab must close it, same as a file or agent tab"
        );
    }

    /// GitHub issue #45 ("Input blink only on focused input or file") plus a live follow-up
    /// report: this filter row had *no* caret element at all - confirmed by reading
    /// `render_graph_branches_filter_row` before this fix, and `branches_filter_focus_handle`
    /// was never threaded through `AdeApp::wire_caret_blink` either (it didn't exist yet when
    /// that call in `Self::new_with_settings` was first wired - the Branches panel is a later
    /// Revision R12 addition). Real interaction coverage, mirroring
    /// `crate::rail::render::rail_filter_caret_tests`' own measured-bounds technique.
    #[gpui::test]
    fn caret_sits_before_the_placeholder_when_empty_and_after_the_text_once_typed(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
            app.set_graph_right_panel(GraphRightPanel::Branches, cx);
            window.focus(&app.graph_state.branches_filter_focus_handle, cx);
        });
        cx.run_until_parked();

        let empty_caret = cx.debug_bounds("graph-branches-filter-caret").expect(
            "the caret should have really painted with an empty filter - before this fix, \
                 no caret element existed here at all, so this selector would never resolve",
        );
        let placeholder = cx
            .debug_bounds("graph-branches-filter-text")
            .expect("the placeholder text should have really painted");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "with an empty filter, the real caret must sit before (at or left of) the \
             placeholder's own start x, not after it - got caret {:?} vs placeholder {:?}",
            empty_caret,
            placeholder,
        );

        cx.simulate_input("main");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .branches_filter
                .as_str()
                .to_string()),
            "main",
            "sanity check: real typed filter"
        );

        let typed_caret = cx
            .debug_bounds("graph-branches-filter-caret")
            .expect("the caret should have really painted with a typed filter");
        let typed_text = cx
            .debug_bounds("graph-branches-filter-text")
            .expect("the real typed text should have really painted");
        assert!(
            typed_caret.origin.x >= typed_text.origin.x + typed_text.size.width,
            "with a typed filter, the real caret must sit at or after the typed text's own \
             right edge, not before it - got caret {:?} vs text {:?}",
            typed_caret,
            typed_text,
        );
        assert!(
            typed_caret.origin.x > empty_caret.origin.x,
            "the caret's real measured horizontal position must differ between the \
             empty-filter state (before the placeholder) and a typed-filter state (after the \
             real text) - got {:?} vs {:?}",
            empty_caret.origin.x,
            typed_caret.origin.x,
        );
    }

    /// GitHub issue #45's own title, taken literally: before this fix,
    /// `branches_filter_focus_handle` was never wired into `AdeApp::wire_caret_blink` at all, so
    /// *blurring* it never called the real [`AdeApp::stop_caret_blink`] that pins the shared
    /// flag straight to "dimmed" the instant focus leaves - the same real, immediate effect
    /// every already-wired handle has always had.
    ///
    /// This deliberately checks *blur*, not *focus*: typing into the field also calls
    /// `Self::reset_caret_blink` directly (`Self::handle_branches_filter_key_down`'s own real
    /// "solid mid-keystroke" behaviour, GitHub issue #27) regardless of whether
    /// `branches_filter_focus_handle` is wired into the shared loop at all - so a test that only
    /// focuses-then-types would pass even with the wiring gap this fix closes, and prove
    /// nothing about it. Blurring is the one real effect that only a genuine `AdeApp::
    /// wire_caret_blink` subscription produces: an *immediate*, synchronous flip to
    /// `caret_blink_visible == false`, not something that could be explained by the still-live
    /// timer from the earlier keystroke (which hasn't been given time to fire - the background
    /// clock is never advanced in this test).
    #[gpui::test]
    fn blurring_the_branches_filter_stops_the_real_shared_blink_loop(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded(cx);
        app.update_in(cx, |_app, window, _cx| window.activate_window());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
            app.set_graph_right_panel(GraphRightPanel::Branches, cx);
            window.focus(&app.graph_state.branches_filter_focus_handle, cx);
        });
        // `open_git_graph` kicks off a real, async `load_graph` (a background `gix` commit
        // walk) - the Branches panel only actually renders its filter row (rather than the
        // "open the graph to see branches" placeholder `render_graph_branches_panel` shows
        // while `current_graph()` is still `None`) once that finishes. Parked here first, the
        // same way the sibling position test above does, so `cx.simulate_input` below dispatches
        // against a frame that genuinely has the filter row - and therefore its focus - in it.
        cx.run_until_parked();

        cx.simulate_input("m");
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "sanity check: typing must leave the caret solid/visible, whether that came from \
             this fix's own on-focus wiring or `handle_branches_filter_key_down`'s pre-existing \
             `reset_caret_blink` call"
        );

        // Move real keyboard focus to a genuinely different, real, *unwired* handle
        // (`AdeApp::rail_focus_handle`, the rail's own always-present container fallback, never
        // itself a caret-bearing input) - and force the same real redraw a live blur event
        // needs to be observed (`rail::render::rail_filter_caret_tests`' own identical
        // `cx.simulate_input`-forces-a-redraw finding), via a harmless, unbound plain keystroke
        // rather than typing into a real field.
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.rail_focus_handle, cx);
        });
        cx.simulate_keystrokes("x");

        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "blurring the branches filter must have immediately pinned the shared caret-blink \
             flag off (`AdeApp::stop_caret_blink`) - before this fix, \
             `branches_filter_focus_handle` was never threaded through \
             `AdeApp::wire_caret_blink`, so nothing would have reacted to it losing focus at \
             all, and the caret would have stayed solid/visible until whatever timer the \
             earlier keystroke started eventually fired on its own"
        );
    }

    /// GitHub issue #90 (independent audit, second round): a genuinely empty window has no real
    /// repo to graph at all. `mod+shift+G` is bound with no key context - see
    /// `crate::default_key_bindings`'s own docs - so it dispatches on the root element regardless
    /// of whether a repo is focused, reaching `AdeApp::open_git_graph` directly. Before this fix,
    /// that call would move real keyboard focus onto `graph_focus_handle` - a handle only ever
    /// tracked inside `Self::render_center_pane` (part of `Self::render_workspace_body`, never
    /// rendered while `Self::render_empty_state` is showing instead) - dangling it exactly like
    /// the `empty_state_focus_handle` bug `AdeApp::open_repo_in_current_window`'s own
    /// `forget_target` calls fix - and would load a real graph from `self.diff_root`, an empty
    /// `PathBuf` in this state, which `gix::open` silently resolves relative to the process's own
    /// real working directory.
    #[gpui::test]
    fn open_git_graph_is_a_no_op_with_no_focused_repo(cx: &mut TestAppContext) {
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                crate::settings::store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo(), None, "sanity check: starts empty");
        });

        let empty_state_handle = app.read_with(cx, |app, _| app.empty_state_focus_handle.clone());
        app.update_in(cx, |app, window, cx| {
            window.focus(&empty_state_handle, cx);
            app.handle_new_git_graph_action(&NewGitGraph, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                !app.graph_tab_open,
                "mod+shift+G with no focused repo must not open the graph tab"
            );
            assert!(!app.graph_tab_active);
            assert!(
                matches!(app.graph_state.load, GraphLoadState::NotLoaded),
                "no real graph load may be triggered against an empty/wrong repo path"
            );
        });
        let focused = app.update_in(cx, |_app, window, cx| window.focused(cx));
        assert_eq!(
            focused,
            Some(empty_state_handle),
            "focus must never move off the still-rendered empty-state handle onto the \
             unrendered graph view"
        );
    }
}

/// Real interaction coverage for GitHub issue #1's "push (force with lease, force, no force)"/
/// "pull" acceptance criteria - `AdeApp::request_graph_fetch`/`request_graph_pull`/
/// `request_graph_push` really shelling out to `wt_core::remote`, not just the not-yet-wired
/// stub these replaced.
#[cfg(test)]
mod graph_remote_action_tests {
    use crate::root::focus::palette_focus_tests;
    use crate::root::AdeApp;
    use gpui::{Entity, TestAppContext};
    use std::path::Path;
    use wt_core::remote::PushForce;

    fn git(dir: &Path, args: &[&str]) {
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

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
    }

    /// A real bare remote plus a real clone of it, with a real committer identity and one
    /// tracked file - the app is opened against the clone, matching how a real user's worktree
    /// would already have a configured upstream.
    fn open_seeded_with_remote(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let remote = tempfile::tempdir().expect("tempdir");
        git(remote.path(), &["init", "--bare", "-b", "main"]);

        let seed = tempfile::tempdir().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = tempfile::tempdir().expect("tempdir");
        git(
            local.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, local.path().to_path_buf());
        (remote, local, app, cx)
    }

    #[gpui::test]
    async fn fetch_really_updates_the_remote_tracking_ref_and_the_status_line(
        cx: &mut TestAppContext,
    ) {
        let (remote, local, app, cx) = open_seeded_with_remote(cx);

        // Advance the remote past what the local clone knows about, so fetch has something real
        // to do.
        let seed_dir = tempfile::tempdir().expect("tempdir");
        git(
            seed_dir.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(
            seed_dir.path(),
            &["config", "user.email", "test@example.com"],
        );
        git(seed_dir.path(), &["config", "user.name", "Test User"]);
        commit(seed_dir.path(), "b.txt", "1", "second");
        git(seed_dir.path(), &["push", "origin", "main"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_fetch(cx);
        });
        cx.run_until_parked();

        let remote_tracking_subject =
            git_output(local.path(), &["log", "-1", "--format=%s", "origin/main"]);
        assert_eq!(
            remote_tracking_subject, "second",
            "the real click must have run a real git fetch that updated the remote-tracking ref"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Fetch".to_string()),
            "a successful fetch must report real success, not the old 'not implemented yet' \
             stub text"
        );
        assert!(
            !app.read_with(cx, |app, _| app.graph_state.remote_op_in_flight),
            "the in-flight guard must clear once the real fetch completes"
        );
    }

    #[gpui::test]
    async fn plain_push_runs_on_a_single_click_and_really_updates_the_remote(
        cx: &mut TestAppContext,
    ) {
        let (remote, local, app, cx) = open_seeded_with_remote(cx);
        commit(local.path(), "b.txt", "1", "local work");

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_push(PushForce::None, cx);
        });
        cx.run_until_parked();

        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "local work",
            "a plain Push click must run immediately and really update the remote - it can \
             only ever fast-forward, so it must never require the two-click confirmation"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.push_force_confirm_armed),
            None,
            "a plain push must never arm the force-confirmation state"
        );
    }

    #[gpui::test]
    async fn force_push_requires_a_real_second_click_on_the_same_variant(cx: &mut TestAppContext) {
        let (remote, local, app, cx) = open_seeded_with_remote(cx);
        // Diverge local history from the remote so a plain push would be a non-fast-forward -
        // the real case Force exists for.
        commit(local.path(), "b.txt", "1", "local work");
        git(
            local.path(),
            &["commit", "--amend", "-m", "amended local work"],
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_push(PushForce::Force, cx);
        });
        cx.run_until_parked();

        let remote_subject_after_first_click =
            git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject_after_first_click, "base",
            "the first click on Force must only arm the confirmation, never touch the real \
             remote"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.push_force_confirm_armed),
            Some(PushForce::Force),
            "the first click must arm exactly the Force variant that was clicked"
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_push(PushForce::Force, cx);
        });
        cx.run_until_parked();

        let remote_subject_after_second_click =
            git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject_after_second_click, "amended local work",
            "the second click on the same armed variant must really run the force push"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.push_force_confirm_armed),
            None,
            "the confirmation must disarm once the real push has actually run"
        );
    }

    #[gpui::test]
    async fn clicking_a_different_row_disarms_the_previous_confirmation(cx: &mut TestAppContext) {
        let (remote, local, app, cx) = open_seeded_with_remote(cx);
        commit(local.path(), "b.txt", "1", "local work");
        git(
            local.path(),
            &["commit", "--amend", "-m", "amended local work"],
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_push(PushForce::WithLease, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.push_force_confirm_armed),
            Some(PushForce::WithLease),
            "premise: With lease is armed by its own first click"
        );

        // A click on the *other* danger row must not accidentally inherit the first row's arm -
        // switching rows disarms rather than carrying over a confirmation the user never gave
        // for this specific variant.
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_push(PushForce::Force, cx);
        });
        cx.run_until_parked();

        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "base",
            "switching rows must never let one row's confirmation authorize a different row's \
             push"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.push_force_confirm_armed),
            Some(PushForce::Force),
            "the click must have armed the newly-clicked Force row instead"
        );
    }

    #[gpui::test]
    async fn pull_surfaces_a_real_conflict_as_a_real_status_message_not_a_crash(
        cx: &mut TestAppContext,
    ) {
        let (remote, local, app, cx) = open_seeded_with_remote(cx);

        let seed_dir = tempfile::tempdir().expect("tempdir");
        git(
            seed_dir.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(
            seed_dir.path(),
            &["config", "user.email", "test@example.com"],
        );
        git(seed_dir.path(), &["config", "user.name", "Test User"]);
        commit(seed_dir.path(), "a.txt", "remote change", "remote diverges");
        git(seed_dir.path(), &["push", "origin", "main"]);
        commit(local.path(), "a.txt", "local change", "local diverges");

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_pull(cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Pull failed:")),
            "a real conflicting pull must surface as a real, visible failure message, not a \
             silent success or a panic - got {status:?}"
        );
        assert!(
            !app.read_with(cx, |app, _| app.graph_state.remote_op_in_flight),
            "the in-flight guard must still clear even when the operation fails"
        );
    }

    /// A real local repo (no remote needed) with a `main` branch and a `feature` branch that
    /// diverged from it - `Self::request_graph_cherry_pick`/`Self::request_graph_revert`'s own
    /// tests only ever need one worktree, unlike the push/pull tests above.
    fn open_seeded_local_repo(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let local = tempfile::tempdir().expect("tempdir");
        git(local.path(), &["init", "-b", "main"]);
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);
        commit(local.path(), "a.txt", "base", "base");

        let (app, cx) = palette_focus_tests::open_test_app(cx, local.path().to_path_buf());
        (local, app, cx)
    }

    #[gpui::test]
    async fn cherry_pick_really_applies_the_commit_and_reports_success(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        git(local.path(), &["checkout", "-b", "feature"]);
        commit(local.path(), "b.txt", "feature content", "feature work");
        let feature_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        git(local.path(), &["checkout", "main"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_cherry_pick(feature_sha, cx);
        });
        cx.run_until_parked();

        let head_subject = git_output(local.path(), &["log", "-1", "--format=%s"]);
        assert_eq!(
            head_subject, "feature work",
            "the real click must have run a real git cherry-pick onto the current branch"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Cherry-pick".to_string()),
            "a successful cherry-pick must report real success, not the old 'not implemented \
             yet' stub text"
        );
        assert!(
            !app.read_with(cx, |app, _| app.graph_state.remote_op_in_flight),
            "the in-flight guard must clear once the real cherry-pick completes"
        );
    }

    #[gpui::test]
    async fn cherry_pick_a_real_conflict_surfaces_as_a_real_status_message_not_a_crash(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        git(local.path(), &["checkout", "-b", "feature"]);
        commit(local.path(), "a.txt", "feature change", "feature work");
        let feature_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        git(local.path(), &["checkout", "main"]);
        commit(
            local.path(),
            "a.txt",
            "conflicting main change",
            "main diverges",
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_cherry_pick(feature_sha, cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Cherry-pick failed:")),
            "a real conflicting cherry-pick must surface as a real, visible failure message, \
             not a silent success or a panic - got {status:?}"
        );
        assert!(
            local.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "the worktree must be left in the real conflicted state for the user to resolve"
        );
    }

    #[gpui::test]
    async fn revert_really_creates_an_undo_commit_and_reports_success(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        commit(local.path(), "a.txt", "changed", "the change to undo");
        let to_revert = git_output(local.path(), &["rev-parse", "HEAD"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_revert(to_revert, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(local.path().join("a.txt")).expect("read a.txt"),
            "base",
            "the real click must have run a real git revert restoring the file's prior content"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Revert".to_string()),
            "a successful revert must report real success, not the old 'not implemented yet' \
             stub text"
        );
    }

    /// Opens the graph tab and resolves `sha`'s own loaded row index - exactly what a real click
    /// on that row's "Rebase onto this commit" entry hands `AdeApp::enter_rebase_mode`, so these
    /// tests drive the same entry point the row menu does rather than a test-only shortcut.
    fn graph_row_index_of(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        sha: &str,
    ) -> usize {
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| match &app.graph_state.load {
            crate::graph_view::GraphLoadState::Loaded(graph) => graph
                .rows
                .iter()
                .position(|row| row.commit.id == sha)
                .unwrap_or_else(|| panic!("commit {sha} must be a real loaded graph row")),
            other => panic!("the graph must be loaded to click a row, got {other:?}"),
        })
    }

    /// GitHub issue #241 folded the row menu's two rebase entries into one: "Rebase onto this
    /// commit" now opens the Planning banner, whose own `Start rebase` runs the replay. The
    /// replay itself is what this proves - the same history a plain `git rebase <onto>` produces.
    #[gpui::test]
    async fn rebase_onto_really_replays_the_branch_and_reports_success(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        // A second branch line the app's own worktree branch will be rebased onto.
        git(local.path(), &["checkout", "-b", "target-branch"]);
        commit(local.path(), "b.txt", "target content", "target advances");
        let target_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        git(local.path(), &["checkout", "main"]);
        commit(local.path(), "c.txt", "own content", "own work");

        let target_row = graph_row_index_of(&app, cx, &target_sha);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(target_row, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        let log = git_output(local.path(), &["log", "--format=%s"]);
        assert_eq!(
            log, "own work\ntarget advances\nbase",
            "the real click must have run a real git rebase replaying this worktree's own \
             commit on top of the target branch's tip"
        );
        // A real `Completed` outcome leaves rebase mode entirely - there is deliberately no
        // leftover mode or banner to dismiss after a clean rebase.
        assert!(
            app.read_with(cx, |app, _| app.graph_state.rebase.is_none()),
            "a cleanly completed rebase must leave no rebase mode behind"
        );
        assert!(
            !local.path().join(".git/rebase-merge").exists(),
            "a cleanly completed rebase must leave no in-progress rebase state on disk"
        );
    }

    /// GitHub issue #241's real fix for this action, and the reason it moved onto the engine at
    /// all: rebasing onto a commit used to report `Rebase failed: …` and stop there, leaving
    /// `.git/rebase-merge/` on disk with **nothing in this app able to continue, skip or abort
    /// it**. It now stops in [`RebasePhase::Stopped`], whose banner carries real
    /// `Continue`/`Skip`/`Abort` and `Resolve in the diff view`.
    #[gpui::test]
    async fn rebase_onto_a_real_conflict_stops_in_the_recoverable_rebase_mode_not_a_dead_end(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        git(local.path(), &["checkout", "-b", "target-branch"]);
        commit(local.path(), "a.txt", "target change", "target advances");
        let target_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        git(local.path(), &["checkout", "main"]);
        commit(local.path(), "a.txt", "conflicting own change", "own work");

        let target_row = graph_row_index_of(&app, cx, &target_sha);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(target_row, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        let conflicted_files = app.read_with(cx, |app, _| {
            match app.graph_state.rebase.as_ref().map(|rs| &rs.phase) {
                Some(crate::graph_view::rebase::RebasePhase::Stopped {
                    outcome:
                        wt_core::rebase::RebaseOutcome::StoppedForConflict {
                            conflicted_files, ..
                        },
                }) => Some(conflicted_files.clone()),
                other => panic!(
                    "a real conflicting rebase must stop in the recoverable rebase mode with a \
                     real StoppedForConflict outcome - got {other:?}"
                ),
            }
        });
        assert_eq!(
            conflicted_files,
            Some(vec![std::path::PathBuf::from("a.txt")]),
            "the stop must name the real conflicted file, so `Resolve in the diff view` has \
             something real to open"
        );
        assert!(
            local.path().join(".git/rebase-merge").exists(),
            "the worktree must genuinely still be mid-rebase - the recovery banner is what makes \
             that state actionable rather than a dead end"
        );

        // And the recovery really works: the banner's own `Abort` restores the branch.
        app.update_in(cx, |app, _window, cx| {
            app.abort_rebase(cx);
        });
        cx.run_until_parked();
        assert!(
            !local.path().join(".git/rebase-merge").exists(),
            "aborting from the stopped banner must genuinely end the in-progress rebase"
        );
        assert_eq!(
            git_output(local.path(), &["log", "--format=%s"]),
            "own work\nbase",
            "an abort must restore the branch exactly as it was before the rebase started"
        );
        assert!(
            app.read_with(cx, |app, _| app.graph_state.rebase.is_none()),
            "aborting must leave rebase mode"
        );
    }

    /// Entering rebase mode must never start a second one over a live one - the first one's
    /// banner is the only surface that can recover it. The row menu itself is unreachable while
    /// rebase mode is showing, so this covers `enter_rebase_mode_inner`'s own backstop guard.
    #[gpui::test]
    async fn rebase_onto_refuses_while_a_rebase_mode_is_already_live(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        git(local.path(), &["checkout", "-b", "target-branch"]);
        commit(local.path(), "a.txt", "target change", "target advances");
        let target_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        git(local.path(), &["checkout", "main"]);
        commit(local.path(), "a.txt", "conflicting own change", "own work");
        let other_sha = git_output(local.path(), &["rev-parse", "HEAD"]);

        // Both row indices resolved up front, off the same loaded graph - the stopped rebase
        // below can genuinely reload and renumber rows underneath them.
        let target_row = graph_row_index_of(&app, cx, &target_sha);
        let other_row = graph_row_index_of(&app, cx, &other_sha);

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(target_row, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();
        let onto_before = app.read_with(cx, |app, _| {
            app.graph_state
                .rebase
                .as_ref()
                .map(|rs| rs.onto.clone())
                .expect("stopped rebase mode is live")
        });

        // A second entry on a different commit while the first is still stopped.
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(other_row, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .rebase
                .as_ref()
                .map(|rs| rs.onto.clone())),
            Some(onto_before),
            "the live rebase mode must be left exactly as it was, never replaced by a second one"
        );

        app.update_in(cx, |app, _window, cx| {
            app.abort_rebase(cx);
        });
        cx.run_until_parked();
        assert!(!local.path().join(".git/rebase-merge").exists());
    }

    // GitHub issue #241: "Check out" / "Create branch here" / Soft-Mixed-Hard reset.

    #[gpui::test]
    async fn checkout_really_moves_head_and_reports_success(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        commit(local.path(), "b.txt", "second", "second commit");

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_checkout(base_sha.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(local.path(), &["rev-parse", "HEAD"]),
            base_sha,
            "the real click must have run a real git checkout onto the target commit"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Check out".to_string()),
            "a successful checkout must report real success, not the old 'not implemented yet' \
             stub text"
        );
        assert!(
            !app.read_with(cx, |app, _| app.graph_state.remote_op_in_flight),
            "the in-flight guard must clear once the real checkout completes"
        );
    }

    #[gpui::test]
    async fn checkout_on_a_dirty_worktree_with_a_real_conflict_surfaces_gits_own_refusal(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        git(local.path(), &["checkout", "-b", "other"]);
        commit(
            local.path(),
            "a.txt",
            "other branch content",
            "other change",
        );
        git(local.path(), &["checkout", "main"]);
        // An uncommitted change that genuinely conflicts with what "other" holds - real git
        // refuses to silently clobber it, and this makes no attempt to pre-check that itself.
        std::fs::write(local.path().join("a.txt"), "uncommitted dirty content")
            .expect("write a.txt");

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_checkout("other".to_string(), cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Check out failed:")),
            "a real conflicting checkout must surface as a real, visible failure message, not a \
             silent success or a panic - got {status:?}"
        );
        assert_eq!(
            git_output(local.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main",
            "a refused checkout must leave the worktree exactly where it was"
        );
        assert_eq!(
            std::fs::read_to_string(local.path().join("a.txt")).expect("read a.txt"),
            "uncommitted dirty content",
            "the refused checkout must not have touched the dirty file"
        );
    }

    #[gpui::test]
    async fn create_branch_here_really_creates_and_switches_and_reports_success(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        commit(local.path(), "b.txt", "second", "second commit");

        app.update_in(cx, |app, window, cx| {
            app.start_graph_create_branch(
                base_sha.clone(),
                base_sha[..7].to_string(),
                "base".to_string(),
                window,
                cx,
            );
            app.graph_state.branch_prompt_name =
                crate::text_history::TextField::seeded("feature-x");
            app.commit_graph_branch_prompt(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(local.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feature-x",
            "must really switch onto the newly created branch"
        );
        assert_eq!(
            git_output(local.path(), &["rev-parse", "HEAD"]),
            base_sha,
            "the new branch must really be rooted at the commit the prompt was opened on"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Create branch".to_string()),
            "a successful create-branch must report real success, not the old 'not implemented \
             yet' stub text"
        );
        assert!(
            app.read_with(cx, |app, _| app.graph_state.branch_prompt.is_none()),
            "the prompt must close once the branch has really been created"
        );
    }

    #[gpui::test]
    async fn branch_prompt_rejects_an_empty_name_without_touching_git(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);

        app.update_in(cx, |app, window, cx| {
            app.start_graph_create_branch(
                base_sha.clone(),
                base_sha[..7].to_string(),
                "base".to_string(),
                window,
                cx,
            );
            // The field starts genuinely empty - commit without typing anything, the same real
            // "just hit enter" case a hand-rolled empty-name guard exists for.
            app.commit_graph_branch_prompt(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .branch_prompt
                .as_ref()
                .and_then(|prompt| prompt.error.clone())),
            Some("branch name can't be empty".to_string()),
            "an empty name must leave the prompt open with a real, visible rejection, not \
             silently close it or invoke git at all"
        );
        assert_eq!(
            git_output(local.path(), &["branch", "--list"]),
            "* main",
            "no branch must have been created for an empty name - this is exactly the \
             'clearly-broken invocation' the empty-name guard exists to avoid, not a real git \
             error"
        );
    }

    #[gpui::test]
    async fn create_branch_with_a_colliding_name_surfaces_gits_own_real_error(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        git(local.path(), &["branch", "existing-branch"]);

        app.update_in(cx, |app, window, cx| {
            app.start_graph_create_branch(
                base_sha.clone(),
                base_sha[..7].to_string(),
                "base".to_string(),
                window,
                cx,
            );
            app.graph_state.branch_prompt_name =
                crate::text_history::TextField::seeded("existing-branch");
            app.commit_graph_branch_prompt(window, cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Create branch failed:")
                    && text.contains("already exists")),
            "a real branch-name collision must surface git's own real error, not hand-rolled \
             validation - got {status:?}"
        );
        assert_eq!(
            git_output(local.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main",
            "a failed create-branch must not have switched anything"
        );
    }

    #[gpui::test]
    async fn soft_reset_moves_the_branch_tip_and_keeps_the_change_staged(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        commit(local.path(), "a.txt", "changed", "second commit");

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_reset(wt_core::checkout::ResetMode::Soft, base_sha.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(git_output(local.path(), &["rev-parse", "HEAD"]), base_sha);
        assert_eq!(
            git_output(local.path(), &["diff", "--cached", "--name-only"]),
            "a.txt",
            "a soft reset must leave the undone commit's own change staged"
        );
        assert_eq!(
            std::fs::read_to_string(local.path().join("a.txt")).expect("read a.txt"),
            "changed",
            "the working tree content must be untouched by a soft reset"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Soft reset".to_string())
        );
    }

    #[gpui::test]
    async fn mixed_reset_moves_the_branch_tip_and_unstages_the_change(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        commit(local.path(), "a.txt", "changed", "second commit");

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_reset(wt_core::checkout::ResetMode::Mixed, base_sha.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(git_output(local.path(), &["rev-parse", "HEAD"]), base_sha);
        assert!(
            git_output(local.path(), &["diff", "--cached", "--name-only"]).is_empty(),
            "a mixed reset must leave nothing staged"
        );
        assert_eq!(
            git_output(local.path(), &["diff", "--name-only"]),
            "a.txt",
            "the undone commit's own change must land unstaged instead"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Mixed reset".to_string())
        );
    }

    #[gpui::test]
    async fn hard_reset_requires_a_real_second_click_before_it_runs(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        commit(local.path(), "a.txt", "changed", "second commit");
        let tip_sha = git_output(local.path(), &["rev-parse", "HEAD"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_reset(wt_core::checkout::ResetMode::Hard, base_sha.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(local.path(), &["rev-parse", "HEAD"]),
            tip_sha,
            "the first click on Hard must only arm the confirmation, never touch real git state"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .hard_reset_confirm_armed
                .clone()),
            Some(base_sha.clone()),
            "the first click must arm exactly the commit that was clicked"
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_reset(wt_core::checkout::ResetMode::Hard, base_sha.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(local.path(), &["rev-parse", "HEAD"]),
            base_sha,
            "the second click on the same armed commit must really run the hard reset"
        );
        assert_eq!(
            std::fs::read_to_string(local.path().join("a.txt")).expect("read a.txt"),
            "base",
            "a hard reset must really restore the working tree to the target commit's content"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .hard_reset_confirm_armed
                .clone()),
            None,
            "the confirmation must disarm once the real reset has actually run"
        );
    }

    #[gpui::test]
    async fn a_different_row_action_disarms_a_previously_armed_hard_reset(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_local_repo(cx);
        let base_sha = git_output(local.path(), &["rev-parse", "HEAD"]);
        commit(local.path(), "a.txt", "changed", "second commit");
        let tip_sha = git_output(local.path(), &["rev-parse", "HEAD"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_reset(wt_core::checkout::ResetMode::Hard, base_sha.clone(), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .hard_reset_confirm_armed
                .clone()),
            Some(base_sha.clone()),
            "premise: Hard is armed by its own first click"
        );

        // A click on a *different* row-menu action (here, a harmless clipboard copy - no git
        // mutation at all) must disarm rather than let a later, unrelated second click on Hard
        // silently ride on this stale confirmation. Mirrors
        // `clicking_a_different_row_disarms_the_previous_confirmation`'s identical premise for
        // the Push menu's own force-confirmation.
        app.update_in(cx, |app, _window, cx| {
            app.copy_graph_text("unrelated".to_string(), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .hard_reset_confirm_armed
                .clone()),
            None,
            "a different action must disarm the previous Hard confirmation"
        );

        // With the arm cleared, a further click on Hard must arm again from scratch rather than
        // execute immediately.
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_reset(wt_core::checkout::ResetMode::Hard, base_sha.clone(), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            git_output(local.path(), &["rev-parse", "HEAD"]),
            tip_sha,
            "the disarmed confirmation must require a fresh first click before Hard can run again"
        );
    }
}

/// Real, live-rendered proof that the commit row list is genuinely virtualized (GitHub issue
/// #218) - that a row scrolled far below the viewport is not merely *invisible* but never becomes
/// a painted element at all.
///
/// This is the property the whole fix rests on, and none of it is observable from the pure
/// `wt_core::graph` logic: `build_graph` returns exactly the same rows either way. Only a real
/// render can tell "built 200 rows and clipped 170 of them" apart from "built 30". These tests
/// therefore also assert the *positive* half - that the rows which should paint really do, and
/// that the ones which don't are still reachable by really scrolling - so a future change that
/// "virtualizes" by rendering nothing fails here rather than passing.
///
/// Both of the first two tests were run against the pre-fix eager `flex_col` before being
/// committed, and both genuinely failed against it (row 199 was painted); they pass against the
/// `uniform_list`. That is what they measure and all they measure.
///
/// **What they do not catch, honestly:** re-wrapping the list in an outer `overflow_y_scroll()`
/// container - the trap `crate::sidebar::render::AdeApp::render_file_tree`'s docs warn about -
/// was tried here directly, and this list kept virtualizing anyway, because the `uniform_list`'s
/// own `.flex_1().min_h_0()` still resolves against the outer scroller's definite height rather
/// than growing to the full virtual one. So no assertion below claims to guard that; the only
/// thing keeping the wrapper away is [`AdeApp::render_graph_rows`]' own structure and docs.
///
/// Mirrors `crate::sidebar::render::virtualization_tests`, the same proof for the file tree and
/// the Changes list.
#[cfg(test)]
mod graph_virtualization_tests {
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
            "git {:?} failed in {:?}: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
    }

    /// `count` real commits, and a clean working tree at the end so `build_graph` adds no
    /// "Working tree" row to shift the indices these tests name by literal selector.
    ///
    /// `--allow-empty` keeps the seed cheap: what is being measured here is how many *rows* get
    /// painted, and an empty commit is as real a `GraphRow` as any other.
    fn seed_commits(dir: &std::path::Path, count: usize) {
        init_repo(dir);
        std::fs::write(dir.join("a.txt"), "1\n").expect("write a.txt");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "first"]);
        for index in 1..count {
            git(
                dir,
                &["commit", "--allow-empty", "-m", &format!("c{index}")],
            );
        }
    }

    fn open_graph_on<'a>(
        cx: &'a mut TestAppContext,
        repo: &std::path::Path,
    ) -> (Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    /// The test viewport is 1920x1080 (`gpui`'s own test display), so at `theme::graph::ROW`'s
    /// 26px a little over 30 commit rows can possibly be on screen at once. 200 rows is far past
    /// that in every direction, and still well under `wt_core::graph::DEFAULT_MAX_COMMITS`, so
    /// this measures virtualization alone rather than the load cap.
    const SEEDED_COMMITS: usize = 200;

    #[gpui::test]
    fn a_graph_row_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), SEEDED_COMMITS);
        let (_app, cx) = open_graph_on(cx, repo.path());

        assert!(
            cx.debug_bounds("graph-row-0").is_some(),
            "the first commit row must really paint - if it doesn't, this test proves nothing \
             about virtualization, only that the graph is empty"
        );
        assert!(
            cx.debug_bounds("graph-row-199").is_none(),
            "the 200th commit row is far below any plausible viewport, so a virtualized list \
             must never build it as an element at all - this is exactly what the pre-fix eager \
             `flex_col` did, and what this assertion was checked to genuinely fail against"
        );
    }

    /// The other half of "is it really virtualized": a row that legitimately isn't painted yet
    /// must still be reachable. This scrolls the real list with a real `gpui::ScrollWheelEvent`
    /// and asserts the row that was absent genuinely materializes - which simultaneously proves
    /// the list still scrolls at all now that the former `div().overflow_y_scroll()` wrapper is
    /// gone, the one behaviour this change could plausibly have broken outright.
    #[gpui::test]
    fn scrolling_the_virtualized_graph_materializes_a_row_that_was_not_painted(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), SEEDED_COMMITS);
        let (_app, cx) = open_graph_on(cx, repo.path());

        let first_row = cx
            .debug_bounds("graph-row-0")
            .expect("the first commit row must really paint");
        assert!(
            cx.debug_bounds("graph-row-199").is_none(),
            "precondition: the last row must not be painted before scrolling"
        );

        // A deliberately huge delta: `uniform_list` clamps to its own real maximum scroll offset,
        // so this lands at the true bottom without this test having to model row heights or the
        // viewport itself.
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: first_row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("graph-row-199").is_some(),
            "scrolling to the bottom must really materialize the last row - if this fails the \
             list is not scrollable any more, which is a far worse regression than the per-frame \
             render cost this change set out to fix"
        );
        assert!(
            cx.debug_bounds("graph-row-0").is_none(),
            "and the rows scrolled off the top must stop being built, not merely move - a list \
             that keeps painting them is not virtualizing, it is just translating"
        );
    }

    /// The trailing "loading more commits" row is the final *item* of the virtualized list rather
    /// than a sibling appended below it, so it has to (a) sit directly below the last loaded row
    /// rather than floating anywhere else, and (b) respect the fixed row height every other item
    /// has - `uniform_list` sizes every slot from item 0 alone, so a row that disagreed with
    /// `theme::graph::ROW` would be laid out at the wrong height with no panic and no warning.
    ///
    /// The truncated graph here is real `wt_core::graph::build_graph` output, walked with an
    /// explicit small `max_commits` rather than a repository of 501 commits - the same real code
    /// path and the same real `Graph::truncated` flag `DEFAULT_MAX_COMMITS` sets, at a seed cost
    /// this test suite can afford. `load_more_in_flight` is set the same way the real trigger sets
    /// it, and left set, so the frame under assertion is stable: the real walk is never allowed to
    /// finish and take the row away mid-test.
    #[gpui::test]
    fn the_load_more_row_is_the_last_item_of_the_list_at_the_shared_row_height(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), 6);
        let (app, cx) = open_graph_on(cx, repo.path());

        let truncated = wt_core::graph::build_graph(repo.path(), GraphScope::All, 3)
            .expect("a real, deliberately capped graph walk");
        assert!(
            truncated.truncated && truncated.rows.len() == 3,
            "precondition: this must be a genuinely truncated walk, got {} rows, truncated={}",
            truncated.rows.len(),
            truncated.truncated
        );
        app.update(cx, |app, cx| {
            app.graph_state.load = GraphLoadState::Loaded(truncated);
            app.graph_state.load_more_in_flight = true;
            cx.notify();
        });
        cx.run_until_parked();

        let last_row = cx
            .debug_bounds("graph-row-2")
            .expect("the last loaded commit row must paint");
        let loading = cx.debug_bounds("graph-rows-load-more").expect(
            "while more history is genuinely being walked the list must say so at its bottom",
        );
        assert_eq!(
            loading.origin.y,
            last_row.origin.y + last_row.size.height,
            "the loading row must be the item directly below the last loaded commit row"
        );
        assert_eq!(
            loading.size.height,
            theme::graph::ROW,
            "and it must be exactly as tall as every other item, which is the fixed height \
             `uniform_list` lays every slot out at"
        );
    }

    /// GitHub issue #221 seeds: more commits than `wt_core::graph::DEFAULT_MAX_COMMITS`, so the
    /// tab's own first load is genuinely capped and truncated exactly the way the reported bug is.
    /// 520 keeps the whole remaining history inside a single [`LOAD_MORE_BATCH`], so one real
    /// scroll to the bottom is enough to reach the true end.
    const OVER_CAP_COMMITS: usize = 520;

    /// The issue itself: the graph must not stop at 500. Scrolling the real `uniform_list` to the
    /// bottom of the initially-capped walk has to load the rest, and rows that could not possibly
    /// exist before (index >= 500) have to become genuinely paintable elements.
    #[gpui::test]
    fn scrolling_to_the_bottom_of_a_capped_walk_really_loads_more_commits(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), OVER_CAP_COMMITS);
        let (app, cx) = open_graph_on(cx, repo.path());

        app.read_with(cx, |app, _| {
            let graph = app.current_graph().expect("the graph must be loaded");
            assert_eq!(
                graph.rows.len(),
                wt_core::graph::DEFAULT_MAX_COMMITS,
                "precondition: the first load really is capped - this is the reported bug"
            );
            assert!(
                graph.truncated,
                "precondition: and it knows it stopped early"
            );
        });
        assert!(
            cx.debug_bounds("graph-row-519").is_none(),
            "precondition: the 520th commit is not even loaded yet, let alone painted"
        );

        // A deliberately huge delta: `uniform_list` clamps to its own real maximum scroll offset,
        // so this lands at the true bottom of what is loaded without modelling row heights.
        let anchor = row_list_anchor(cx);
        scroll_to_bottom(cx, anchor);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let graph = app.current_graph().expect("the graph must still be loaded");
            assert_eq!(
                graph.rows.len(),
                OVER_CAP_COMMITS,
                "reaching the bottom must walk the rest of the real history, not stop at 500"
            );
            assert!(
                !graph.truncated,
                "and with the whole history walked nothing is truncated any more"
            );
            assert_eq!(
                app.graph_state.loaded_cap,
                wt_core::graph::DEFAULT_MAX_COMMITS + LOAD_MORE_BATCH,
                "exactly one batch must have been applied"
            );
        });

        // Now that they exist, the newly loaded rows must really paint - scrolling again goes to
        // the new bottom, which the previous scroll offset (unchanged by the swap, which is the
        // point) is no longer at.
        scroll_to_bottom(cx, anchor);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("graph-row-519").is_some(),
            "the last commit of the real history must be a genuinely painted row once loaded"
        );
        assert!(
            cx.debug_bounds("graph-rows-load-more").is_none(),
            "and with nothing left to load there must be no loading row at the bottom"
        );
    }

    /// A load-more replaces the whole `Graph`, so the user's selection has to survive it. It does
    /// because a bigger cap is an element-identical prefix superset of the smaller walk
    /// (`wt_core::graph::tests::graph_walk_is_prefix_stable_across_caps` pins that), and this
    /// asserts the consequence end to end: the same row index still names the same real commit.
    #[gpui::test]
    fn loading_more_commits_keeps_the_selected_row_on_the_same_commit(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), OVER_CAP_COMMITS);
        let (app, cx) = open_graph_on(cx, repo.path());

        let selected_sha = app.update(cx, |app, cx| {
            app.select_graph_row(12, cx);
            app.current_graph_row(12)
                .expect("row 12 of a 500-row walk")
                .commit
                .id
                .clone()
        });
        cx.run_until_parked();

        let anchor = row_list_anchor(cx);
        scroll_to_bottom(cx, anchor);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.current_graph().expect("loaded").rows.len()
                    > wt_core::graph::DEFAULT_MAX_COMMITS,
                "precondition: a load-more really did happen"
            );
            assert_eq!(
                app.graph_state.selected_row,
                Some(12),
                "the selection must not move when more history is appended below it"
            );
            assert_eq!(
                app.current_graph_row(12).map(|row| row.commit.id.clone()),
                Some(selected_sha.clone()),
                "and that index must still name the very same commit"
            );
        });
    }

    /// The single-flight guard. `AdeApp::load_more_graph_rows` is what every frame near the
    /// bottom calls - and `uniform_list` runs the row builder several times per frame - so a
    /// second call while a walk is already running must start nothing.
    ///
    /// This calls that real function directly rather than through a second simulated scroll,
    /// because mid-flight is not reachable through one: `VisualTestContext::simulate_event` runs
    /// the executor until parked itself, so by the time it returns the walk has already finished.
    /// The observable is `_load_more_task`, which only `load_more_graph_rows` ever writes: it is
    /// cleared between the two calls, so the slot being `None` afterwards means no second walk was
    /// spawned. (Clearing it also cancels the first walk, which is what pins `load_more_in_flight`
    /// true for the rest of the test - exactly the state the guard exists to handle.)
    #[gpui::test]
    fn a_second_call_while_a_walk_is_in_flight_starts_nothing(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), 6);
        let (app, cx) = open_graph_on(cx, repo.path());

        let truncated = wt_core::graph::build_graph(repo.path(), GraphScope::All, 3)
            .expect("a real, deliberately capped graph walk");
        assert!(truncated.truncated, "precondition: a truncated walk");
        app.update(cx, |app, cx| {
            app.graph_state.load = GraphLoadState::Loaded(truncated);
            app.graph_state.loaded_cap = 3;

            app.load_more_graph_rows(cx);
            assert!(
                app.graph_state.load_more_in_flight,
                "the first call must really start a walk"
            );
            assert!(
                app.graph_state._load_more_task.is_some(),
                "and really hold it as a task"
            );

            app.graph_state._load_more_task = None;
            app.load_more_graph_rows(cx);
            assert!(
                app.graph_state._load_more_task.is_none(),
                "a second call while one is already in flight must not spawn an overlapping walk"
            );
        });
    }

    /// The other half of the guard: a walk that genuinely failed must not be retried on every
    /// frame the user spends near the bottom. The failure here is real - the repository is
    /// deleted out from under the walk, which is exactly what `build_graph` errors on - and the
    /// real error has to reach the user through the toolbar's own status line.
    #[gpui::test]
    fn a_failed_load_more_reports_the_real_error_and_stops_retrying(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), 6);
        let (app, cx) = open_graph_on(cx, repo.path());

        let truncated = wt_core::graph::build_graph(repo.path(), GraphScope::All, 3)
            .expect("a real, deliberately capped graph walk");
        app.update(cx, |app, cx| {
            app.graph_state.load = GraphLoadState::Loaded(truncated);
            app.graph_state.loaded_cap = 3;
            cx.notify();
        });
        std::fs::remove_dir_all(repo.path().join(".git")).expect("remove the real repository");

        app.update(cx, |app, cx| app.load_more_graph_rows(cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.load_more_failed,
                "a genuinely failed walk must be recorded, or the next frame retries it forever"
            );
            let status = app.graph_state.status_message.clone();
            assert!(
                status
                    .as_deref()
                    .is_some_and(|message| message.starts_with("loading more commits failed:")),
                "the real error must reach the user rather than being swallowed - got {status:?}"
            );
            assert_eq!(
                app.current_graph().expect("still loaded").rows.len(),
                3,
                "and the rows already on screen must survive a failed extension of the walk"
            );
        });

        app.update(cx, |app, cx| {
            app.graph_state._load_more_task = None;
            app.load_more_graph_rows(cx);
            assert!(
                app.graph_state._load_more_task.is_none(),
                "and no further frame near the bottom may retry the failed walk"
            );
        });
    }

    /// The true end of history is not noteworthy: a fully-scrolled small repository must show no
    /// trailing row at all - no loading indicator, and above all none of the old "showing the
    /// first N commits" notice the issue asked for the removal of - and must not spin on a
    /// reload loop that never terminates.
    #[gpui::test]
    fn reaching_the_true_end_of_history_shows_nothing_and_loads_nothing(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_commits(repo.path(), 4);
        let (app, cx) = open_graph_on(cx, repo.path());

        app.read_with(cx, |app, _| {
            let graph = app.current_graph().expect("the graph must be loaded");
            assert!(!graph.truncated, "precondition: the whole history fits");
            assert_eq!(
                graph_item_count(graph, app.graph_state.load_more_in_flight),
                graph.rows.len(),
                "with nothing to load the list has exactly one item per commit row"
            );
        });

        // Every one of these parks, which is itself the "no infinite reload loop" assertion: a
        // trigger that kept re-firing would keep the executor busy and never park.
        let anchor = row_list_anchor(cx);
        for _ in 0..3 {
            scroll_to_bottom(cx, anchor);
            cx.run_until_parked();
        }

        assert!(
            cx.debug_bounds("graph-rows-load-more").is_none(),
            "there is nothing left to load, so nothing may claim to be loading"
        );
        app.read_with(cx, |app, _| {
            assert!(
                !app.graph_state.load_more_in_flight,
                "an untruncated walk must never start a load-more at all"
            );
            assert_eq!(
                app.graph_state.loaded_cap,
                wt_core::graph::DEFAULT_MAX_COMMITS,
                "and the cap must not have been bumped for a walk that never ran"
            );
            assert_eq!(
                app.current_graph().expect("loaded").rows.len(),
                4,
                "the loaded rows must be exactly the real history, unchanged"
            );
        });
    }

    /// A window position genuinely inside the row list, captured from the first row while it is
    /// still on screen. The list occupies that screen area no matter how far it is scrolled, so
    /// one capture is enough to keep aiming later scroll events at it.
    fn row_list_anchor(cx: &mut gpui::VisualTestContext) -> gpui::Point<Pixels> {
        cx.debug_bounds("graph-row-0")
            .expect("the first commit row must paint before anything is scrolled")
            .center()
    }

    /// Scrolls the real list past its own maximum offset; `uniform_list` clamps, so this lands at
    /// the true bottom of whatever is currently loaded without modelling row heights or the
    /// viewport.
    fn scroll_to_bottom(cx: &mut gpui::VisualTestContext, anchor: gpui::Point<Pixels>) {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: anchor,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
    }
}

/// GitHub issue #241: the Branches panel's own branch right-click context menu, driven through
/// **real** simulated mouse events (`cx.simulate_event`) against real painted rows rather than by
/// calling the open/close methods directly - the same discipline `graph_row_menu_tests` above
/// established, and for the same reason: the scrim/popover/row interactions are exactly where
/// this class of menu has historically gone wrong (a right-click on the popover falling through
/// to the row underneath, a dismiss click also re-triggering what it landed on).
#[cfg(test)]
mod graph_branch_menu_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Entity, Pixels, TestAppContext};

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

    fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A real repository with three real local branches - `main` (checked out), `feature-a` and
    /// `feature-b` - all reachable from the default `GraphScope::All` walk, so all three really
    /// appear as rows in the Branches panel.
    fn seed_three_branches(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        std::fs::write(dir.join("a.txt"), "1\n").expect("write a.txt");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "first"]);
        git(dir, &["checkout", "-b", "feature-a"]);
        std::fs::write(dir.join("a.txt"), "2\n").expect("write a.txt");
        git(dir, &["commit", "-am", "feature a work"]);
        git(dir, &["checkout", "main"]);
        git(dir, &["checkout", "-b", "feature-b"]);
        std::fs::write(dir.join("b.txt"), "1\n").expect("write b.txt");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "feature b work"]);
        git(dir, &["checkout", "main"]);
    }

    /// Opens the graph tab with the Branches panel showing - the real surface these rows live on.
    fn open_seeded_branches_panel(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        seed_three_branches(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_graph_right_panel(GraphRightPanel::Branches, cx);
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
    fn right_clicking_a_branch_row_opens_its_menu_for_that_branch(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);

        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("feature-a's branch row must really be painted in the Branches panel");
        right_click(cx, row.center());

        app.read_with(cx, |app, _| {
            let menu = app
                .graph_state
                .branch_menu_open
                .clone()
                .expect("a real right-click on a branch row must open its menu");
            assert_eq!(
                menu.branch, "feature-a",
                "the menu must name the branch under the cursor, not some other row's"
            );
        });
        let painted = cx
            .debug_bounds("graph-branch-menu-popover")
            .expect("and the popover must genuinely paint");
        app.read_with(cx, |app, _| {
            let menu = app.graph_state.branch_menu_open.clone().expect("menu");
            assert_eq!(
                (painted.origin.x, painted.origin.y),
                (menu.origin_x, menu.origin_y),
                "the popover must paint at exactly the position captured when it opened"
            );
        });
    }

    /// The Branches panel sits hard against the window's right edge, so *every* branch row's
    /// right-click would open a popover running off-screen without the same
    /// `clamp_menu_origin` the row menu uses. That makes this the one menu in the app where the
    /// clamp is not an edge case at all.
    #[gpui::test]
    fn the_branch_menu_is_clamped_fully_inside_the_window(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);

        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());

        let painted = cx
            .debug_bounds("graph-branch-menu-popover")
            .expect("popover painted");
        let viewport = cx.update(|window, _cx| window.bounds().size);
        assert!(
            painted.origin.x >= px(0.0) && painted.origin.x + painted.size.width <= viewport.width,
            "the popover must be clamped inside the window horizontally: painted {painted:?} in \
             {viewport:?}"
        );
        assert!(
            painted.origin.y >= px(0.0)
                && painted.origin.y + painted.size.height <= viewport.height,
            "the popover must be clamped inside the window vertically: painted {painted:?} in \
             {viewport:?}"
        );
        assert!(
            row.center().x > painted.origin.x,
            "premise: the click really was far enough right that an unclamped popover would have \
             run off the edge"
        );
        app.read_with(cx, |app, _| {
            assert!(app.graph_state.branch_menu_open.is_some());
        });
    }

    /// `theme::graph::BRANCH_MENU_HEIGHT` is a hand-measured constant the edge clamp above relies
    /// on - see [`theme::graph::ROW_MENU_HEIGHT`]'s own docs for why it can't be a formula, and
    /// what to do when this fails (re-measure, don't guess).
    #[gpui::test]
    fn the_branch_menu_pins_the_real_height_this_edge_clamp_relies_on(cx: &mut TestAppContext) {
        let (_repo, _app, cx) = open_seeded_branches_panel(cx);

        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());
        let painted = cx
            .debug_bounds("graph-branch-menu-popover")
            .expect("the popover must genuinely paint");

        assert_eq!(
            (painted.size.width, painted.size.height),
            (
                theme::graph::BRANCH_MENU_WIDTH,
                theme::graph::BRANCH_MENU_HEIGHT
            ),
            "the real painted size must match the constants the edge clamp uses - re-measure and \
             update BRANCH_MENU_HEIGHT if this menu's content genuinely changed"
        );
    }

    #[gpui::test]
    fn right_clicking_a_different_branch_row_retargets_the_open_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);

        let row_b = cx
            .debug_bounds("graph-branch-row-feature-b")
            .expect("feature-b row painted");
        right_click(cx, row_b.center());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .branch_menu_open
                    .as_ref()
                    .map(|menu| menu.branch.clone()),
                Some("feature-b".to_string()),
                "premise: feature-b's menu really is open first"
            );
        });

        // `feature-a` sorts above `feature-b`, so its row sits above the open popover rather than
        // underneath it - the "different, unobscured row" case (a right-click *through* the
        // popover is a separate, deliberately different scenario, covered below).
        let row_a = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("feature-a row painted");
        assert!(
            row_a.center().y < row_b.center().y,
            "premise: feature-a's row sits above feature-b's, so feature-b's downward-opening \
             popover cannot cover it"
        );
        right_click(cx, row_a.center());

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .branch_menu_open
                    .as_ref()
                    .map(|menu| menu.branch.clone()),
                Some("feature-a".to_string()),
                "the newly right-clicked branch must win - not still the stale one"
            );
        });
    }

    /// The exact adversarial-audit-found bug the popover's own `.occlude()` exists for, ported to
    /// this menu: the popover paints *over* the branch list, so without it a right-click on the
    /// panel itself retargeted the menu to whichever branch row happened to be underneath.
    #[gpui::test]
    fn right_clicking_inside_the_open_branch_popover_does_not_retarget_to_the_row_underneath(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);

        let row_a = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("feature-a row painted");
        right_click(cx, row_a.center());
        let popover = cx
            .debug_bounds("graph-branch-menu-popover")
            .expect("popover painted");

        // A point genuinely inside the popover, and genuinely over a *different* branch row.
        let row_b = cx
            .debug_bounds("graph-branch-row-feature-b")
            .expect("feature-b row painted");
        // Inside both boxes: a few pixels into feature-b's own row, which the popover's painted
        // width really does cover at that point. (The popover's *centre* sits left of the panel
        // entirely, so centring on it would test nothing - the click would have missed the row
        // even without `.occlude()`.)
        let inside = gpui::Point {
            x: row_b.origin.x + px(4.0),
            y: row_b.center().y,
        };
        assert!(
            inside.y > popover.origin.y
                && inside.y < popover.origin.y + popover.size.height
                && inside.x > popover.origin.x
                && inside.x < popover.origin.x + popover.size.width,
            "premise: the point must really be inside the popover's own painted bounds: point \
             {inside:?} vs popover {popover:?}"
        );
        assert!(
            inside.x >= row_b.origin.x
                && inside.x <= row_b.origin.x + row_b.size.width
                && inside.y >= row_b.origin.y
                && inside.y <= row_b.origin.y + row_b.size.height,
            "premise: and genuinely over feature-b's own row, so without `.occlude()` it really \
             would have fallen through to it: point {inside:?} vs row {row_b:?}"
        );
        right_click(cx, inside);

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .branch_menu_open
                    .as_ref()
                    .map(|menu| menu.branch.clone()),
                Some("feature-a".to_string()),
                "a right-click on the popover itself must never fall through and retarget the \
                 menu to the row painted underneath it"
            );
        });
    }

    /// GitHub issue #176's shared one-menu-at-a-time invariant, at this menu's own real trigger.
    #[gpui::test]
    fn opening_the_branch_menu_closes_an_open_row_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);
        app.update_in(cx, |app, window, cx| {
            app.open_graph_row_menu_at(0, px(20.0), px(120.0), window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.graph_state.row_menu_open.is_some()),
            "premise: the row menu really is open first"
        );

        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.row_menu_open.is_none(),
                "opening the branch menu must close the row menu - two popovers at once is the \
                 reported bug"
            );
            assert!(app.graph_state.branch_menu_open.is_some());
        });
    }

    /// The mirror direction: the row menu's own opener sweeps this one closed too, which is what
    /// registering `MenuSurface::GraphBranch` buys.
    #[gpui::test]
    fn opening_the_row_menu_closes_an_open_branch_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);
        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());
        assert!(
            app.read_with(cx, |app, _| app.graph_state.branch_menu_open.is_some()),
            "premise: the branch menu really is open first"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_graph_row_menu_at(0, px(20.0), px(120.0), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.branch_menu_open.is_none(),
                "opening the row menu must close the branch menu"
            );
            assert!(app.graph_state.row_menu_open.is_some());
        });
    }

    /// A left-click anywhere off the popover dismisses it, and - the second
    /// adversarial-audit-found fix this menu inherits - that same click must not also reach
    /// whatever it landed on and re-open something.
    #[gpui::test]
    fn a_left_click_away_dismisses_the_branch_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);
        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());
        assert!(
            app.read_with(cx, |app, _| app.graph_state.branch_menu_open.is_some()),
            "premise: the menu really is open"
        );

        cx.simulate_click(
            gpui::Point {
                x: px(30.0),
                y: px(400.0),
            },
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.branch_menu_open.is_none(),
                "a real click away must dismiss the branch menu"
            );
        });
    }

    /// The menu's rows are really wired to the real actions - driven by a genuine click on the
    /// painted row rather than by calling the handler, so this covers the row → handler → git
    /// path the user actually takes.
    #[gpui::test]
    fn clicking_the_checkout_row_really_checks_that_branch_out(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded_branches_panel(cx);
        assert_eq!(
            git_output(repo.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main",
            "premise: the worktree starts on main"
        );

        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());
        let checkout_row = cx
            .debug_bounds("dropdown-menu-row-Checkout Branch")
            .expect("the menu's Checkout row must really paint");
        cx.simulate_click(checkout_row.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            git_output(repo.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feature-a",
            "a real click on the real menu row must have run a real git checkout"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.branch_menu_open.is_none(),
                "acting on a row dismisses the menu"
            );
        });
    }

    /// The Rename row through its whole real path: a real click opens the shared branch-name
    /// prompt, real keystrokes edit it, and a real Enter runs the real `git branch -m` - so the
    /// prompt's own key handler and focus wiring are covered for the rename kind too, not just
    /// for the create kind it was originally built for.
    #[gpui::test]
    fn renaming_through_the_real_prompt_keystrokes_really_renames_the_branch(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_branches_panel(cx);

        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());
        let rename_row = cx
            .debug_bounds("dropdown-menu-row-Rename Branch\u{2026}")
            .expect("the menu's Rename row must really paint");
        cx.simulate_click(rename_row.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.branch_prompt_name.as_str(),
                "feature-a",
                "the prompt must open pre-filled with the branch's real current name"
            );
        });
        assert!(
            cx.debug_bounds("graph-branch-prompt-subtitle").is_some(),
            "the shared prompt's subtitle line must really paint for the rename kind too - it is \
             where the branch being renamed is named"
        );

        cx.simulate_input("2");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let branches = git_output(repo.path(), &["branch", "--format=%(refname:short)"]);
        assert!(
            branches.lines().any(|line| line == "feature-a2"),
            "real keystrokes plus a real Enter must have run a real git branch -m: {branches:?}"
        );
        assert!(
            !branches.lines().any(|line| line == "feature-a"),
            "and the old name must genuinely be gone: {branches:?}"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.branch_prompt.is_none(),
                "the prompt must close once Enter really dispatched the rename"
            );
        });
    }

    /// Every remaining row, driven by a real click on the real painted row - so each label is
    /// pinned to the action actually behind it. Without this, a row wired to the wrong handler (or
    /// to the right handler with the wrong argument) would still pass every handler-level test,
    /// because those call the handlers directly.
    #[gpui::test]
    fn each_menu_row_is_really_wired_to_its_own_action(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded_branches_panel(cx);

        // Copy Branch Name - the clipboard must get the *branch name*, not a sha or a subject
        // (the row shares `copy_graph_text` with the commit menu's own Copy rows, so nothing else
        // pins which string it hands over).
        open_menu_at(cx, "graph-branch-row-feature-a");
        click_menu_row(cx, "dropdown-menu-row-Copy Branch Name to Clipboard");
        assert_eq!(
            cx.update(|_window, cx| cx.read_from_clipboard())
                .and_then(|item| item.text()),
            Some("feature-a".to_string()),
            "the Copy row must copy the branch it was opened for"
        );

        // Delete Branch - a first click arms *that* branch and deletes nothing.
        open_menu_at(cx, "graph-branch-row-feature-a");
        click_menu_row(cx, "dropdown-menu-row-Delete Branch\u{2026}");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.delete_branch_confirm_armed.clone(),
                Some("feature-a".to_string()),
                "the Delete row must arm the branch it was opened for"
            );
        });
        assert!(
            git_output(repo.path(), &["branch", "--format=%(refname:short)"])
                .lines()
                .any(|line| line == "feature-a"),
            "and must not have deleted anything on a first click"
        );

        // Push Branch - no remote is configured in this fixture, so what this pins is that the
        // row really reaches `push_branch` (its own action name) rather than some other action.
        open_menu_at(cx, "graph-branch-row-feature-a");
        click_menu_row(cx, "dropdown-menu-row-Push Branch\u{2026}");
        cx.run_until_parked();
        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Push branch failed:")),
            "the Push row must really run a push and report git's own failure for a repository \
             with no remote - got {status:?}"
        );

        // Rebase current branch on Branch - enters the shared rebase mode, pinned to that
        // branch's own real tip commit.
        let feature_a_tip = git_output(repo.path(), &["rev-parse", "feature-a"]);
        open_menu_at(cx, "graph-branch-row-feature-a");
        click_menu_row(
            cx,
            "dropdown-menu-row-Rebase current branch on Branch\u{2026}",
        );
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .rebase
                    .as_ref()
                    .map(|rebase| rebase.onto.clone()),
                Some(feature_a_tip.clone()),
                "the Rebase row must enter rebase mode onto the clicked branch's real tip"
            );
        });
        app.update_in(cx, |app, _window, cx| {
            app.leave_rebase_mode(cx);
            // `leave_rebase_mode` itself does not notify (its real callers do), and the Branches
            // list is only painted again once a frame is drawn without the rebase Result panel
            // over it.
            cx.notify();
        });
        cx.run_until_parked();

        // Merge into current branch - fills the app's one existing merge flow.
        open_menu_at(cx, "graph-branch-row-feature-a");
        click_menu_row(cx, "dropdown-menu-row-Merge into current branch\u{2026}");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                app.merge_flow.is_some(),
                "the Merge row must really start the existing merge flow"
            );
        });
    }

    /// Right-clicks the branch row painted under `row_selector` and returns once its menu is
    /// really open. Takes the selector rather than the branch name because `debug_bounds` needs a
    /// `&'static str`.
    fn open_menu_at(cx: &mut gpui::VisualTestContext, row_selector: &'static str) {
        let row = cx
            .debug_bounds(row_selector)
            .unwrap_or_else(|| panic!("{row_selector} must really be painted"));
        right_click(cx, row.center());
    }

    /// Clicks the open menu's row painted under `row_selector`, for real.
    fn click_menu_row(cx: &mut gpui::VisualTestContext, row_selector: &'static str) {
        let row = cx
            .debug_bounds(row_selector)
            .unwrap_or_else(|| panic!("the menu's {row_selector} row must really paint"));
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    /// Why the menu is keyed by branch **name** and not by the branch row's index: the panel's
    /// list is re-filtered live, so the row a menu was opened over genuinely moves (or vanishes)
    /// while that menu stays open. An index-keyed menu would then act on whatever branch now sits
    /// at that index - or on nothing at all.
    #[gpui::test]
    fn the_open_menu_keeps_targeting_its_branch_when_the_filtered_list_reorders(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_branches_panel(cx);

        let row_b = cx
            .debug_bounds("graph-branch-row-feature-b")
            .expect("feature-b row painted");
        right_click(cx, row_b.center());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .branch_menu_open
                    .as_ref()
                    .map(|menu| menu.branch.clone()),
                Some("feature-b".to_string())
            );
        });

        // A real filter that leaves feature-b as the *only* row, moving it from the third
        // position to the first while its own menu is still open.
        app.update_in(cx, |app, _window, cx| {
            app.graph_state.branches_filter = crate::text_history::TextField::seeded("b");
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("graph-branch-row-feature-a").is_none(),
            "premise: the filter really did drop the row that used to sit above it"
        );

        let checkout_row = cx
            .debug_bounds("dropdown-menu-row-Checkout Branch")
            .expect("the still-open menu must still paint");
        cx.simulate_click(checkout_row.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            git_output(repo.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feature-b",
            "the menu must still act on the branch it was opened for, not on whatever branch the \
             re-filtered list now holds at that position"
        );
    }

    /// Switching away from the graph tab and back must never resurrect a stale popover with no
    /// click at all - the exact gap `leave_graph_tab` documents for the row/push menus, mirrored.
    #[gpui::test]
    fn leaving_the_graph_tab_dismisses_the_branch_menu(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_branches_panel(cx);
        let row = cx
            .debug_bounds("graph-branch-row-feature-a")
            .expect("branch row painted");
        right_click(cx, row.center());
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature-a".to_string(), cx);
        });
        assert!(
            app.read_with(cx, |app, _| app
                .graph_state
                .delete_branch_confirm_armed
                .is_some()),
            "premise: the Delete confirmation really is armed"
        );

        app.update_in(cx, |app, window, cx| {
            app.leave_graph_tab(window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.branch_menu_open.is_none(),
                "leaving the tab must dismiss the branch menu"
            );
            assert!(
                app.graph_state.delete_branch_confirm_armed.is_none(),
                "and must never leave a branch armed for a one-click delete on the way back"
            );
        });
    }
}

/// GitHub issue #241: what each of the branch context menu's seven actions really does, end to
/// end, against a real git repository through a real `AdeApp` - the same "prove the real git
/// effect, not just the state field" discipline `graph_remote_action_tests` above applies to the
/// toolbar and row-menu actions.
#[cfg(test)]
mod graph_branch_action_tests {
    use crate::merge::state as merge;
    use crate::root::focus::palette_focus_tests;
    use crate::root::AdeApp;
    use crate::text_history::TextField;
    use crate::work_surface::agents::ProcessKind;
    use gpui::{Entity, TestAppContext};
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
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

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
    }

    fn branches(dir: &Path) -> Vec<String> {
        git_output(dir, &["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn current_branch(dir: &Path) -> String {
        git_output(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    /// `MergeFlowState` is deliberately not `Debug` (it carries whole parsed file contents), so
    /// an unexpected variant is reported by name rather than by dumping it.
    fn merge_state_name(state: &merge::MergeFlowState) -> String {
        match state {
            merge::MergeFlowState::Running => "Running".to_string(),
            merge::MergeFlowState::AlreadyUpToDate { .. } => "AlreadyUpToDate".to_string(),
            merge::MergeFlowState::Clean { .. } => "Clean".to_string(),
            merge::MergeFlowState::Conflicted { .. } => "Conflicted".to_string(),
            // The real error text matters far more than the variant name here.
            merge::MergeFlowState::Error { message, .. } => format!("Error({message})"),
        }
    }

    /// A real local repo on `main`, with a real `feature` branch that is checked out nowhere -
    /// the real shape a branch row in the panel has.
    fn open_seeded_with_feature_branch(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let local = tempfile::tempdir().expect("tempdir");
        git(local.path(), &["init", "-b", "main"]);
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);
        commit(local.path(), "a.txt", "base", "base");
        git(local.path(), &["checkout", "-b", "feature"]);
        commit(local.path(), "b.txt", "feature", "feature work");
        git(local.path(), &["checkout", "main"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, local.path().to_path_buf());
        (local, app, cx)
    }

    #[gpui::test]
    async fn checkout_branch_really_switches_the_worktree_onto_that_branch(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        assert_eq!(
            current_branch(local.path()),
            "main",
            "premise: the worktree starts on main"
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_branch_checkout("feature".to_string(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            current_branch(local.path()),
            "feature",
            "the real click must have run a real git checkout landing on the branch itself, not \
             a detached HEAD"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Check out".to_string()),
            "a successful checkout must report real success"
        );
    }

    #[gpui::test]
    async fn rename_branch_prefills_the_current_name_and_really_renames_the_ref(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);

        app.update_in(cx, |app, window, cx| {
            app.start_graph_rename_branch("feature".to_string(), window, cx);
        });
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state.branch_prompt_name.as_str(),
                "feature",
                "a rename must start from the branch's real current name, not an empty box"
            );
            assert!(
                matches!(
                    app.graph_state.branch_prompt.as_ref().map(|p| &p.kind),
                    Some(crate::graph_view::state::GraphBranchPromptKind::Rename { old_name })
                        if old_name == "feature"
                ),
                "the prompt must be open in rename mode for the branch that was clicked"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.graph_state.branch_prompt_name = TextField::seeded("renamed-feature");
            app.commit_graph_branch_prompt(window, cx);
        });
        cx.run_until_parked();

        let branches = branches(local.path());
        assert!(
            branches.iter().any(|b| b == "renamed-feature"),
            "the real Enter must have run a real git branch -m: {branches:?}"
        );
        assert!(
            !branches.iter().any(|b| b == "feature"),
            "and the old name must genuinely be gone: {branches:?}"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Rename branch".to_string())
        );
        assert!(
            app.read_with(cx, |app, _| app.graph_state.branch_prompt.is_none()),
            "the prompt must close once the rename is really dispatched"
        );
    }

    #[gpui::test]
    async fn rename_onto_a_name_that_already_exists_surfaces_gits_own_real_error(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        git(local.path(), &["branch", "taken"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_rename_branch("feature".to_string(), "taken".to_string(), cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Rename branch failed:")
                    && text.contains("already exists")),
            "a real collision must surface git's own real refusal, not a hand-rolled \
             pre-validation - got {status:?}"
        );
        assert!(
            branches(local.path()).iter().any(|b| b == "feature"),
            "the refused rename must have left the branch exactly where it was"
        );
    }

    #[gpui::test]
    async fn rename_prompt_rejects_an_empty_name_without_touching_git(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);

        app.update_in(cx, |app, window, cx| {
            app.start_graph_rename_branch("feature".to_string(), window, cx);
            app.graph_state.branch_prompt_name = TextField::seeded("   ");
            app.commit_graph_branch_prompt(window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let prompt = app
                .graph_state
                .branch_prompt
                .as_ref()
                .expect("an empty name must keep the prompt open, not close it");
            assert_eq!(
                prompt.error.as_deref(),
                Some("branch name can't be empty"),
                "and must say why"
            );
            assert_eq!(
                app.graph_state.status_message, None,
                "nothing must have been dispatched to git at all"
            );
        });
        assert!(
            branches(local.path()).iter().any(|b| b == "feature"),
            "the branch must be untouched"
        );
    }

    #[gpui::test]
    async fn delete_branch_needs_a_real_second_click_on_the_same_branch(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        // Merge `feature` into `main` first, so `git branch -d`'s own safety rule is satisfied
        // and what this test measures is the *confirmation*, not git's refusal.
        git(
            local.path(),
            &["merge", "--no-ff", "feature", "-m", "merge feature"],
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();

        assert!(
            branches(local.path()).iter().any(|b| b == "feature"),
            "the first click must only arm the confirmation, never delete anything"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .delete_branch_confirm_armed
                .clone()),
            Some("feature".to_string()),
            "the first click must arm exactly the branch that was clicked"
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();

        assert!(
            !branches(local.path()).iter().any(|b| b == "feature"),
            "the second click on the same branch must really run git branch -d"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .delete_branch_confirm_armed
                .clone()),
            None,
            "the confirmation must disarm once the real delete has actually run"
        );
    }

    /// The confirmation is keyed by branch name for exactly this reason: one branch's armed
    /// confirmation must never authorize a *different* branch's delete.
    #[gpui::test]
    async fn arming_delete_on_one_branch_never_deletes_another(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        git(local.path(), &["branch", "other"]);

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("other".to_string(), cx);
        });
        cx.run_until_parked();

        let branches = branches(local.path());
        assert!(
            branches.iter().any(|b| b == "other"),
            "the click on a different branch must only arm it, never delete it on the strength \
             of the first branch's confirmation: {branches:?}"
        );
        assert!(
            branches.iter().any(|b| b == "feature"),
            "and the originally-armed branch must be untouched too: {branches:?}"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .delete_branch_confirm_armed
                .clone()),
            Some("other".to_string()),
            "the newly-clicked branch is what ends up armed"
        );
    }

    /// An adversarial audit's finding: a Delete click that lands while some other graph operation
    /// is still in flight must not arm a confirmation at all. Before the guard moved to the top of
    /// `request_graph_delete_branch`, that click armed silently (`run_graph_remote_op`'s own
    /// single-flight check sits at the far end, well past the arming), so the *next* click - the
    /// first one the user makes after the lane clears, and to them a first click - ran a real
    /// delete with no confirmation of its own.
    #[gpui::test]
    async fn a_delete_click_while_another_op_is_in_flight_arms_nothing(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        git(
            local.path(),
            &["merge", "--no-ff", "feature", "-m", "merge feature"],
        );

        // A real, genuinely in-flight graph operation: `run_graph_remote_op` sets the flag
        // synchronously and only clears it when its background task lands, which is deliberately
        // not given a chance to run before the click below.
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_fetch(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.graph_state.remote_op_in_flight),
            "premise: an unrelated graph operation really is in flight"
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature".to_string(), cx);
        });

        // Read *before* letting the fetch finish: this is the exact moment the old ordering armed
        // a confirmation the app had no intention of honouring.
        assert_eq!(
            app.read_with(cx, |app, _| app
                .graph_state
                .delete_branch_confirm_armed
                .clone()),
            None,
            "a click the app cannot act on must arm nothing at all"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Fetch\u{2026}".to_string()),
            "and must not overwrite the in-flight operation's own status line with a \
             confirmation prompt it will not honour"
        );
        cx.run_until_parked();

        // The user's next click is, to them, a first click - so it must arm, not delete.
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();
        assert!(
            branches(local.path()).iter().any(|b| b == "feature"),
            "the first click after the lane clears must only arm - deleting here is the real bug: \
             it takes one visible click instead of two"
        );

        app.update_in(cx, |app, _window, cx| {
            app.request_graph_delete_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();
        assert!(
            !branches(local.path()).iter().any(|b| b == "feature"),
            "and the real second click still deletes"
        );
    }

    #[gpui::test]
    async fn deleting_an_unmerged_branch_surfaces_gits_own_real_refusal(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);

        // Two real clicks, past the confirmation - the refusal under test is git's, not the UI's.
        for _ in 0..2 {
            app.update_in(cx, |app, _window, cx| {
                app.request_graph_delete_branch("feature".to_string(), cx);
            });
            cx.run_until_parked();
        }

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Delete branch failed:")
                    && text.contains("not fully merged")),
            "the safe delete's real refusal must reach the user verbatim - got {status:?}"
        );
        assert!(
            branches(local.path()).iter().any(|b| b == "feature"),
            "and the branch (with its unmerged commits) must still be there"
        );
    }

    #[gpui::test]
    async fn push_branch_really_pushes_that_branch_not_the_checked_out_one(
        cx: &mut TestAppContext,
    ) {
        let remote = tempfile::tempdir().expect("tempdir");
        git(remote.path(), &["init", "--bare", "-b", "main"]);
        let local = tempfile::tempdir().expect("tempdir");
        git(
            local.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);
        commit(local.path(), "a.txt", "base", "base");
        git(local.path(), &["push", "origin", "main"]);
        git(local.path(), &["checkout", "-b", "feature"]);
        commit(local.path(), "b.txt", "feature", "feature work");
        git(local.path(), &["checkout", "main"]);
        // A commit on `main` that must *not* be pushed - proof the push really targeted the
        // named branch rather than whatever is checked out.
        commit(local.path(), "c.txt", "main only", "main only work");

        let (app, cx) = palette_focus_tests::open_test_app(cx, local.path().to_path_buf());
        app.update_in(cx, |app, _window, cx| {
            app.request_graph_push_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(remote.path(), &["log", "-1", "--format=%s", "feature"]),
            "feature work",
            "the named branch must really exist on the real remote now"
        );
        assert_eq!(
            git_output(remote.path(), &["log", "-1", "--format=%s", "main"]),
            "base",
            "and the checked-out branch's own newer commit must not have been pushed with it"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            Some("Push branch".to_string())
        );
    }

    #[gpui::test]
    async fn copy_branch_name_really_writes_the_branch_name_to_the_clipboard(
        cx: &mut TestAppContext,
    ) {
        let (_local, app, cx) = open_seeded_with_feature_branch(cx);

        app.update_in(cx, |app, _window, cx| {
            app.copy_graph_text("feature".to_string(), cx);
        });
        cx.run_until_parked();

        let clipboard = cx.update(|_window, cx| cx.read_from_clipboard());
        assert_eq!(
            clipboard.and_then(|item| item.text()),
            Some("feature".to_string()),
            "the real click must have written the real branch name to the real clipboard"
        );
    }

    #[gpui::test]
    async fn rebase_on_branch_resolves_its_real_tip_and_enters_the_shared_rebase_mode(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        // `main` has its own commit that `feature` doesn't - so there is something real to replay.
        commit(local.path(), "c.txt", "own", "own work");
        let feature_tip = git_output(local.path(), &["rev-parse", "feature"]);

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode_onto_branch("feature".to_string(), cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase = app
                .graph_state
                .rebase
                .as_ref()
                .expect("the branch's own menu row must enter the shared rebase mode");
            assert_eq!(
                rebase.onto, feature_tip,
                "the mode must be pinned to the branch's real tip commit, not its moving name"
            );
            assert!(
                feature_tip.starts_with(&rebase.onto_short),
                "and the banner's short form must be git's own abbreviation of that same commit: \
                 {:?}",
                rebase.onto_short
            );
        });

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(local.path(), &["log", "--format=%s"]),
            "own work\nfeature work\nbase",
            "and the real replay must land this worktree's own commit on top of that branch"
        );
        assert!(
            app.read_with(cx, |app, _| app.graph_state.rebase.is_none()),
            "a cleanly completed rebase must leave no rebase mode behind"
        );
    }

    #[gpui::test]
    async fn rebase_on_a_branch_that_no_longer_exists_reports_gits_own_error_and_enters_no_mode(
        cx: &mut TestAppContext,
    ) {
        let (_local, app, cx) = open_seeded_with_feature_branch(cx);

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode_onto_branch("no-such-branch".to_string(), cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.starts_with("Rebase onto no-such-branch failed:")),
            "an unresolvable branch must report a real failure - got {status:?}"
        );
        assert!(
            app.read_with(cx, |app, _| app.graph_state.rebase.is_none()),
            "and must enter no rebase mode at all"
        );
    }

    #[gpui::test]
    async fn merge_into_current_branch_lands_in_the_existing_merge_flow(cx: &mut TestAppContext) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        // The worktree's own agent tab - the surface the existing resolver renders inside. A test
        // app already opens one for the focused repo, which is exactly the real situation this
        // action expects to find.
        let agent_id = app.read_with(cx, |app, _| {
            app.agents
                .iter_for_cwd(local.path().to_path_buf())
                .map(|agent| agent.id)
                .next()
                .expect("the test app opens a real agent in the focused worktree")
        });
        // A second agent, in a genuinely different directory, left *active* - so the
        // "activate the worktree's own agent tab first" assertion below has something real to
        // measure. Without this the worktree's only agent is already active and the assertion
        // would hold with `select_agent` deleted entirely.
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let other_agent = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                elsewhere.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_agent(other_agent, window, cx);
        });
        cx.run_until_parked();
        assert_ne!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(agent_id),
            "premise: a different agent really is the active tab before the merge starts"
        );

        app.update_in(cx, |app, window, cx| {
            app.start_merge_from_graph_branch("feature".to_string(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let flow = app
                .merge_flow
                .as_ref()
                .expect("the merge must land in the app's one existing merge flow");
            assert_eq!(
                flow.agent_id, agent_id,
                "and must be shown in the focused worktree's own agent tab"
            );
            assert_eq!(
                app.agents.active_id(),
                Some(agent_id),
                "that tab must really have been activated first - the resolver renders inside it,                  so a conflict must never land in a surface the user is not looking at"
            );
            match &flow.state {
                merge::MergeFlowState::Clean {
                    base_branch, files, ..
                } => {
                    assert_eq!(
                        base_branch, "main",
                        "the branch merged into must be the one really checked out here"
                    );
                    assert_eq!(files, &vec![std::path::PathBuf::from("b.txt")]);
                }
                other => panic!(
                    "expected a real clean merge state, got {}",
                    merge_state_name(other)
                ),
            }
        });
        assert!(
            wt_core::merge::merge_head_exists(local.path()).expect("merge_head_exists"),
            "the real merge must genuinely be in progress and uncommitted, waiting for the \
             existing resolver's own Complete merge"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.graph_state.status_message.clone()),
            None,
            "the graph tab's own \"Merging …\" line must not outlive the merge it described - \
             the resolver owns the outcome from here"
        );
    }

    #[gpui::test]
    async fn merge_into_current_branch_conflicting_lands_in_the_existing_conflict_resolver(
        cx: &mut TestAppContext,
    ) {
        let local = tempfile::tempdir().expect("tempdir");
        git(local.path(), &["init", "-b", "main"]);
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);
        commit(local.path(), "shared.txt", "line1\nline2\nline3\n", "base");
        git(local.path(), &["checkout", "-b", "feature"]);
        commit(
            local.path(),
            "shared.txt",
            "line1\nFEATURE\nline3\n",
            "feature changes shared",
        );
        git(local.path(), &["checkout", "main"]);
        commit(
            local.path(),
            "shared.txt",
            "line1\nMAIN\nline3\n",
            "main changes shared",
        );

        let (app, cx) = palette_focus_tests::open_test_app(cx, local.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                local.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });

        app.update_in(cx, |app, window, cx| {
            app.start_merge_from_graph_branch("feature".to_string(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            match &app.merge_flow.as_ref().expect("merge flow").state {
                merge::MergeFlowState::Conflicted { files, .. } => {
                    assert_eq!(
                        files.len(),
                        1,
                        "the one really conflicted file must be loaded into the existing \
                         resolver's own file list"
                    );
                    assert_eq!(files[0].relative_path(), std::path::Path::new("shared.txt"));
                }
                other => panic!(
                    "expected a real conflicted merge state, got {}",
                    merge_state_name(other)
                ),
            }
        });
        let on_disk =
            std::fs::read_to_string(local.path().join("shared.txt")).expect("read shared.txt");
        assert!(
            on_disk.contains("<<<<<<< HEAD") && on_disk.contains(">>>>>>> feature"),
            "and the real conflict markers must genuinely be on disk for it to resolve: \
             {on_disk:?}"
        );
    }

    /// The resolver renders inside an agent's work surface, so with no agent tab there is
    /// genuinely nowhere to show it. Refused honestly, with nothing started - never a merge run
    /// into a surface the user can't see.
    #[gpui::test]
    async fn merge_into_current_branch_refuses_with_no_agent_tab_in_the_worktree(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        // Close every agent in the focused worktree, so there is genuinely no surface left to
        // show the resolver in - the real state this refusal exists for.
        let existing: Vec<_> = app.read_with(cx, |app, _| {
            app.agents
                .iter_for_cwd(local.path().to_path_buf())
                .map(|agent| agent.id)
                .collect()
        });
        for id in existing {
            app.update_in(cx, |app, window, cx| app.close_agent(id, window, cx));
        }
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_merge_from_graph_branch("feature".to_string(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.merge_flow.is_none(),
                "no merge flow must have been started at all"
            );
            let status = app.graph_state.status_message.clone();
            assert!(
                status
                    .as_deref()
                    .is_some_and(|text| text.contains("no agent tab")),
                "and the refusal must say why, rather than looking like a dead click - got \
                 {status:?}"
            );
        });
        assert!(
            !wt_core::merge::merge_head_exists(local.path()).expect("merge_head_exists"),
            "and no real git merge must have been run"
        );
    }

    #[gpui::test]
    async fn merge_into_current_branch_refuses_while_another_merge_is_in_flight(
        cx: &mut TestAppContext,
    ) {
        let (local, app, cx) = open_seeded_with_feature_branch(cx);
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                local.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.start_merge_from_graph_branch("feature".to_string(), window, cx);
        });
        cx.run_until_parked();
        let generation_after_first = app.read_with(cx, |app, _| {
            app.merge_flow
                .as_ref()
                .map(|flow| flow.generation)
                .expect("premise: the first merge really is live")
        });

        app.update_in(cx, |app, window, cx| {
            app.start_merge_from_graph_branch("feature".to_string(), window, cx);
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, _| app.graph_state.status_message.clone());
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.contains("already in progress")),
            "a second merge while one is live must be refused with a real reason - got {status:?}"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .merge_flow
                .as_ref()
                .map(|flow| flow.generation)),
            Some(generation_after_first),
            "and must not have started a second attempt over the first - a fresh attempt always \
             bumps the generation stamp (`MergeFlow::generation`)"
        );
        assert!(
            wt_core::merge::merge_head_exists(local.path()).expect("merge_head_exists"),
            "the first merge must still be the one in progress"
        );
    }
}
