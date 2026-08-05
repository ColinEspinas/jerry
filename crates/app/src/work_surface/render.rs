use super::*;
use crate::root::widgets::{
    hover_bg, menu_popover_chrome, render_action_keycap_row, render_env_chip, render_hint_pair,
    render_keycap_row, text_tooltip, KeycapSize,
};
use gpui::{Animation, AnimationExt, DragMoveEvent};
use std::time::Duration;

/// Defines one `JumpToAgentN` action handler forwarding a literal position to
/// [`AdeApp::jump_to_agent_at`]. Each `actions!`-generated struct is a distinct action type
/// with no positional data, so GPUI needs one `on_action` handler per keystroke regardless; this
/// macro just keeps the eight near-identical bodies from drifting from each other.
macro_rules! agent_jump_action_handler {
    ($fn_name:ident, $action:ty, $position:expr) => {
        pub(crate) fn $fn_name(
            &mut self,
            _action: &$action,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.jump_to_agent_at($position, window, cx);
        }
    };
}

/// A tab chrome click handler - middle-click-to-close and click-to-activate share this exact
/// shape (`&mut AdeApp, &mut Window, &mut Context<AdeApp>`), factored into a real alias per
/// clippy's own `type_complexity` suggestion rather than spelling the `Box<dyn Fn(...)>` out
/// twice in [`TabChromeArgs`].
pub(crate) type TabChromeClickHandler = Box<dyn Fn(&mut AdeApp, &mut Window, &mut Context<AdeApp>)>;

/// Bundles [`AdeApp::render_tab_chrome`]'s per-kind parameters to keep that function's argument
/// count under clippy's `too_many_arguments` limit - the same reason
/// `code_surface::file_view::HoverRenderContext` exists. `on_middle_click`/`on_activate` are
/// boxed rather than generic type parameters so this struct itself stays non-generic (a distinct
/// monomorphization per call site would buy nothing here - this is render-path code re-built
/// every frame regardless).
pub(crate) struct TabChromeArgs {
    pub(crate) outer_id: gpui::ElementId,
    pub(crate) hit_id: gpui::ElementId,
    pub(crate) tab_ref: work_surface::TabRef,
    pub(crate) drag_value: DraggedTab,
    pub(crate) is_active: bool,
    pub(crate) content: Vec<gpui::AnyElement>,
    pub(crate) on_middle_click: TabChromeClickHandler,
    pub(crate) on_activate: TabChromeClickHandler,
    /// A real GPUI `.debug_selector` (test-only bounds lookup, distinct from `outer_id` - see
    /// `gpui::app::test_context::VisualTestContext::debug_bounds`'s own docs), for whichever tab
    /// kind's own tests need to simulate a real mouse event at this tab's painted position rather
    /// than calling its close/activate handler directly. `None` for kinds with no such test.
    pub(crate) debug_selector: Option<&'static str>,
}

impl AdeApp {
    /// Spawns a new agent tab into [`Self::active_agent_cwd`] - the single real chokepoint every
    /// "new terminal"/"new shell" entry point in this app funnels through: `secondary-n`/
    /// `ctrl-shift-T`'s own `handle_new_agent_action`/`handle_new_terminal_action`, the `+` menu's
    /// row, the title bar's Agent menu row (`crate::title_bar::menu::AdeApp::agent_menu_rows`),
    /// and the palette's `PaletteCommand::NewShell`.
    ///
    /// GitHub issue #90: a genuinely empty window (no [`Self::focused_repo`]) has no real repo
    /// root to spawn into at all - a real, live-reproduced bug (independent audit) found that
    /// without this guard, [`Self::active_agent_cwd`] fell through to [`Self::focused_repo_path`],
    /// which itself used to fall back to *some other, unopened* known repo's real path (`Self::
    /// repos.first()`), silently spawning a real PTY - and, from there, reachable real destructive
    /// git operations (`Keep All Changes`/`Discard Worktree`) - against a repo the user never
    /// opened and can't even see (the empty-state view renders no tab strip at all, so the tab
    /// this spawned would have been invisible). A no-op here is the honest fix: there is nothing
    /// this app can offer to spawn an agent into until a real repo is opened.
    pub(crate) fn new_agent(
        &mut self,
        kind: AgentKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focused_repo().is_none() {
            return;
        }
        let cwd = self.active_agent_cwd();
        self.agents.spawn(
            kind,
            cwd,
            self.settings.appearance.terminal_font_size,
            window,
            cx,
        );
        self.focus_newly_spawned_agent(window, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// Moves focus onto the agent [`Agents::spawn`] just made active - but only when neither
    /// a file tab ([`Self::render_center_pane`] renders the file tab in that case, not a
    /// agent's `TerminalPane`) nor Settings ([`Self::settings_open`] - Settings replaces the
    /// entire workspace body, per `crate::root::mod`'s own docs, so no agent's pane is
    /// rendered anywhere while it's showing) is occupying the centre pane instead, since focusing
    /// an agent's pane while either is true would point `Window::focus` at a node nothing in the
    /// rendered tree tracks. Reachable with Settings open via the title bar's Agent menu (New
    /// Terminal/New Agent Pane), which is an unconditional sibling of the Settings/workspace-body
    /// swap.
    pub(crate) fn focus_newly_spawned_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_change.is_none() && !self.settings_open && !self.graph_tab_active {
            self.agents.focus_active(window, cx);
        }
    }

    pub(crate) fn handle_new_agent_action(
        &mut self,
        _action: &NewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent(AgentKind::Shell, window, cx);
    }

    /// `Ctrl+W` (GitHub issue #26) - closes whichever tab the centre pane is genuinely showing
    /// right now: a file tab (via [`crate::code_surface::tabs::AdeApp::request_close_file_tab`],
    /// the real unsaved-changes-confirming entry point every other close gesture already uses) if
    /// [`AdeApp::open_change`] is `Some`, else the globally active agent tab (via
    /// [`Self::close_agent`], which already tears down its real child process cleanly - SIGHUP,
    /// a bounded grace period, then `SIGKILL` - see `pty_core::PtySession::shutdown`'s own docs;
    /// nothing here reimplements that). A genuine no-op, never a window close, whenever there is
    /// no real tab to close: Settings is showing over the workspace body (`AdeApp::settings_open`,
    /// meaning nothing tab-like is on screen to act on), or the active worktree already has no
    /// open tab at all (the real "last tab closed" end state this app leaves alone rather than
    /// spawning a replacement or closing the window - this app registers no window-close
    /// keybinding at all, on any platform, so there is no native "Ctrl+W closes the window"
    /// default here to accidentally fall back to in the first place).
    ///
    /// Scoped to `Some("!terminal")` in `crate::default_key_bindings` (not global) - plain
    /// `Ctrl+W` is `crate::terminal::pane::keystroke_to_bytes`'s own real `unix-word-rerase`
    /// control byte (`0x17`) a focused shell needs for its own word-backspace; see that list's
    /// own docs (and its `secondary-z`/`"]"` entries) for this project's established precedent
    /// for exactly this "app-level shortcut steals terminal input" conflict class (`secondary-p`
    /// is the one deliberate exception to that precedent, not a further example of it - see that
    /// binding's own docs) - a live terminal pane keeps `Ctrl+W`, and closing a *terminal* tab
    /// this way stays reachable through the tab strip's own `×`/middle-click instead.
    pub(crate) fn handle_close_focused_tab_action(
        &mut self,
        _action: &CloseFocusedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open {
            return;
        }
        if let Some(path) = self.open_change.clone() {
            self.request_close_file_tab(path, window, cx);
            return;
        }
        if let Some(id) = self.agents.active_id() {
            self.close_agent(id, window, cx);
            cx.notify();
        }
    }

    /// GitHub issue #20's "stays rebindable" requirement for the terminal footer's `clear`
    /// action - see [`Self::render_pty_info_footer`] for the click entry point this shares
    /// [`crate::terminal::pane::TerminalPane::clear`] with. Scoped to `Some("terminal")` in
    /// `crate::default_key_bindings`, exactly like [`Self::handle_close_focused_tab_action`]'s
    /// own `Some("!terminal")` - a real terminal keeps its own control bytes for anything not
    /// bound here, and this only ever fires while a `TerminalPane` genuinely has focus. Acts on
    /// whichever agent is currently active, matching `handle_close_focused_tab_action`'s own
    /// "the centre pane is genuinely showing right now" target.
    pub(crate) fn handle_terminal_clear_action(
        &mut self,
        _action: &TerminalClear,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent) = self.agents.active() {
            agent.pane.clone().update(cx, |pane, cx| pane.clear(cx));
        }
    }

    /// GitHub issue #158's terminal Copy. Scoped to `Some("terminal")` in
    /// `crate::default_key_bindings` (`Ctrl+Shift+C`, `Cmd+C` on macOS) for the reason that
    /// scoping exists at all here: plain `Ctrl+C` is the pty's own `SIGINT` byte and must never
    /// be claimed as a copy shortcut, which is exactly why every terminal emulator puts copy on
    /// the shifted variant instead. Targets the active agent's pane, matching
    /// [`Self::handle_terminal_clear_action`]'s own "the centre pane is genuinely showing right
    /// now" target.
    pub(crate) fn handle_terminal_copy_action(
        &mut self,
        _action: &TerminalCopy,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent) = self.agents.active() {
            agent
                .pane
                .clone()
                .update(cx, |pane, cx| pane.copy_selection(cx));
        }
    }

    /// GitHub issue #158's terminal Paste - the counterpart to
    /// [`Self::handle_terminal_copy_action`], on `Ctrl+Shift+V` (`Cmd+V` on macOS) for the same
    /// reason (plain `Ctrl+V` is the pty's own `0x16`, readline's `quoted-insert`).
    pub(crate) fn handle_terminal_paste_action(
        &mut self,
        _action: &TerminalPaste,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent) = self.agents.active() {
            agent
                .pane
                .clone()
                .update(cx, |pane, cx| pane.paste_from_clipboard(cx));
        }
    }

    /// Activates agent `id`'s tab and, if it maps to a currently-listed worktree, also selects
    /// that worktree, keeping the file tree/diff sidebar in sync with the agent just clicked
    /// (the sidebar is still driven by [`Self::selected`] - a `focused_agent`-driven Zone 2/3
    /// hasn't been rebuilt yet). If a file tab was active, this deactivates it
    /// (`Self::open_change = None`, without closing it - it stays in [`Self::open_files`]) and
    /// restores focus onto the agent's pane via [`restore_focus`].
    pub(crate) fn select_agent(
        &mut self,
        id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agents.set_active(id, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        // If the git graph tab was showing, this leaves it (without closing its tab) - see
        // `crate::graph_view::render::AdeApp::leave_graph_tab`'s own docs for why this must run
        // *after* `set_active` above: its `restore_focus` fallback resolves to
        // `Self::agents.active()`, which by this point is already the agent just selected.
        self.leave_graph_tab(window, cx);
        let had_open_file_tab = self.open_change.is_some();
        if had_open_file_tab {
            self.open_change = None;
            self.refresh_open_diff_file_cache();
            self.hover = None;
            // See `crate::code_surface::tabs::AdeApp::open_and_focus_file`'s identical
            // `dismiss_completions()` call for why (Revision R8.5b audit finding 3).
            self.dismiss_completions();
            if self.settings_open {
                // Settings is showing over the whole workspace body right now (reachable here
                // via the title bar's Agent menu cycle rows/Archive Agent, unconditional
                // siblings of the Settings/workspace-body swap) - real focus already correctly
                // lives on `settings_focus_handle`. Discard the captured pre-file-tab target
                // rather than restoring it onto an agent pane `Self::render_settings` isn't
                // drawing, mirroring `Self::close_palette`'s identical Settings-aware branch
                // (`self.palette_focus.clear()`).
                self.code_focus.clear();
            } else {
                restore_focus(&self.agents, &mut self.code_focus, window, cx);
            }
        }
        let cwd = self
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| agent.cwd.clone());
        if let Some(cwd) = cwd {
            if let Some(index) = self.worktrees.iter().position(|item| item.path == cwd) {
                if self.selected != Some(index) {
                    self.select_worktree(index, window, cx);
                    return;
                }
            }
        }
        // GitHub issue #112: when no file tab was showing, nothing above moves real keyboard
        // focus - `restore_focus` only runs inside the `had_open_file_tab` branch, and a
        // same-worktree agent switch (the common case for a rail/tab-strip click between two
        // already-open terminals) never reaches `select_worktree` either. Left as-is, `Window::
        // focus` stays on the previously-active agent's `TerminalPane` handle, which
        // `render_center_pane` no longer mounts once a different agent becomes active - GPUI's
        // dispatch then falls back to the window root, outside the `"terminal"` key context, so
        // typed input silently goes nowhere and normally-suppressed global bindings (e.g. Ctrl+W)
        // fire instead. `had_open_file_tab` is checked, not `self.open_change.is_none()` (always
        // true here since the branch above clears it): reusing `focus_newly_spawned_agent`
        // unconditionally would override `restore_focus`'s more precise restore target when
        // re-selecting an already-active agent that had a file tab open (Revision R8.5b's
        // captured-overlay-focus mechanism).
        if !had_open_file_tab {
            self.focus_newly_spawned_agent(window, cx);
        }
        cx.notify();
    }

    /// Derives the [`Status`] for a live agent - the single source of truth both
    /// [`Self::build_agent_rows`] (the rail) and the work surface (status pill, pane header/
    /// footer) read, so the rail and the work surface can never disagree about an agent's
    /// status.
    pub(crate) fn agent_status(&self, agent: &Agent, cx: &App) -> Status {
        let pane = agent.pane.read(cx);
        let signal = if pane.is_running() {
            status::ProcessSignal::Running {
                idle: pane.idle_duration().unwrap_or_default(),
            }
        } else if let Some(exit) = pane.exit_status() {
            status::ProcessSignal::Exited {
                success: exit.success(),
            }
        } else if pane.spawn_error().is_some() {
            // A process that never started still counts as a failure, even though it has no
            // `ExitStatus` to report.
            status::ProcessSignal::Exited { success: false }
        } else {
            status::ProcessSignal::NoProcess
        };
        let has_diff = self
            .diff_cache
            .get(&agent.cwd)
            .map(|summary| summary.has_changes)
            .unwrap_or(false);
        status::derive_status(agent.kind, signal, has_diff)
    }

    /// The context bar's and idle-status footer's `Archive` action - closes the tab via
    /// [`Self::close_agent`] (see that method's docs for why every close path must go through
    /// it rather than `Agents::close` directly).
    pub(crate) fn archive_agent(
        &mut self,
        id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_agent(id, window, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// Closes agent `id`'s tab (`Agents::close` tears down its child process and moves focus
    /// onto whichever agent becomes active) and, if `id` is the agent whose `Merge` click
    /// started [`Self::merge_flow`], cleans that up too (see
    /// [`Self::clear_merge_flow_for_closed_agent`]).
    ///
    /// Every close path - [`Self::archive_agent`], [`Self::respawn_agent`]'s
    /// close-then-respawn, and the tab strip's own `×` - must go through this function rather
    /// than `Agents::close` directly: previously only `archive_agent` cleared `merge_flow`,
    /// so archiving (or retrying) a mid-merge agent left `merge_flow.agent_id` pointing at a
    /// agent that no longer existed, permanently disabling the `Merge` button for every
    /// agent (`Self::render_merge_button`'s disabled check never cleared).
    ///
    /// Tells `Agents::close` to skip its own focus move whenever the centre pane isn't
    /// actually showing an agent's pane right now - a file tab is open, *or* Settings has
    /// replaced the whole workspace body (`Self::settings_open`, see the title bar's Agent
    /// menu docs - Archive Agent is reachable from there while Settings is showing, and
    /// moving focus onto a pane `Self::render_settings` isn't drawing would dangle it exactly
    /// like the file-tab case this guard already covered).
    ///
    /// If closing `id` leaves its worktree with no agent at all (and no file tab either), real
    /// keyboard focus falls back onto [`Self::rail_focus_handle`] - the same fallback
    /// [`Self::select_worktree`] uses for the identical "nothing left to focus" case - so
    /// `Window::focus` never stays pointed at the just-`shutdown()`, no-longer-rendered pane. The
    /// rail's *root*, not its filter field (which this used to target): see
    /// [`Self::rail_focus_handle`]'s own docs for the real keystroke-swallowing bug that was.
    pub(crate) fn close_agent(&mut self, id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        // The graph tab (like a file tab or Settings) occupies the centre pane instead of an
        // agent's own `TerminalPane` while active, so `Agents::close`'s own focus-follows-close
        // move onto the newly active agent's pane would be exactly as dangling as the
        // file-tab/Settings cases this guard already covers - a real, adversarial-audit-found
        // gap: the sibling guard in `Self::focus_newly_spawned_agent` was updated for this, this
        // one was not.
        let skip_focus_move =
            self.open_change.is_some() || self.settings_open || self.graph_tab_active;
        self.agents.close(id, skip_focus_move, window, cx);
        if self
            .merge_flow
            .as_ref()
            .is_some_and(|flow| flow.agent_id == id)
        {
            self.clear_merge_flow_for_closed_agent(cx);
        }
        if self.agents.active_id().is_none()
            && self.open_change.is_none()
            && !self.settings_open
            && !self.graph_tab_active
        {
            window.focus(&self.rail_focus_handle, cx);
        }
    }

    /// The surface footer's `Interrupt ⌃C` action - sends `Ctrl-C` to the agent's pty via
    /// `TerminalPane::interrupt`.
    pub(in crate::work_surface) fn interrupt_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let pane = agent.pane.clone();
        pane.update(cx, |pane, cx| pane.interrupt(cx));
    }

    /// The surface footer's `Retry ⌘R` (failed agents) / `Resume ⌘⏎` (idle agents) action.
    /// This app has no saved-agent resumability to resume *from* (see
    /// `crate::work_surface::state::pty_state_label`'s docs), so the honest equivalent is: close this
    /// tab, then spawn a fresh agent of the same kind into the same worktree - not literally
    /// "resume where it left off" (`crate::work_surface::state::ActionKind::Respawn`'s docs name this
    /// trade-off).
    pub(in crate::work_surface) fn respawn_agent(
        &mut self,
        id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let kind = agent.kind;
        let cwd = agent.cwd.clone();
        self.close_agent(id, window, cx);
        self.agents.spawn(
            kind,
            cwd,
            self.settings.appearance.terminal_font_size,
            window,
            cx,
        );
        self.focus_newly_spawned_agent(window, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// The surface footer's `Open terminal` action - selects an already-open `Shell` agent in
    /// the same worktree, or spawns one if none exists. Each agent is its own independent tab/
    /// process (`crate::work_surface::agents`'s module docs), so "open terminal" just means "get me a shell
    /// in this worktree", the same capability as the rail's "+ New Shell" button.
    pub(in crate::work_surface) fn open_companion_terminal(
        &mut self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .agents
            .iter()
            .find(|agent| agent.kind == AgentKind::Shell && agent.cwd == cwd)
            .map(|agent| agent.id);
        match existing {
            Some(id) => self.select_agent(id, window, cx),
            None => {
                self.agents.spawn(
                    AgentKind::Shell,
                    cwd,
                    self.settings.appearance.terminal_font_size,
                    window,
                    cx,
                );
                self.focus_newly_spawned_agent(window, cx);
                self.prune_confirm_armed = false;
                self.discard_confirm_armed = None;
                cx.notify();
            }
        }
    }

    /// The active worktree's real combined tab order (GitHub issue #16) - every agent and file
    /// tab currently open in it, interleaved exactly as [`Self::render_tab_strip`] draws them,
    /// instead of always "every agent, then every file". Reconciled fresh from
    /// [`Self::tab_order`]'s stored order plus whatever's *actually* open right now
    /// (`work_surface::state::reconcile_tab_order`'s own docs on why this is safe to call on
    /// every render rather than caching a mutated copy) - [`Self::tab_order`] itself only records
    /// a user's real drag-chosen order, never which tabs exist; that's still `Agents`/
    /// [`Self::open_files`]'s job.
    ///
    /// GitHub issue #120: an earlier revision (R12 §3, "a bare worktree shows only the shell
    /// tab") truncated this to the active file only (or nothing) whenever a worktree had no real
    /// agent running - reconciling against an empty file list instead of the real
    /// [`Self::open_files`]. That's gone: every open file tab stays visible regardless of whether
    /// an agent, or even the default shell, is running - opening and working across files was
    /// never meant to depend on a shell/agent existing. Bareness still affects how the shell's
    /// own tab is *labeled* (`Self::current_worktree_agent_tab_labels`'s bare-shell label), just
    /// not which tabs render at all.
    pub(crate) fn combined_tab_order(&self) -> Vec<work_surface::TabRef> {
        let cwd = self.active_agent_cwd();
        let agents_for_cwd: Vec<&Agent> = self.agents.iter_for_cwd(cwd.clone()).collect();
        let agent_ids: Vec<AgentId> = agents_for_cwd.iter().map(|agent| agent.id).collect();
        // `Self::tab_order` hasn't been touched for yet *this session* falls back to its real,
        // on-disk order (GitHub issue #16's own "persists... and restores on relaunch") instead
        // of the empty slice a brand new session would otherwise reconcile against - see
        // `Self::tab_order`'s own docs. Recomputed fresh each call rather than cached back into
        // `Self::tab_order`: `crate::work_surface::tab_order_state::TabOrderState` is already
        // fully loaded in memory (no I/O here), and leaving `Self::tab_order` itself untouched
        // until a real drag happens is what keeps `Self::tab_order_owned` scoped to worktrees
        // this instance has actually *changed*, not merely visited.
        let persisted_fallback;
        let stored: &[work_surface::TabRef] = match self.tab_order.get(&cwd) {
            Some(order) => order.as_slice(),
            None => {
                persisted_fallback = self
                    .tab_order_state
                    .file_order(&cwd)
                    .into_iter()
                    .filter_map(|absolute| absolute.strip_prefix(&cwd).ok().map(Path::to_path_buf))
                    .map(work_surface::TabRef::File)
                    .collect::<Vec<_>>();
                &persisted_fallback
            }
        };
        work_surface::reconcile_tab_order(
            stored,
            &agent_ids,
            self.open_files(),
            self.graph_tab_open,
        )
    }

    /// The unified tab strip's real drag-to-reorder entry point (GitHub issue #16) - moves
    /// `dragged` to sit immediately before (or, if `insert_after`, immediately after) `target` in
    /// the active worktree's own combined tab order, regardless of whether either is an agent or
    /// a file tab (`work_surface::state::move_tab_order`'s own docs on why this is one function,
    /// not a per-kind pair). Persists the result into [`Self::tab_order`], keyed by the active
    /// worktree's cwd, so it survives the next render's [`Self::combined_tab_order`]
    /// reconciliation and, for agent tabs, a later worktree switch away and back. Never
    /// restarts a pty or reloads a file buffer - `Agents`/[`Self::open_files`] themselves are
    /// untouched; only this ordering layer changes.
    pub(in crate::work_surface) fn reorder_tab(
        &mut self,
        dragged: work_surface::TabRef,
        target: work_surface::TabRef,
        insert_after: bool,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.active_agent_cwd();
        let mut order = self.combined_tab_order();
        work_surface::move_tab_order(&mut order, &dragged, &target, insert_after);
        self.tab_order.insert(cwd.clone(), order.clone());

        // The file half of the same order, persisted to disk (GitHub issue #16) - see
        // `work_surface::tab_order_state`'s own module docs for why agent-tab entries are never
        // recorded here at all.
        let files: Vec<PathBuf> = order
            .iter()
            .filter_map(|tab_ref| match tab_ref {
                work_surface::TabRef::File(path) => Some(cwd.join(path)),
                work_surface::TabRef::Agent(_) | work_surface::TabRef::Graph => None,
            })
            .collect();
        self.tab_order_state.set_file_order(&cwd, &files);
        if let Some(key) = crate::work_surface::tab_order_state::worktree_key(&cwd) {
            self.tab_order_owned.insert(key);
        }
        self.persist_tab_order(cx);
        cx.notify();
    }

    /// Queues a background-executor save of [`Self::tab_order_state`] to
    /// [`Self::tab_order_path`] - the write-side counterpart to [`Self::reorder_tab`]'s own
    /// read-side fallback. A genuine no-op with a `None` path (every GPUI test that hasn't opted
    /// into a real one). The write is a *merge*
    /// (`crate::work_surface::tab_order_state::TabOrderState::save_merged_at` against
    /// [`Self::tab_order_owned`]), matching [`Self::persist_fold_state`]'s own reasoning: a
    /// second `jerry` instance browsing a different repository is writing the same file, and a
    /// whole-file write would erase its saved order.
    pub(in crate::work_surface) fn persist_tab_order(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.tab_order_path.clone() else {
            return;
        };
        let state = self.tab_order_state.clone();
        let owned = self.tab_order_owned.clone();
        let task = cx.spawn(async move |_this, cx| {
            let save_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { state.save_merged_at(&save_path, &owned) })
                .await;
            if let Err(err) = result {
                log::warn!("failed to save {}: {err}", path.display());
            }
        });
        self._tab_order_save_task = Some(task);
    }

    /// One tab's own `on_drag_move::<DraggedTab>` handler (both [`Self::render_agent_tab`] and
    /// [`Self::render_file_tab`] register this, each closing over its own `hovered` [`work_surface::
    /// TabRef`]) - real per-tab mouse tracking during a drag, not a container-level listener:
    /// `on_drag_move`'s own doc comment and `crate::root::scrollbar`'s module docs both confirm
    /// GPUI dispatches a matching `on_drag_move::<T>` to *every* mounted element of that type on
    /// every drag-move tick, each receiving its own `event.bounds` - so every tab checks whether
    /// the live cursor (`event.event.position`) actually falls inside its own bounds before
    /// claiming the insertion caret; whichever tab's bounds the cursor is really over is the one
    /// that wins, on the very next tick after the cursor crosses into it. Splits `hovered`'s own
    /// width in half (`Bounds::center`) to decide "before" (`insert_after = false`, left half) vs.
    /// "after" (`insert_after = true`, right half) - the exact cursor-position precision GitHub
    /// issue #16 asks for, replacing the old whole-tab `border_l` highlight that couldn't tell
    /// the two apart. A no-op while the dragged tab is hovering over *itself* - dropping a tab on
    /// its own slot is always a no-op ([`work_surface::state::move_tab_order`]'s own docs), so no
    /// caret should invite it either.
    pub(in crate::work_surface) fn update_tab_drag_insertion(
        &mut self,
        hovered: &work_surface::TabRef,
        event: &DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        if event.drag(cx).tab_ref() == *hovered {
            return;
        }
        if !event.bounds.contains(&event.event.position) {
            return;
        }
        let insert_after = event.event.position.x >= event.bounds.center().x;
        if self.tab_drag_insertion.as_ref() != Some(&(hovered.clone(), insert_after)) {
            self.tab_drag_insertion = Some((hovered.clone(), insert_after));
            cx.notify();
        }
    }

    /// One tab's own `on_drop::<DraggedTab>` handler (both [`Self::render_agent_tab`] and
    /// [`Self::render_file_tab`] register this) - reads which half of `target`'s own tab the
    /// cursor last landed on from [`Self::tab_drag_insertion`] (defaulting to "before" if the
    /// drop lands on a tab [`Self::update_tab_drag_insertion`] never actually recorded for - a
    /// drop can still fire on a tab the cursor technically never entered, e.g. a very fast
    /// release), then delegates to [`Self::reorder_tab`], and clears the now-stale caret state.
    pub(in crate::work_surface) fn drop_dragged_tab(
        &mut self,
        dragged: work_surface::TabRef,
        target: work_surface::TabRef,
        cx: &mut Context<Self>,
    ) {
        let insert_after = self
            .tab_drag_insertion
            .as_ref()
            .is_some_and(|(hovered, after)| *hovered == target && *after);
        self.next_tab_settle_id += 1;
        self.dropped_tab_settle = Some((dragged.clone(), self.next_tab_settle_id));
        self.reorder_tab(dragged, target, insert_after, cx);
        self.tab_drag_insertion = None;
        self.dragging_tab = None;
    }

    /// One tab's own `on_drag` constructor callback (both [`Self::render_agent_tab`] and
    /// [`Self::render_file_tab`] call this) - records `tab_ref` into [`Self::dragging_tab`] so
    /// that tab's own slot can dim itself while its ghost is the real thing following the
    /// cursor. A plain method (not inlined into the closure) so it's directly testable without
    /// simulating a real GPUI drag gesture, matching [`Self::update_tab_drag_insertion`]/
    /// [`Self::drop_dragged_tab`]'s own precedent.
    pub(in crate::work_surface) fn start_dragging_tab(
        &mut self,
        tab_ref: work_surface::TabRef,
        cx: &mut Context<Self>,
    ) {
        self.dragging_tab = Some(tab_ref);
        cx.notify();
    }

    /// Clears any in-progress tab drag's tracked state - the real cancelled-drag path (Esc, or
    /// releasing outside any tab's own drop target) that GPUI gives no dedicated callback for
    /// (see [`Self::dragging_tab`]'s own docs). `crate::root::AdeApp`'s workspace-body
    /// `on_mouse_up` is this method's only real caller; returns whether anything was actually
    /// cleared so that caller only `cx.notify()`s when something changed, rather than on every
    /// unrelated click in the window.
    pub(crate) fn cancel_any_tab_drag(&mut self) -> bool {
        let cleared_insertion = self.tab_drag_insertion.take().is_some();
        let cleared_dragging = self.dragging_tab.take().is_some();
        cleared_insertion || cleared_dragging
    }

    /// Every agent open in the *currently selected* worktree (`Self::active_agent_cwd`), in
    /// the same order [`Self::combined_tab_order`] renders them - never Agents' own raw
    /// creation order once a real drag has interleaved them differently, and never every agent
    /// across every worktree, per this revision's whole point (see `crate::root::mod`'s "One
    /// rail row per worktree" docs). The real per-worktree tab-strip order both
    /// [`Self::render_tab_strip`] and [`Self::agent_jump_keys`]/[`Self::jump_to_agent_at`]
    /// share, so the tabs shown and the tabs a jump keycap can reach can never disagree.
    pub(crate) fn current_worktree_agents(&self) -> impl Iterator<Item = &Agent> {
        let order = self.combined_tab_order();
        order.into_iter().filter_map(move |tab_ref| match tab_ref {
            work_surface::TabRef::Agent(id) => self.agents.iter().find(|agent| agent.id == id),
            work_surface::TabRef::File(_) | work_surface::TabRef::Graph => None,
        })
    }

    /// Whether the *currently selected* worktree has zero real agents - at most a default
    /// `Shell` tab (Revision R12 §3: "a bare worktree shows only the shell tab"). One predicate
    /// shared by [`Self::current_worktree_agent_tab_labels`]'s `zsh \u{b7} <branch>` shell-tab
    /// label and [`Self::render_agent_context_bar`]'s `Merge`/`Archive` -> `Start an agent`
    /// swap, so the two can never disagree about what "bare" means. Vacuously `true` when the
    /// worktree has no agent at all - callers that reach a context bar or a tab strip already
    /// know at least one agent exists ([`Self::render_center_pane`]'s `None` branch handles the
    /// empty case separately).
    pub(crate) fn current_worktree_is_bare(&self) -> bool {
        !self
            .current_worktree_agents()
            .any(|agent| agent.kind != AgentKind::Shell)
    }

    /// The branch label for the currently selected worktree, if any is recorded for it - shared
    /// by [`Self::current_worktree_agent_tab_labels`]'s bare-shell label and
    /// [`Self::render_agent_context_bar`]'s own branch lookup so both read the same fact the
    /// same way.
    fn current_worktree_branch(&self) -> Option<String> {
        let cwd = self.active_agent_cwd();
        self.worktrees
            .iter()
            .find(|item| item.path == cwd)
            .and_then(|item| item.branch.clone())
    }

    /// The tab label each of `agent_ids` (already filtered to the currently selected worktree)
    /// should render, in the same order - real `TerminalPane::program_label` facts (the
    /// bare-worktree shell gets `work_surface::bare_worktree_shell_label` instead of the generic
    /// `"terminal"`) run through `work_surface::disambiguate_tab_labels`, computed once so
    /// [`Self::render_tab_strip`] and this method (also used directly by tests) can never
    /// disagree about what's actually rendered. Any id not found in `Self::agents` is skipped
    /// rather than panicking - `render_tab_strip` re-does the same lookup per id and simply
    /// omits a tab for it too.
    pub(crate) fn current_worktree_agent_tab_labels(
        &self,
        agent_ids: &[AgentId],
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let is_bare = self.current_worktree_is_bare();
        let branch = self.current_worktree_branch();
        let raw: Vec<String> = agent_ids
            .iter()
            .filter_map(|id| self.agents.iter().find(|agent| agent.id == *id))
            .map(|agent| match work_surface::tab_chip_kind(agent.kind) {
                work_surface::TabChipKind::Cli => agent.pane.read(cx).program_label(),
                work_surface::TabChipKind::Term => {
                    if is_bare {
                        work_surface::bare_worktree_shell_label(
                            &agent.pane.read(cx).program_label(),
                            branch.as_deref(),
                        )
                    } else {
                        "terminal".to_string()
                    }
                }
            })
            .collect();
        work_surface::disambiguate_tab_labels(raw)
    }

    /// The tab strip: one tab per entry of [`Self::combined_tab_order`], in that exact order -
    /// [`Self::render_agent_tab`] for a `TabRef::Agent`, [`Self::render_file_tab`] for a
    /// `TabRef::File` - so an agent tab and a file tab can sit side by side in either order
    /// (GitHub issue #16), rather than always "every agent, then every file" - followed by the
    /// `+` menu button ([`Self::render_tab_strip_plus`]) and right-aligned agent-jump keycaps.
    ///
    /// Agent labels are computed once, for every agent tab in this worktree together
    /// (`Self::current_worktree_agent_tab_labels`), *before* rendering any individual tab - the
    /// ordinal-disambiguation pass (Revision R12 §3) needs every label in hand up front to tell
    /// which ones repeat, so it can't be done tab-by-tab inside the loop below.
    pub(in crate::work_surface) fn render_tab_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut bar = div()
            .id("tab-strip")
            .flex()
            .flex_none()
            .items_stretch()
            .h(theme::band::CHROME_HEADER)
            .bg(theme::surface::TITLE_BAR)
            .border_b_1()
            .border_color(theme::border::ZONE);

        let order = self.combined_tab_order();
        let agent_ids: Vec<AgentId> = order
            .iter()
            .filter_map(|tab_ref| match tab_ref {
                work_surface::TabRef::Agent(id) => Some(*id),
                work_surface::TabRef::File(_) | work_surface::TabRef::Graph => None,
            })
            .collect();
        let labels = self.current_worktree_agent_tab_labels(&agent_ids, cx);
        let mut label_by_id: std::collections::HashMap<AgentId, String> =
            agent_ids.into_iter().zip(labels).collect();

        for tab_ref in order {
            match tab_ref {
                work_surface::TabRef::Agent(id) => {
                    if let Some(agent) = self.agents.iter().find(|agent| agent.id == id) {
                        let label = label_by_id.remove(&id).unwrap_or_default();
                        bar = bar.child(self.render_agent_tab(agent, label, cx));
                    }
                }
                work_surface::TabRef::File(path) => {
                    bar = bar.child(self.render_file_tab(&path, cx));
                }
                // GitHub issue #93: the git graph tab is now a real member of the same combined
                // order every agent/file tab already goes through - draggable, and its own
                // per-worktree position remembered - rather than a fixed, un-reorderable third
                // slot always rendered after every other tab. See `crate::graph_view::render::
                // render_graph_tab`'s own docs for the drag wiring this required.
                work_surface::TabRef::Graph => {
                    bar = bar.child(crate::graph_view::render::render_graph_tab(self, cx));
                }
            }
        }

        bar = bar.child(self.render_tab_strip_plus(cx));

        let jump_keys = self.agent_jump_keys();

        bar.child(div().flex_1()).child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .child(render_keycap_row(&jump_keys, KeycapSize::Standard))
                .child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(px(10.0))
                        .text_color(theme::text::PATH)
                        .child("agent"),
                ),
        )
    }

    /// The real `secondary-1`..`secondary-8` agent-jump keycap labels: one per agent open in
    /// the *currently selected* worktree (`Self::current_worktree_agents`), capped at 8 since
    /// those are the only ones actually bound (`crate::default_key_bindings`) - never a keycap
    /// advertising a shortcut that silently does nothing. Shared by [`Self::render_tab_strip`]'s
    /// own right-aligned cluster and the status bar's agent hint (`status_bar::render::
    /// render_status_agent_hint`), so the two can never independently drift on what's really
    /// bound.
    pub(crate) fn agent_jump_keys(&self) -> Vec<String> {
        let agent_count = self.current_worktree_agents().count().min(8);
        (1..=agent_count).map(|n| n.to_string()).collect()
    }

    /// Every tab kind's own `×` close hit box - identical id-suffixing, size, hover, and styling
    /// regardless of which kind renders it (GitHub issue #103). Before this, `render_file_tab`/
    /// `render_agent_tab`/`render_graph_tab` each hand-rolled their own copy, which is exactly
    /// how GitHub issue #96 happened: two of three tab kinds grew a real close button and one
    /// silently didn't, because no single place guaranteed every kind got the same treatment.
    /// `tooltip` is `Some` only for [`Self::render_file_tab`]'s own two-gesture "close without
    /// saving?" cue (GitHub issue #26); every other kind passes `None`.
    pub(crate) fn render_tab_close_button(
        &self,
        id: impl Into<gpui::ElementId>,
        close_color: theme::ColorToken,
        tooltip: Option<&'static str>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id.into())
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
            .when_some(tooltip, |el, text| el.tooltip(text_tooltip(text)))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                on_click(this, window, cx);
            }))
    }

    /// The shared tab "chrome" every `render_*_tab` wraps its own content in (GitHub issue
    /// #103): the border, active/inactive background and underline (`work_surface::tab_colors`),
    /// the opacity dim while this tab's own drag ghost is the real thing following the cursor,
    /// the full `on_drag`/`on_drag_move`/`on_drop` wiring (GitHub issue #16's unified
    /// drag-to-reorder system - see [`DraggedTab`]'s own docs), the insertion caret, middle-click
    /// close, and the drop settle-fade (GitHub issue #16 §5). Every real per-kind visual (chip,
    /// label, dirty/status dot, the close button itself) is still supplied by the caller as
    /// `args.content`, in the order it should render - only the chrome around it is shared now,
    /// which is what makes the exact bug class GitHub issue #96 was structurally impossible:
    /// there is now exactly one place that wires drag/close/settle-fade for every tab kind, not
    /// three.
    pub(crate) fn render_tab_chrome(
        &self,
        args: TabChromeArgs,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let TabChromeArgs {
            outer_id,
            hit_id,
            tab_ref,
            drag_value,
            is_active,
            content,
            on_middle_click,
            on_activate,
            debug_selector,
        } = args;
        let colors = work_surface::tab_colors(is_active);
        let insertion_caret = match &self.tab_drag_insertion {
            Some((target, insert_after)) if *target == tab_ref => Some(*insert_after),
            _ => None,
        };
        let is_dragging = self.dragging_tab.as_ref() == Some(&tab_ref);
        let settle_animation_id = tab_settle_animation_id(&self.dropped_tab_settle, &tab_ref);
        let this_entity = cx.entity();
        let tab_ref_for_drag = tab_ref.clone();
        let tab_ref_for_drag_move = tab_ref.clone();
        let tab_ref_for_drop = tab_ref;

        let tab_div = div()
            .id(outer_id)
            .when_some(debug_selector, |el, selector| {
                el.debug_selector(move || selector.to_string())
            })
            .relative()
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            .when(is_dragging, |el| el.opacity(0.4))
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    on_middle_click(this, window, cx);
                }),
            )
            .on_drag(drag_value, move |dragged, _position, _window, cx| {
                this_entity.update(cx, |this, cx| {
                    this.start_dragging_tab(tab_ref_for_drag.clone(), cx);
                });
                cx.new(|_| dragged.clone())
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedTab>, _window, cx| {
                    this.update_tab_drag_insertion(&tab_ref_for_drag_move, event, cx);
                },
            ))
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _window, cx| {
                this.drop_dragged_tab(dragged.tab_ref(), tab_ref_for_drop.clone(), cx);
            }))
            .when_some(insertion_caret, |el, insert_after| {
                el.child(render_tab_insertion_caret(insert_after))
            })
            .child(
                div()
                    .id(hit_id)
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(13.0))
                    .cursor_pointer()
                    // GitHub issue #128: only a tab's own `×` close glyph gave any hover feedback
                    // - the much larger click target that actually activates the tab gave none.
                    // Skipped for the already-active tab: it already reads as selected via
                    // `colors.bg` (`theme::surface::CENTER`) on the outer tab div above, and
                    // layering a second bg here would just muddy that. Fixed once, here, in the
                    // shared chrome every tab kind (file, agent, graph) renders through - not
                    // per call site.
                    .when(!is_active, |el| hover_bg(el, theme::surface::ROW_HOVER))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        on_activate(this, window, cx);
                    }))
                    .children(content),
            )
            .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline));

        // A real drop's own settle-in fade (GitHub issue #16's "dropping animates the tab
        // settling into its slot") - see `tab_settle_animation_id`'s own docs for why a fresh id
        // is required, and why this branches to `gpui::AnyElement` rather than a plain
        // `.when_some` (`gpui::AnimationExt::with_animation` returns a different wrapper type,
        // not `Self`).
        match settle_animation_id {
            Some(id) => tab_div
                .with_animation(
                    id,
                    Animation::new(TAB_SETTLE_ANIMATION_DURATION),
                    |el, delta| el.opacity(0.55 + 0.45 * delta),
                )
                .into_any_element(),
            None => tab_div.into_any_element(),
        }
    }

    /// A file tab: language chip (`file_tree::lang_chip_for_name`, dimmed via
    /// `work_surface::file_tab_chip_colors` when inactive), file name, and a close hit box.
    /// Clicking the body activates the tab ([`Self::activate_file_tab`]); clicking `×`, middle-
    /// clicking anywhere on the tab, or the global `Ctrl+W` (GitHub issue #26) all close it via
    /// [`crate::code_surface::tabs::AdeApp::request_close_file_tab`] (never [`Self::close_file_tab`]
    /// directly - see that method's own docs for the real unsaved-changes confirmation this keeps
    /// every close gesture honest about), stopping propagation so a close never also activates
    /// (the same pattern [`render_agent_tab`]'s close button uses). Shares active/inactive bg/
    /// underline/label colours with agent tabs (`work_surface::tab_colors`).
    pub(in crate::work_surface) fn render_file_tab(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let is_active = self.open_change.as_deref() == Some(path);
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let lang = file_tree::lang_chip_for_name(&file_name);
        let chip_colors = work_surface::file_tab_chip_colors(lang, is_active);
        let colors = work_surface::tab_colors(is_active);
        // `Self::close_tab_confirm_armed` (GitHub issue #26): a real, visible cue - not just an
        // internal flag - that this specific dirty tab is one more close gesture away from really
        // closing without saving, matching `Self::prune_status`'s own "the confirmation state is
        // always on screen, never silent" precedent for this app's other two-gesture confirmations.
        let is_close_armed = self.close_tab_confirm_armed.as_deref() == Some(path);
        let close_color = if is_close_armed {
            theme::button::DANGER_FG
        } else if is_active {
            theme::text::DIMMER
        } else {
            theme::text::DISABLED
        };
        let activate_path = path.to_path_buf();
        let close_path = activate_path.clone();
        let middle_click_path = activate_path.clone();
        let key = path.display().to_string();
        // Real dirty-state indicator (Revision R8.5a): a small dot, shown only while this tab's
        // real `EditBuffer` genuinely has unsaved edits (`EditBuffer::is_dirty`) - `false` for a
        // tab with no buffer yet (still loading, or a truncated/read-only file - see
        // `AdeApp::edit_buffers`' own docs), never a fabricated placeholder.
        let is_dirty = self
            .edit_buffer(path)
            .is_some_and(|buffer| buffer.is_dirty());
        let tab_ref = work_surface::TabRef::File(path.to_path_buf());
        let drag_value = DraggedTab::File {
            path: path.to_path_buf(),
            label: file_name.clone(),
        };

        let close_button = self.render_tab_close_button(
            format!("close-file-tab-{key}"),
            close_color,
            // Real, visible confirmation cue (GitHub issue #26) - see `close_color`'s own docs
            // above for why `is_close_armed` never leaves this a silent internal-only flag.
            is_close_armed.then_some("Unsaved changes - click × again to close without saving"),
            move |this, window, cx| {
                this.request_close_file_tab(close_path.clone(), window, cx);
            },
            cx,
        );
        let mut content: Vec<gpui::AnyElement> = vec![
            div()
                .flex_none()
                .w(px(14.0))
                .h(px(14.0))
                .rounded(theme::radius::CHIP)
                .flex()
                .items_center()
                .justify_center()
                .bg(chip_colors.bg)
                .font(font(theme::font::MONO))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(7.0))
                .text_color(chip_colors.fg)
                .child(lang.label)
                .into_any_element(),
            div()
                .font(font(theme::font::MONO))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(self.ui_text_size(11.0))
                .text_color(colors.label)
                .child(file_name)
                .into_any_element(),
        ];
        if is_dirty {
            content.push(
                div()
                    .id(format!("file-tab-dirty-{key}"))
                    .flex_none()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::status::ASK)
                    .into_any_element(),
            );
        }
        content.push(close_button.into_any_element());

        self.render_tab_chrome(
            TabChromeArgs {
                outer_id: format!("file-tab-{key}").into(),
                hit_id: format!("file-tab-hit-{key}").into(),
                tab_ref,
                drag_value,
                is_active,
                content,
                // Middle-click closes any file tab outright (GitHub issue #26), same real
                // `request_close_file_tab` entry point as `×`/`Ctrl+W` - so a dirty tab still
                // gets the real unsaved-changes confirmation rather than a middle-click silently
                // bypassing it.
                on_middle_click: Box::new(move |this, window, cx| {
                    this.request_close_file_tab(middle_click_path.clone(), window, cx);
                }),
                on_activate: Box::new(move |this, window, cx| {
                    this.activate_file_tab(activate_path.clone(), window, cx);
                }),
                debug_selector: None,
            },
            cx,
        )
    }

    /// The tab strip's `+` menu button - toggles [`Self::plus_menu_open`] (unconditionally
    /// spawning a shell is the rail's separate `+` -
    /// [`crate::rail::render::render_new_agent_button`]). A `gpui::canvas` child captures
    /// this button's painted bounds into [`Self::plus_button_bounds`] every render, which
    /// [`Self::render_plus_menu`] positions the popover off of. Opening the menu also refreshes
    /// [`Self::load_agent_rows`], so the "New agent" row's icon/chip
    /// ([`Self::resolved_new_agent_kind`]) reflects a reasonably fresh `$PATH` search rather than
    /// a possibly-empty cached snapshot.
    pub(in crate::work_surface) fn render_tab_strip_plus(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_open = self.plus_menu_open;

        div()
            .id("tab-strip-new")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .cursor_pointer()
            .bg(if is_open {
                theme::surface::SEGMENT_TRACK.into()
            } else {
                work_surface::TRANSPARENT
            })
            .hover(|el| el.bg(theme::surface::SEGMENT_TRACK))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(13.0))
                    .text_color(theme::text::GHOST)
                    .child("+"),
            )
            .child({
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.plus_button_bounds = bounds;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.plus_menu_open = !this.plus_menu_open;
                // This is the tab strip's own `+`, not a rail repo header's - see
                // `Self::plus_menu_repo_anchor`'s own docs for why `Self::render_plus_menu` needs
                // to know which one opened it.
                this.plus_menu_repo_anchor = None;
                if this.plus_menu_open {
                    this.load_agent_rows(cx);
                }
                cx.notify();
            }))
    }

    /// The tab strip's `+` menu popover: an absolutely-positioned scrim + panel, the same overlay
    /// shape [`Self::render_palette`] uses (transparent, not dimmed - the design has no
    /// full-window dimming for this smaller popover). The scrim's `on_click` closes the menu;
    /// the panel stops that click from bubbling up (`cx.stop_propagation()`). Positioned off
    /// [`Self::plus_button_bounds`].
    ///
    /// Five rows, in the exact order and wording Revision R12 §3 specifies: *New terminal*
    /// ([`Self::new_agent`] with [`AgentKind::Shell`]), *New agent* (`runs in <branch>` -
    /// [`Self::new_agent_pane`]), *Git graph* ([`Self::open_git_graph`]), *Open file…*
    /// ([`Self::open_palette`], scoped to [`palette::PaletteScope::Files`]), and *Next changed
    /// file* ([`Self::next_changed_file`]). *New terminal*, *New agent*, *Git graph*, and *Next
    /// changed file* each dispatch the same method their own global keybinding does
    /// (`crate::default_key_bindings`) and show that binding's keycap; *Open file…* has no global
    /// keybinding of its own (its own real `mod+P` spec is now claimed by `TogglePalette`
    /// instead, see `crate::default_key_bindings`'s own docs for that tradeoff) and so shows no
    /// keycap. Every row's click handler also closes the menu.
    ///
    /// There is no *New file* row - §3's item list names only these five. A worktree's file tree
    /// (`crate::sidebar::render::render_file_tree_row`/`render_right_sidebar_toggle`) already
    /// carries its own always-visible `+` affordances (per-directory and root-level), both wired
    /// to the same real [`Self::start_new_file`] this row used to call, so removing it here drops
    /// no reachable functionality - it was a second entry point to an action that already has one
    /// the spec keeps.
    pub(crate) fn render_plus_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let macos = self.window_controls_style().is_macos();
        // See `Self::plus_menu_repo_anchor`'s own docs: a rail repo header's own `+` positions
        // this popover off that header's own painted bounds, not the tab strip's - falling back
        // to the tab strip's bounds if that repo's header hasn't painted this popover's anchor
        // yet (defensive; not a real reachable path through this app's own UI).
        let bounds = match self.plus_menu_repo_anchor {
            Some(repo_id) => self
                .rail_plus_button_bounds
                .get(&repo_id)
                .copied()
                .unwrap_or(self.plus_button_bounds),
            None => self.plus_button_bounds,
        };

        let resolved_kind = self.resolved_new_agent_kind();
        let (agent_fg, agent_bg) = work_surface::agent_tint(resolved_kind);
        let agent_initial = work_surface::agent_initial(resolved_kind);
        let branch = self.current_worktree_branch();
        let new_agent_secondary = work_surface::new_agent_menu_secondary_text(branch.as_deref());
        let changed_count = self
            .current_diff()
            .map(|diff| diff.files.len())
            .unwrap_or(0);

        div()
            .id("plus-menu-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.plus_menu_open = false;
                cx.notify();
            }))
            .child(
                menu_popover_chrome(
                    div()
                        .id("plus-menu-popover")
                        .absolute()
                        .left(bounds.origin.x + px(2.0))
                        .top(bounds.origin.y + bounds.size.height)
                        .w(theme::zone::PLUS_MENU_WIDTH)
                        .py(px(4.0)),
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .child(
                    render_dropdown_menu_row(
                        "\u{276f}",
                        theme::text::DIM.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "New terminal",
                        "in this worktree".to_string(),
                        keymap::resolve_combo("ctrl+shift+T", macos),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, window, cx| {
                            this.new_agent(AgentKind::Shell, window, cx);
                            this.plus_menu_open = false;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        agent_initial,
                        agent_fg,
                        agent_bg,
                        "New agent",
                        new_agent_secondary,
                        keymap::resolve_combo("mod+shift+N", macos),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, _window, cx| {
                            this.new_agent_pane(cx);
                            this.plus_menu_open = false;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        "\u{2325}",
                        theme::graph::TAB_CHIP_FG.into(),
                        theme::graph::TAB_CHIP_BG.into(),
                        "Git graph",
                        "commit history".to_string(),
                        keymap::resolve_combo("mod+shift+G", macos),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, window, cx| {
                            this.plus_menu_open = false;
                            this.open_git_graph(window, cx);
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        "@",
                        theme::palette::COMMAND_CHIP.0.into(),
                        theme::palette::COMMAND_CHIP.1.into(),
                        "Open file\u{2026}",
                        "search this worktree".to_string(),
                        // No keycap: this row has no global keybinding (see the function
                        // docs above), and `render_keycap_row` renders nothing for `&[]`.
                        Vec::new(),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, window, cx| {
                            this.plus_menu_open = false;
                            this.open_palette(window, cx);
                            // `open_palette` always resets `palette_scope` to
                            // `PaletteScope::default()`, so this must be set after it
                            // returns, not before.
                            this.palette_scope = palette::PaletteScope::Files;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    render_dropdown_menu_row(
                        "]",
                        theme::text::DIM.into(),
                        theme::surface::CHIP_NEUTRAL.into(),
                        "Next changed file",
                        format!("{changed_count} changed"),
                        keymap::resolve_combo("]", macos),
                        true,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, window, cx| {
                            this.plus_menu_open = false;
                            this.next_changed_file(window, cx);
                        },
                    )),
                ),
            )
    }

    /// Which agent kind the `+` menu's "New agent" row would spawn right now: the first
    /// [`settings::AGENT_KINDS`] entry [`Self::agent_rows`] (refreshed on menu open) confirms is
    /// installed, or `AGENT_KINDS[0]` if none are (or `agent_rows` hasn't been populated yet).
    /// Display-only - [`Self::new_agent_pane`] runs its own detection independently, off the
    /// foreground thread, at the moment it actually spawns.
    pub(crate) fn resolved_new_agent_kind(&self) -> AgentKind {
        settings::AGENT_KINDS
            .into_iter()
            .find(|kind| {
                self.agent_rows
                    .iter()
                    .any(|row| row.kind == *kind && row.is_ready())
            })
            .unwrap_or(settings::AGENT_KINDS[0])
    }

    /// The `+` menu's "New agent" action (`secondary-shift-n`) - spawns the first
    /// [`settings::AGENT_KINDS`] entry a background `$PATH` search
    /// (`pty_core::resolve_on_path`, the same search [`Self::load_agent_rows`] runs) confirms is
    /// installed, rather than blocking the click on a filesystem walk.
    ///
    /// If no configured agent is installed, this spawns `AGENT_KINDS[0]` anyway, same as the
    /// agent toolbar's `+ claude`/`+ codex` buttons when that binary isn't on `$PATH`: the
    /// process fails to spawn and a non-panicking spawn error shows in the new tab
    /// (`TerminalPane::spawn_error`).
    pub(in crate::work_surface) fn new_agent_pane(&mut self, cx: &mut Context<Self>) {
        // GitHub issue #90: the same real "nothing to spawn into yet" guard [`Self::new_agent`]'s
        // own docs explain - see those for the concrete bug this closes.
        if self.focused_repo().is_none() {
            return;
        }
        let cwd = self.active_agent_cwd();
        let task = cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move {
                    settings::AGENT_KINDS.into_iter().find(|kind| {
                        kind.agent_binary_name()
                            .and_then(pty_core::resolve_on_path)
                            .is_some()
                    })
                })
                .await;
            // Needs `Window` access to move focus onto the newly spawned agent's pane
            // (`Self::focus_newly_spawned_agent`) - `Entity::update_in` provides it.
            let _ = this.update_in(cx, |this, window, cx| {
                let kind = installed.unwrap_or(settings::AGENT_KINDS[0]);
                this.agents.spawn(
                    kind,
                    cwd,
                    this.settings.appearance.terminal_font_size,
                    window,
                    cx,
                );
                this.focus_newly_spawned_agent(window, cx);
                this.prune_confirm_armed = false;
                cx.notify();
            });
        });
        // A `TaskPool`, not a single `Option` slot: two rapid clicks before the first click's
        // `$PATH` search resolves must not drop (and so cancel, per GPUI's "dropping a `Task`
        // cancels it" semantics) the first click's task when the second is assigned.
        self._new_agent_pane_task.push(task);
    }

    /// The `+` menu's "Next changed file" action (`]`) - opens the next changed file after the
    /// active file tab as a tab, wrapping around to the first once the last is passed (so a
    /// repeated `]` press cycles indefinitely, matching how the agent-jump keycaps and palette
    /// arrow keys already treat "next"/"previous"). If the active file isn't itself a changed
    /// file, or nothing is active, this opens the first changed file. No-op if there's no loaded
    /// diff, or it has no changed files.
    pub(crate) fn next_changed_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next_path = {
            let Some(diff) = self.current_diff() else {
                return;
            };
            if diff.files.is_empty() {
                return;
            }
            let current_index = self
                .open_change
                .as_ref()
                .and_then(|active| diff.files.iter().position(|file| &file.path == active));
            let next_index = match current_index {
                Some(index) => (index + 1) % diff.files.len(),
                None => 0,
            };
            diff.files[next_index].path.clone()
        };
        self.open_change_diff(next_path, window, cx);
    }

    /// The tab strip's agent-jump keycaps (`secondary-1`..`secondary-8`) - jumps to the
    /// agent at 1-indexed `position` in the same order [`Self::render_tab_strip`] iterates
    /// (`Self::current_worktree_agents`), via [`Self::select_agent`]. No-op if fewer than
    /// `position` agents are currently open in the selected worktree.
    pub(crate) fn jump_to_agent_at(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = position
            .checked_sub(1)
            .and_then(|index| self.current_worktree_agents().nth(index))
            .map(|agent| agent.id)
        else {
            return;
        };
        self.select_agent(id, window, cx);
    }

    /// The Windows/Linux title bar's Agent menu "Next agent"/"Previous agent" rows
    /// (`crate::title_bar::menu::AdeApp::render_title_menu`) - `delta` is `1`/`-1`. Cycles
    /// through [`Self::current_worktree_agents`] in the same order
    /// [`Self::jump_to_agent_at`] indexes - **never** every agent across every worktree
    /// (a real, live-reproduced bug found in this revision's own self-audit: an earlier version
    /// cycled `self.agents` directly, so "Next Agent" could jump to a *different* worktree's
    /// agent, which [`Self::select_agent`] then silently promotes into a full
    /// [`Self::select_worktree`] switch - landing the user on the wrong worktree entirely (a menu
    /// row labeled "cycle tabs" must never have that side effect), including its `edit_buffers`
    /// entries, which are real, live per-worktree state (see that field's own docs) rather than
    /// something a switch discards - wrapping around both ends (mirroring
    /// [`Self::next_changed_file`]'s own cyclic-index convention for "next" over an existing
    /// ordered list), via the same real [`Self::select_agent`] every tab-strip click and jump
    /// keycap already goes through - no separate "next agent" subsystem, just a cyclic index
    /// over the existing per-worktree list. No-op with fewer than two agents in the selected
    /// worktree (nothing to cycle to) or no active agent at all (both real, reachable states -
    /// the latter only while every agent has been closed).
    pub(crate) fn select_relative_agent(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<AgentId> = self.current_worktree_agents().map(|s| s.id).collect();
        if ids.len() < 2 {
            return;
        }
        let Some(active_id) = self.agents.active_id() else {
            return;
        };
        let Some(current_index) = ids.iter().position(|id| *id == active_id) else {
            return;
        };
        let len = ids.len() as isize;
        let next_index = (current_index as isize + delta).rem_euclid(len) as usize;
        self.select_agent(ids[next_index], window, cx);
    }

    /// [`NewTerminal`]'s `ctrl-shift-T` action handler - the `+` menu's "New terminal" row's own
    /// keybinding, spawning a [`AgentKind::Shell`] agent like the row's click handler does.
    pub(crate) fn handle_new_terminal_action(
        &mut self,
        _action: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent(AgentKind::Shell, window, cx);
    }

    /// [`NewAgentPane`]'s `secondary-shift-n` action handler - see [`Self::new_agent_pane`].
    pub(crate) fn handle_new_agent_pane_action(
        &mut self,
        _action: &NewAgentPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_pane(cx);
    }

    /// [`NextChangedFile`]'s `]` action handler - see [`Self::next_changed_file`].
    pub(crate) fn handle_next_changed_file_action(
        &mut self,
        _action: &NextChangedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.next_changed_file(window, cx);
    }

    agent_jump_action_handler!(handle_jump_to_agent_1_action, JumpToAgent1, 1);
    agent_jump_action_handler!(handle_jump_to_agent_2_action, JumpToAgent2, 2);
    agent_jump_action_handler!(handle_jump_to_agent_3_action, JumpToAgent3, 3);
    agent_jump_action_handler!(handle_jump_to_agent_4_action, JumpToAgent4, 4);
    agent_jump_action_handler!(handle_jump_to_agent_5_action, JumpToAgent5, 5);
    agent_jump_action_handler!(handle_jump_to_agent_6_action, JumpToAgent6, 6);
    agent_jump_action_handler!(handle_jump_to_agent_7_action, JumpToAgent7, 7);
    agent_jump_action_handler!(handle_jump_to_agent_8_action, JumpToAgent8, 8);

    /// One tab: a 14×14 kind chip, `label` (already resolved and, if it repeats within this
    /// worktree, ordinal-disambiguated by the caller - `Self::render_tab_strip`'s own docs on why
    /// that has to happen before this per-tab call, not inside it), and a `×` that closes it
    /// (`Agents::close`, tearing down the process). Split into a `flex_1` clickable content row
    /// plus a `flex_none` 1px underline bar, rather than a single div with two
    /// differently-coloured borders, because GPUI's `Style::border_color` is one colour for every
    /// edge (`vendor/zed/crates/gpui/src/style.rs`) - it can't give the right border (always
    /// `theme::border::INNER`) and the active/inactive-dependent underline two different colours
    /// on the same div.
    pub(in crate::work_surface) fn render_agent_tab(
        &self,
        agent: &Agent,
        label: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = agent.id;
        let is_active = self.agents.active_id() == Some(id);
        let chip_kind = work_surface::tab_chip_kind(agent.kind);
        let is_mono = matches!(chip_kind, work_surface::TabChipKind::Cli);
        let colors = work_surface::tab_colors(is_active);
        // §3: "5px status square that keeps reporting while you read another tab" - the same
        // real `Status` the rail's own agent row and context bar already derive this agent's
        // colour from, so the tab strip can never disagree with either about what state an
        // agent is in.
        let status_color: gpui::Rgba = self.agent_status(agent, cx).color();
        let close_color = if is_active {
            theme::text::DIMMER
        } else {
            theme::text::DISABLED
        };
        let tab_ref = work_surface::TabRef::Agent(id);
        let drag_value = DraggedTab::Agent {
            id,
            label: label.clone(),
        };

        let close_button = self.render_tab_close_button(
            ("close-agent-tab", id),
            close_color,
            None,
            move |this, window, cx| {
                this.close_agent(id, window, cx);
            },
            cx,
        );
        let content: Vec<gpui::AnyElement> = vec![
            render_tab_chip(agent.kind, is_active).into_any_element(),
            div()
                .font(font(if is_mono {
                    theme::font::MONO
                } else {
                    theme::font::SANS
                }))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(self.ui_text_size(if is_mono { 11.0 } else { 11.5 }))
                .text_color(colors.label)
                .child(label)
                .into_any_element(),
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded(px(2.5))
                .bg(status_color)
                .into_any_element(),
            close_button.into_any_element(),
        ];

        self.render_tab_chrome(
            TabChromeArgs {
                outer_id: ("agent-tab", id).into(),
                hit_id: ("agent-tab-hit", id).into(),
                tab_ref,
                drag_value,
                is_active,
                content,
                // Middle-click closes any agent/terminal tab too (GitHub issue #26) - the same
                // `Self::close_agent` real teardown (`TerminalPane::shutdown`'s SIGHUP/grace/
                // SIGKILL - see that method's own docs) every other close path already uses.
                on_middle_click: Box::new(move |this, window, cx| {
                    this.close_agent(id, window, cx);
                    cx.notify();
                }),
                on_activate: Box::new(move |this, window, cx| {
                    this.select_agent(id, window, cx);
                }),
                debug_selector: None,
            },
            cx,
        )
    }

    /// The agent context bar: agent badge/name, a divider, branch, the worktree path (the one
    /// flexible, ellipsising child - every other child is `flex_none` and non-wrapping, so the
    /// bar never wraps when the centre narrows), a status pill, and `Merge`/`Archive` - or, for a
    /// bare worktree (Revision R12 §3: [`Self::current_worktree_is_bare`]), the agent name greyed
    /// to `no agent` and a single blue `Start an agent` button in `Merge`/`Archive`'s place.
    pub(in crate::work_surface) fn render_agent_context_bar(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.agent_status(agent, cx);
        let (agent_fg, agent_bg) = work_surface::agent_tint(agent.kind);
        let agent_initial = work_surface::agent_initial(agent.kind);
        let is_bare = self.current_worktree_is_bare();
        // `AgentKind` only tracks which CLI binary is running, not which model it's
        // configured to use, so `agent.kind.label()` ("Claude"/"Codex"/"Shell") is the
        // closest honest substitute for a model name this app never actually observes. A bare
        // worktree (no real agent at all - `is_bare` is only ever true while `agent.kind`
        // really is `Shell`, since that's the only tab a bare worktree can be showing) reads
        // `no agent` instead, greyed to `theme::text::FAINT` rather than the normal `MUTED`.
        let agent_label = if is_bare {
            "no agent"
        } else {
            agent.kind.label()
        };
        let agent_label_color = if is_bare {
            theme::text::FAINT
        } else {
            theme::text::MUTED
        };
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == agent.cwd)
            .and_then(|item| item.branch.clone());
        let worktree_path = agent.cwd.display().to_string();
        let id = agent.id;

        let bar = div()
            .id("agent-context-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::band::CONTEXT_BAR)
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .w(px(15.0))
                    .h(px(15.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(agent_bg)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(8.5))
                    .text_color(agent_fg)
                    .child(agent_initial),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(agent_label_color)
                    .child(agent_label),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(13.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::DIM)
                    .child(branch.unwrap_or_else(|| "(detached)".to_string())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::PATH)
                    .child(worktree_path),
            )
            .child(render_status_pill(status_value));

        if is_bare {
            bar.child(self.render_start_agent_button(cx))
        } else {
            bar.child(self.render_merge_button(id, cx))
                .child(self.render_archive_button(id, cx))
        }
    }

    /// A bare worktree's context bar action (Revision R12 §3): a filled blue `Start an agent`
    /// button with a `mod+shift+N` keycap hint, replacing `Merge`/`Archive` outright rather than
    /// sitting alongside them - neither has anything to act on yet in a worktree with no agent.
    /// Real, not decorative: dispatches [`Self::new_agent_pane`], the same entry point the tab
    /// strip's `+` menu row and its own global `secondary-shift-n` keybinding already use.
    pub(in crate::work_surface) fn render_start_agent_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = work_surface::action_button_colors(work_surface::ActionStyle::PrimaryBlue);
        let macos = self.window_controls_style().is_macos();
        let parts = keymap::resolve_combo("mod+shift+N", macos);

        div()
            .id("context-bar-start-agent")
            .flex_none()
            .cursor_pointer()
            .h(px(20.0))
            .px(px(8.0))
            .gap(px(6.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            .border_color(colors.border)
            .bg(colors.bg)
            .flex()
            .items_center()
            .hover(|el| el.bg(theme::button::BLUE_BG_HOVER))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.5))
                    .text_color(colors.fg)
                    .child("Start an agent"),
            )
            .child(render_action_keycap_row(
                &parts,
                colors.keycap_fg,
                colors.keycap_border,
            ))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.new_agent_pane(cx);
            }))
    }

    /// The context bar's `Merge` button - starts [`Self::start_merge`]. Disabled (dimmed,
    /// non-interactive) whenever any merge flow is already active, own agent or not (only one
    /// runs at a time - see [`Self::start_merge`]'s docs), and shows `Merging…` in place of
    /// `Merge` while this agent's own attempt is the one running.
    pub(in crate::work_surface) fn render_merge_button(
        &self,
        id: AgentId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_for_this_agent = self
            .merge_flow
            .as_ref()
            .is_some_and(|flow| flow.agent_id == id);
        let running = active_for_this_agent
            && matches!(
                self.merge_flow.as_ref().map(|flow| &flow.state),
                Some(merge::MergeFlowState::Running)
            );
        let disabled = self.merge_flow.is_some();
        let label = if running { "Merging\u{2026}" } else { "Merge" };

        let base = div()
            .id(("context-bar-merge", id))
            .flex_none()
            .h(px(20.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.5))
            .child(label);

        if disabled {
            base.cursor_default()
                .border_color(theme::border::BUTTON_DISABLED)
                .text_color(theme::text::GHOSTER)
        } else {
            base.cursor_pointer()
                .border_color(theme::border::BUTTON)
                .text_color(theme::text::SECONDARY)
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.start_merge(id, cx);
                }))
        }
    }

    /// The context bar's `Archive` button - see [`Self::archive_agent`].
    pub(in crate::work_surface) fn render_archive_button(
        &self,
        id: AgentId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(("context-bar-archive", id))
            .flex_none()
            .cursor_pointer()
            .h(px(20.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.5))
            .text_color(theme::text::FAINT)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child("Archive")
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.archive_agent(id, window, cx);
            }))
    }

    /// Surface A/B's shared header: the resolved program label (this app has no saved-agent
    /// resumability, so there's no resume argument to show alongside it), a `Shell` agent's
    /// cwd, and a `mod + click a path to open it` hint, rendered for every agent kind -
    /// `TerminalPane` behaves identically for shell and agents (see its module docs), so
    /// link-click is exactly as real for a `Claude`/`Codex` panic frame as for a shell prompt.
    ///
    /// A `Shell` label gets a ` · wsl` suffix when running inside WSL (`crate::env_info::is_wsl`).
    /// The design's third hint, `split`, is deliberately not rendered - this app has no
    /// pane-splitting feature, and this codebase omits hints for features that don't exist
    /// rather than showing a decorative keycap for one (the same precedent
    /// [`Self::render_plus_menu`]'s "Open file…" row sets).
    ///
    /// A `Claude`/`Codex` agent's pid is shown once, in the info footer below
    /// ([`Self::render_pty_info_footer`]) - not duplicated here. GitHub issue #20 moved `clear`
    /// into that same info footer, alongside pid/grid-dims/env - see that method's own docs for
    /// the click entry point, and [`Self::handle_terminal_clear_action`] for the real, rebindable
    /// keybinding that now sits behind it too.
    pub(in crate::work_surface) fn render_pty_header(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = agent.pane.read(cx);
        let program_label = pane.program_label();
        let is_running = pane.is_running();
        let exit_code = pane.exit_status().map(|status| status.exit_code());
        let status_value = self.agent_status(agent, cx);
        let state_label = work_surface::pty_state_label(is_running, status_value, exit_code);
        let is_wsl_shell = agent.kind == AgentKind::Shell && env_info::is_wsl();
        let label_text = if is_wsl_shell {
            format!("{program_label} \u{b7} wsl")
        } else {
            program_label
        };

        let header = div()
            .id("pty-header")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(9.0))
            .px(px(12.0))
            .h(theme::band::PTY_HEADER)
            .bg(theme::surface::FOOTER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::DIM)
                    .child(label_text),
            );

        let header = match agent.kind {
            AgentKind::Shell => header.child(
                div()
                    .flex_none()
                    .max_w(px(280.0))
                    .overflow_hidden()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child(agent.cwd.display().to_string()),
            ),
            // No per-kind header content for an agent - pid is shown once, in the info
            // footer below.
            AgentKind::Claude | AgentKind::Codex => header,
        };

        let macos = self.window_controls_style().is_macos();
        let header = header.child(div().flex_1()).child(
            div()
                .id("pty-header-hints")
                .flex()
                .items_center()
                .gap(px(11.0))
                .child(render_hint_pair(
                    &keymap::resolve_combo("mod", macos),
                    "click a path to open it",
                )),
        );

        header.child(
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::HINT)
                .child(state_label),
        )
    }

    /// The terminal pane's info footer: pid, grid dimensions, the environment chip, `clear`
    /// (GitHub issue #20 - moved here from the header, see [`Self::render_pty_header`]'s own
    /// docs), and a hint about file:line references. Rendered for every agent kind -
    /// `TerminalPane` is the same component behind a `Shell` tab and a `Claude`/`Codex` tab (see
    /// that module's docs), so pid, grid dimensions, and `clear` are equally meaningful for
    /// either. Distinct from, and rendered alongside, [`Self::render_pty_footer`] - the
    /// agent-level Interrupt/Retry/Archive action footer.
    pub(in crate::work_surface) fn render_pty_info_footer(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = agent.pane.read(cx);
        let pid = pane.pid();
        let (cols, rows) = pane.grid_dimensions();

        let divider = || {
            div()
                .flex_none()
                .w(px(1.0))
                .h(px(11.0))
                .bg(theme::border::DIVIDER)
        };
        let mono_text = |text: String| {
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::PATH)
                .child(text)
        };

        let mut footer = div()
            .id("pty-info-footer")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::band::PTY_INFO_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER);

        if let Some(pid) = pid {
            footer = footer
                .child(mono_text(format!("pid {pid}")))
                .child(divider());
        }
        let macos = self.window_controls_style().is_macos();
        let clear_combo =
            keymap::resolve_combo(if macos { "mod+K" } else { "ctrl+shift+L" }, macos);
        let pane_entity = agent.pane.clone();
        footer = footer
            .child(mono_text(format!("{cols}\u{d7}{rows}")))
            .child(divider())
            .child(render_env_chip())
            .child(divider())
            .child(
                div()
                    .id("pty-info-footer-clear")
                    .cursor_pointer()
                    .rounded(theme::radius::CHIP)
                    .px(px(3.0))
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child(render_hint_pair(&clear_combo, "clear"))
                    .on_click(cx.listener(move |_this, _event: &ClickEvent, _window, cx| {
                        pane_entity.update(cx, |pane, cx| pane.clear(cx));
                    })),
            );

        footer.child(div().flex_1()).child(
            div()
                .flex_none()
                .font(font(theme::font::SANS))
                .text_size(px(10.0))
                .text_color(theme::text::HINT)
                .child("file:line references open in a tab"),
        )
    }

    /// Surface A/B's shared footer: git-level actions appropriate to the agent's status - see
    /// `crate::work_surface::state::footer_actions`/[`Self::render_footer_action_button`] for which
    /// actions are implemented vs. disabled. No longer shows a `JERRY` wordmark (deliberate
    /// deviation from the design mockup, per direct user request - see this crate's `lib.rs`/
    /// `BUILD-LOG.md` for context, not a bug fix).
    pub(in crate::work_surface) fn render_pty_footer(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.agent_status(agent, cx);
        let is_running = agent.pane.read(cx).is_running();
        let actions = work_surface::footer_actions(status_value);
        let id = agent.id;
        let cwd = agent.cwd.clone();

        let mut footer = div()
            .id("pty-footer")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .px(px(12.0))
            .h(theme::band::SURFACE_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER);

        for action in actions {
            let mut enabled = action.implemented;
            // A live, merely-idle shell has nothing to "resume" (see
            // `crate::work_surface::state::ActionKind::Respawn`'s docs) - disable it in that case
            // rather than letting a click spawn a redundant duplicate agent.
            if action.kind == work_surface::ActionKind::Respawn
                && status_value == Status::Idle
                && is_running
            {
                enabled = false;
            }
            // `Keep all`/`Discard worktree` (Revision R10) share one in-flight guard
            // (`Self::worktree_history_op_in_flight`) - see `crate::worktree_history::flow`'s own
            // module docs for why one flag is enough discipline here. Disabled, not just
            // relabelled, while busy - mirrors `Self::render_rail_footer`'s own `prune_in_flight`
            // gating.
            let is_worktree_history_action = matches!(
                action.kind,
                work_surface::ActionKind::KeepAllChanges
                    | work_surface::ActionKind::DiscardWorktree
            );
            if is_worktree_history_action && self.worktree_history_op_in_flight.is_some() {
                enabled = false;
            }
            // Busy labels are keyed off the *specific* in-flight kind, not just "something is
            // running" - a real, live-reproduced bug an audit caught: keying this off the bare
            // in-flight flag alone made every visible `Discard worktree` button across every
            // agent read "discarding…" while an unrelated `Keep all` was running.
            let label = match action.kind {
                work_surface::ActionKind::DiscardWorktree
                    if self.worktree_history_op_in_flight
                        == Some(worktree_history::WorktreeHistoryOpKind::Discard) =>
                {
                    "discarding\u{2026}".to_string()
                }
                work_surface::ActionKind::DiscardWorktree
                    if self.discard_confirm_armed == Some(id) =>
                {
                    "confirm discard?".to_string()
                }
                work_surface::ActionKind::KeepAllChanges
                    if self.worktree_history_op_in_flight
                        == Some(worktree_history::WorktreeHistoryOpKind::Keep) =>
                {
                    "keeping\u{2026}".to_string()
                }
                _ => action.label.to_string(),
            };
            footer = footer.child(self.render_footer_action_button(
                id,
                cwd.clone(),
                action,
                label,
                enabled,
                cx,
            ));
        }

        footer.child(div().flex_1())
    }

    /// One footer action button - interactive (`cursor_pointer`, hover, `on_click` dispatch on
    /// `action.kind`) when `enabled`, otherwise dimmed with no cursor/hover/click at all - never
    /// a button that looks clickable but silently does nothing.
    pub(in crate::work_surface) fn render_footer_action_button(
        &self,
        id: AgentId,
        cwd: PathBuf,
        action: work_surface::FooterAction,
        label: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = work_surface::action_button_colors(action.style);
        let kind = action.kind;

        // Keyed off `kind`, not `label` - `label` now varies at render time (the confirm/busy
        // text swaps above), and an element's `id` should stay stable across that, matching
        // `Self::render_rail_footer`'s own static `"rail-prune"` id for its own label-swapping
        // button.
        let mut button = div()
            .id(format!("footer-action-{id}-{kind:?}"))
            .h(px(23.0))
            .px(px(10.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(if enabled {
                colors.bg
            } else {
                // A disabled action must never keep its full-colour fill - that would make an
                // inert button look as clickable as a real one (a disabled "Resume" was once
                // found rendering with a solid fill next to a working "Archive"). The design has
                // no separate disabled-background token, so falling back to `TRANSPARENT` lets
                // the footer's own background show through instead.
                work_surface::TRANSPARENT
            })
            .border_1()
            .border_color(if enabled {
                colors.border
            } else {
                theme::border::BUTTON_DISABLED.into()
            })
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        colors.fg
                    } else {
                        theme::text::GHOSTER.into()
                    })
                    .child(label),
            );

        if let Some(spec) = action.keycap {
            let (keycap_fg, keycap_border) = if enabled {
                (colors.keycap_fg, colors.keycap_border)
            } else {
                (
                    theme::text::GHOSTER.into(),
                    theme::border::BUTTON_DISABLED.into(),
                )
            };
            let parts = keymap::resolve_combo(spec, self.window_controls_style().is_macos());
            button = button.child(render_action_keycap_row(&parts, keycap_fg, keycap_border));
        }

        if enabled {
            button = button
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(
                    cx.listener(move |this, _event: &ClickEvent, window, cx| match kind {
                        work_surface::ActionKind::Interrupt => this.interrupt_agent(id, cx),
                        work_surface::ActionKind::OpenTerminal => {
                            this.open_companion_terminal(cwd.clone(), window, cx)
                        }
                        work_surface::ActionKind::Respawn => this.respawn_agent(id, window, cx),
                        work_surface::ActionKind::KeepAllChanges => this.keep_all_changes(id, cx),
                        work_surface::ActionKind::DiscardWorktree => {
                            this.request_discard_worktree(id, window, cx)
                        }
                        work_surface::ActionKind::Unimplemented => {}
                    }),
                );
        } else {
            button = button.cursor_default();
        }

        button
    }

    /// The centre pane's content: the unified tab strip ([`Self::render_tab_strip`]) always
    /// renders first, above either the active file tab's Surface C
    /// (`Self::render_code_surface`) if [`Self::open_change`] names one, or the active agent's
    /// toolbar/context-bar/pty otherwise.
    ///
    /// A file opened via a Changes-row click (`Self::open_change_diff`) always has a `DiffFile`
    /// to show; one opened via the Files tree (`Self::open_file_view`) may not, in which case
    /// `diff_file` below is `None` and `Self::render_code_surface` shows the File view
    /// unconditionally.
    ///
    /// The terminal-surface fallback's root div keeps `.min_w_0()`: GPUI's flexbox gives a flex
    /// item's minimum width its content's intrinsic width by default, so an unbroken wide
    /// terminal row could otherwise grow this pane past its `flex_1` share and push the
    /// fixed-width right sidebar off screen (`.min_w_0()` zeroes that automatic minimum - see
    /// `vendor/zed/crates/agent_ui/src/agent_panel.rs`'s own `.flex_1().min_w_0()` use for the
    /// same overflow guard).
    pub(crate) fn render_center_pane(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let surface = div()
            .id("work-surface")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme::surface::CENTER)
            .child(self.render_tab_strip(cx));

        if self.graph_tab_active {
            let body = self.render_graph_view(cx);
            return surface.child(body).into_any_element();
        }

        if let Some(open_path) = self.open_change.clone() {
            // `open_diff_file_cache` already holds the up-to-date `DiffFile` for `open_path`
            // (kept fresh by `Self::refresh_open_diff_file_cache`). Taking it out (an `O(1)`
            // pointer swap, not a clone) is required because `Self::render_code_surface` needs
            // `&mut self`, which a live `&DiffFile` borrow from `self` can't coexist with.
            let diff_file = self.open_diff_file_cache.take();
            let has_diff_or_file_view =
                diff_file.is_some() || self.code_view == code_view::CodeView::File;
            if has_diff_or_file_view {
                let body = self.render_code_surface(&open_path, diff_file.as_ref(), cx);
                self.open_diff_file_cache = diff_file;
                return surface.child(body).into_any_element();
            }
            self.open_diff_file_cache = diff_file;
        }

        match self.agents.active() {
            Some(agent) => {
                let body = if self
                    .merge_flow
                    .as_ref()
                    .is_some_and(|flow| flow.agent_id == agent.id)
                {
                    self.render_merge_flow_surface(agent, cx)
                } else {
                    div()
                        .id("pty-surface")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_hidden()
                        .bg(theme::surface::PTY)
                        .child(self.render_pty_header(agent, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .overflow_hidden()
                                .child(agent.pane.clone().into_any_element()),
                        )
                        .child(self.render_pty_info_footer(agent, cx))
                        .child(self.render_pty_footer(agent, cx))
                        .into_any_element()
                };
                surface
                    .child(self.render_agent_context_bar(agent, cx))
                    .child(body)
            }
            None => surface.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::FAINT)
                    .child(
                        "no agents open in this worktree - start one with the tab strip's + menu",
                    ),
            ),
        }
        .into_any_element()
    }
}

/// One row of the tab strip's `+` menu popover: chip, label, dim sub-label, and hint keycaps.
/// Returns the row with no click handler wired yet - [`Self::render_plus_menu`] attaches each
/// row's own action via `.on_click(cx.listener(...))` after the fact, since a free function has
/// no `Context<AdeApp>` to build a listener from. `keys` is already platform-resolved
/// (`crate::keymap::resolve_combo`'s output), rendered via [`render_keycap_row`] at
/// [`KeycapSize::Hint`].
/// One row in either the tab strip's `+` menu ([`AdeApp::render_plus_menu`]) or the Windows/
/// Linux title bar's real File/Edit/View/Agent/Help dropdowns
/// (`crate::title_bar::menu::AdeApp::render_title_menu`) - shared here since both are the same
/// "labeled trigger opens a small popover of real actions" pattern, first built for the `+`
/// menu and reused as-is (not re-derived) for the title-bar menus.
///
/// `enabled` mirrors [`AdeApp::render_footer_action_button`]'s own enabled/disabled convention:
/// `false` dims the chip/label/sub text and drops `cursor_pointer`/hover, so a row a click
/// through it right now would have no real effect on (e.g. "Cut" with no active selection) reads
/// as visibly inert rather than looking exactly as actionable as a working row. Callers must
/// also skip attaching `.on_click` when `enabled` is `false` - this function only controls the
/// row's *look*, never whether it's wired up, so a disabled row a caller forgot to gate would
/// still silently do nothing on click, exactly the "looks actionable, does nothing" bug class
/// this project's discipline forbids.
pub(crate) fn render_dropdown_menu_row(
    chip_glyph: &'static str,
    chip_fg: gpui::Rgba,
    chip_bg: gpui::Rgba,
    label: &'static str,
    sub: String,
    keys: Vec<String>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let (chip_fg, chip_bg, label_color): (gpui::Rgba, gpui::Rgba, gpui::Rgba) = if enabled {
        (chip_fg, chip_bg, theme::text::HEADING.into())
    } else {
        (
            theme::text::GHOSTER.into(),
            theme::surface::CHIP_NEUTRAL.into(),
            theme::text::GHOSTER.into(),
        )
    };
    let mut row = div()
        .id(format!("dropdown-menu-row-{label}"))
        .debug_selector(|| format!("dropdown-menu-row-{label}"))
        .flex()
        .items_center()
        .gap(px(9.0))
        .h(theme::band::PLUS_MENU_ROW)
        .px(px(10.0));
    row = if enabled {
        row.cursor_pointer()
            .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
    } else {
        row.cursor_default()
    };
    row.child(
        div()
            .flex_none()
            .w(px(14.0))
            .h(px(14.0))
            .rounded(theme::radius::CHIP)
            .flex()
            .items_center()
            .justify_center()
            .bg(chip_bg)
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(8.0))
            .text_color(chip_fg)
            .child(chip_glyph),
    )
    .child(
        div()
            .flex_none()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(11.5))
            .text_color(label_color)
            .child(label),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .truncate()
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::FAINTER)
            .child(sub),
    )
    .child(render_keycap_row(&keys, KeycapSize::Hint))
}

/// The tab strip's 14×14 kind chip - a `❯` glyph tinted with the agent's agent colour for
/// agent CLI tabs, or a pane glyph (a bar plus a prompt mark) for terminal tabs. Turns
/// `work_surface::tab_chip_kind`/`tab_chip_colors`'s mapping into GPUI elements; no
/// chip-selection logic lives here.
pub(in crate::work_surface) fn render_tab_chip(kind: AgentKind, active: bool) -> gpui::AnyElement {
    let colors = work_surface::tab_chip_colors(kind, active);
    let base = div()
        .flex_none()
        .w(px(14.0))
        .h(px(14.0))
        .rounded(theme::radius::CHIP)
        .bg(colors.bg);

    match work_surface::tab_chip_kind(kind) {
        work_surface::TabChipKind::Cli => base
            .flex()
            .items_center()
            .justify_center()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(8.0))
            .text_color(colors.fg)
            .child("\u{276f}")
            .into_any_element(),
        work_surface::TabChipKind::Term => base
            .relative()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .w(px(14.0))
                    .h(px(4.0))
                    .bg(colors.fg),
            )
            .child(
                div()
                    .absolute()
                    .left(px(3.0))
                    .top(px(7.0))
                    .w(px(5.0))
                    .h(px(2.0))
                    .rounded(px(1.0))
                    .bg(colors.fg),
            )
            .into_any_element(),
    }
}

/// The unified tab strip's drag-to-reorder value (GitHub issue #16) - both
/// [`AdeApp::render_agent_tab`] and [`AdeApp::render_file_tab`]'s `on_drag`/`on_drag_move`/
/// `on_drop` share this one type (rather than the two separate, kind-locked types this revision
/// replaces) precisely so an agent tab can be dropped onto a file tab and vice versa: GPUI's
/// `on_drop::<T>`/`on_drag_move::<T>` dispatch purely on the dragged value's concrete type
/// (verified against `vendor/zed/crates/gpui/src/elements/div.rs`, and
/// `crate::root::scrollbar`'s own module docs on that exact dispatch rule), so two distinct types
/// could never cross-target each other's drop handlers - real GPUI drag-and-drop
/// (`on_drag`/`on_drag_move`/`on_drop`), not this project's earlier `on_drag`/`on_drag_move` use
/// in `crate::root::resize` (a resize-handle hack with no real drag *payload* at all), and
/// mirroring `vendor/zed/crates/workspace/src/pane.rs`'s own `DraggedTab` for the "reorder tabs
/// by dragging" pattern itself. `Render`ing the dragged value as its own small floating chip -
/// the same choice Zed's own `DraggedTab` makes - keeps what's being dragged legible.
#[derive(Clone)]
pub(crate) enum DraggedTab {
    Agent {
        id: AgentId,
        label: String,
    },
    File {
        path: PathBuf,
        label: String,
    },
    /// GitHub issue #93 - the git graph tab, dragged the same way. No id/path payload: like
    /// [`work_surface::TabRef::Graph`], there is only ever at most one real graph tab.
    Graph {
        label: String,
    },
}

impl DraggedTab {
    fn label(&self) -> &str {
        match self {
            DraggedTab::Agent { label, .. } => label,
            DraggedTab::File { label, .. } => label,
            DraggedTab::Graph { label } => label,
        }
    }

    /// This dragged value's own identity as a [`work_surface::TabRef`] - what
    /// [`AdeApp::reorder_tab`] actually moves, regardless of which concrete kind was dragged.
    pub(in crate::work_surface) fn tab_ref(&self) -> work_surface::TabRef {
        match self {
            DraggedTab::Agent { id, .. } => work_surface::TabRef::Agent(*id),
            DraggedTab::File { path, .. } => work_surface::TabRef::File(path.clone()),
            DraggedTab::Graph { .. } => work_surface::TabRef::Graph,
        }
    }
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // `.opacity(..)` (GitHub issue #16's "a semi-transparent snapshot of the tab") applies to
        // the whole subtree, so the border/text fade along with the fill rather than staying
        // full-strength inside a see-through box.
        div()
            .opacity(0.85)
            .px(px(10.0))
            .py(px(4.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::PALETTE)
            .border_1()
            .border_color(theme::border::POPOVER)
            .font(font(theme::font::SANS))
            .text_size(px(11.0))
            .text_color(theme::text::BODY)
            .child(self.label().to_string())
    }
}

/// A precise insertion-point marker (GitHub issue #16's "better visual feedback" ask) - a thin
/// vertical bar at the exact boundary a dropped tab would land on: a hovered tab's own left edge
/// if `insert_after` is `false` (the dragged tab would land immediately before it), its right
/// edge if `insert_after` is `true` (immediately after) - replacing the old whole-tab `border_l`
/// highlight, which never distinguished "before" from "after" the hovered tab, with an
/// unambiguous "it lands exactly here" caret instead.
pub(in crate::work_surface) fn render_tab_insertion_caret(insert_after: bool) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.0))
        .bottom(px(0.0))
        .w(px(2.0))
        .bg(theme::status::ASK)
        .when(insert_after, |el| el.right(px(0.0)))
        .when(!insert_after, |el| el.left(px(0.0)))
}

/// How long a dropped tab's own settle-in fade runs - short and non-blocking, matching the
/// design handoff's own "animations are short (~120-180ms)" ask (GitHub issue #16 §5).
pub(in crate::work_surface) const TAB_SETTLE_ANIMATION_DURATION: Duration =
    Duration::from_millis(150);

/// A fresh `gpui::AnimationExt::with_animation` id for `tab_ref`, if [`AdeApp::dropped_tab_settle`]
/// says it's the tab a real drop most recently placed - `None` for every other tab, and for a
/// tab that was never the target of a real drop this session. A distinct `String` per drop
/// (`Self::drop_dragged_tab`'s own `next_tab_settle_id` counter baked into the id) rather than a
/// fixed per-tab id, because GPUI keys its own animation progress purely off this id string
/// (`vendor/zed/crates/gpui/src/elements/animation.rs`'s `AnimationState`) - reusing the same id
/// across two different drops of the same tab would resume the *first* drop's already-finished
/// animation instead of starting a fresh one.
pub(in crate::work_surface) fn tab_settle_animation_id(
    settle: &Option<(work_surface::TabRef, u64)>,
    tab_ref: &work_surface::TabRef,
) -> Option<String> {
    match settle {
        Some((settled, id)) if settled == tab_ref => Some(format!("tab-settle-{id}")),
        _ => None,
    }
}

/// The agent context bar's status pill: a coloured dot plus label in the status colour.
pub(in crate::work_surface) fn render_status_pill(status: Status) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(19.0))
        .px(px(7.0))
        .rounded(theme::radius::CHIP)
        .bg(status.pill_bg())
        .child(
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded(px(2.5))
                .bg(status.color()),
        )
        .child(
            div()
                .flex_none()
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(10.0))
                .text_color(status.color())
                .child(status.label()),
        )
}

/// Regression coverage for this revision's core claim: each worktree gets one rail entry, and
/// its agents become tabs scoped to whichever worktree's rail row is selected - the exact
/// behavior `crate::root::mod`'s "One rail row per worktree" module docs describe. Also covers
/// the real drag-to-reorder mechanism (`DraggedAgentTab`'s own docs).
#[cfg(test)]
mod tab_scoping_tests {
    use super::*;
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Focusable, TestAppContext};

    fn worktree_item(path: PathBuf, label: &str) -> WorktreeItem {
        WorktreeItem {
            path,
            label: label.to_string(),
            branch: Some(label.to_string()),
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

    fn seed_two_worktrees(app: &mut AdeApp, wt_a: PathBuf, wt_b: PathBuf) {
        app.worktrees = vec![worktree_item(wt_a, "wt-a"), worktree_item(wt_b, "wt-b")];
    }

    /// The exact bug `crate::rail::state::build_worktree_rows` fixes at the pure-logic level
    /// (`crate::rail::state::tests::build_worktree_rows_folds_every_agent_in_a_worktree_not_just_the_first`),
    /// proven here end to end through the real `Agents`/`AdeApp` plumbing: two agents
    /// spawned into the same worktree must both show up as that worktree's tabs.
    #[gpui::test]
    fn multiple_agents_in_one_worktree_all_show_as_tabs_under_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.agents.spawn(
                AgentKind::Claude,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        let ids: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(
            ids.len(),
            2,
            "both agents spawned into wt-a must show as tabs under it - not just the first \
             one found (the exact bug the old ProjectChild model had)"
        );
    }

    /// The real gap this revision's own self-audit found: switching to a worktree with *no*
    /// open agent leaves `Agents::focus_active` with nothing to focus (a genuine no-op), so
    /// a previously-focused agent's pane - now unrendered once the tab strip's own
    /// per-worktree filter applies - would otherwise leave `Window::focus` dangling, breaking
    /// every global keybinding (including ⌘P itself) until the next click.
    /// `Self::select_worktree`'s own fallback (redirecting focus to `Self::filter_focus_handle`
    /// whenever the newly selected worktree has no agent to focus) closes this.
    #[gpui::test]
    fn ctrl_p_still_works_after_switching_to_a_worktree_with_no_open_agent(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_empty = tempfile::tempdir().expect("tempdir empty");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_empty.path().to_path_buf(), "empty")];
        });

        // Explicitly focus the initial shell agent (the real, concrete "focus is on a live
        // terminal pane" starting state this bug needs), then switch to the agent-less
        // worktree - the exact transition the fix targets.
        app.update_in(cx, |app, window, cx| {
            app.agents.focus_active(window, cx);
            app.select_worktree(0, window, cx);
        });

        let key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after switching to an agent-less worktree must still open \
             the palette - before the fix, focus was left dangling on the previous worktree's \
             now-unrendered terminal pane"
        );
    }

    /// Switching the rail selection to a different worktree must show that worktree's own tabs,
    /// not the previously selected worktree's - the centre-pane-follows-the-rail invariant, and
    /// the exact behavior `crate::root::mod`'s "One rail row per worktree" docs describe: never
    /// showing/pointing at the previously selected worktree's agent.
    #[gpui::test]
    fn switching_worktree_selection_shows_that_worktrees_own_tabs(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let wt_b = tempfile::tempdir().expect("tempdir b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            seed_two_worktrees(app, wt_a.path().to_path_buf(), wt_b.path().to_path_buf());
        });

        let (id_a, id_b) = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let id_a = app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.select_worktree(1, window, cx);
            let id_b = app.agents.spawn(
                AgentKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (id_a, id_b)
        });

        // Still on wt-b (the last selection) - its tab strip must show only its own agent.
        let current: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(current, vec![id_b]);
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(id_b)
        );

        // Switch back to wt-a - must show id_a, never id_b.
        app.update_in(cx, |app, window, cx| app.select_worktree(0, window, cx));
        let current: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(
            current,
            vec![id_a],
            "switching back to wt-a must show its own tab, not wt-b's"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(id_a),
            "the active agent must follow the selected worktree"
        );
    }

    /// Closing the active tab in a worktree that still has another open tab must fall back to
    /// that sibling - the ordinary, non-degenerate case.
    #[gpui::test]
    fn closing_the_active_tab_falls_back_to_a_sibling_in_the_same_worktree(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });

        let (id1, id2) = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let id1 = app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let id2 = app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (id1, id2)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(id2)
        );

        app.update_in(cx, |app, window, cx| app.close_agent(id2, window, cx));

        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(id1),
            "closing the active tab must fall back to the remaining sibling in the same worktree"
        );
        let current: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(current, vec![id1]);
    }

    /// The degenerate case, and the real reason `Agents::close`'s fallback had to become
    /// worktree-scoped rather than a same-`Vec` neighbor: closing the *last* tab in one worktree
    /// must never fall back to a different worktree's own still-open agent, even though it
    /// might sit right next to it in the flat underlying storage.
    ///
    /// Also real, live-reproduced coverage for another instance of this project's own "focus
    /// left pointing at something unrendered" bug class (see `crate::root::focus`'s module doc):
    /// before the fix, `Self::close_agent` left `Window::focus` dangling on the
    /// just-`shutdown()` `TerminalPane` in this exact case (`self.agents.active = None`, and
    /// `Agents::focus_active` is a real no-op with nothing active) - so a real ⌘P afterward,
    /// not just checking `active_id() == None`, is what actually proves focus isn't dangling,
    /// matching every other test for this bug class in this project.
    #[gpui::test]
    fn closing_the_last_tab_in_a_worktree_never_falls_back_to_a_different_worktrees_agent(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let wt_b = tempfile::tempdir().expect("tempdir b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        app.update(cx, |app, _cx| {
            seed_two_worktrees(app, wt_a.path().to_path_buf(), wt_b.path().to_path_buf());
        });

        let id_a = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.close_agent(id_a, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            None,
            "closing the only tab in wt-a must leave it with no active agent, never silently \
             fall back to wt-b's own still-open agent"
        );

        let key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(key);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after closing the last tab in a worktree must still open \
             the palette - before the fix, Window::focus was left dangling on the just-closed \
             agent's now-unmounted pane, with nothing real for the next keystroke to reach"
        );
    }

    /// Real drag-to-reorder, unified across agent and file tabs (GitHub issue #16): dropping
    /// one agent tab onto another must actually change the *combined* tab order
    /// (`Self::current_worktree_agents`, which now reads `Self::combined_tab_order` rather than
    /// `Agents`' own raw creation order - see that method's own docs on why).
    #[gpui::test]
    fn drag_reordering_two_agent_tabs_changes_their_order(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, id2, id3) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.agents.active_id().expect("initial shell agent");
            let id2 = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let id3 = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (initial_id, id2, id3)
        });

        let before: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(before, vec![initial_id, id2, id3]);

        // The real drop handler's own logic: drag id3, drop it before `initial_id`'s tab.
        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::Agent(id3),
                work_surface::TabRef::Agent(initial_id),
                false,
                cx,
            );
        });

        let after: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(
            after,
            vec![id3, initial_id, id2],
            "id3 must now sit immediately before initial_id, and id2 must be otherwise \
             untouched"
        );
    }

    /// `Self::reorder_tab`/`work_surface::state::move_tab_order` must never corrupt the tab order
    /// on a bad target - an unknown dragged id, an unknown drop target, or dropping a tab onto
    /// itself must all be real no-ops.
    #[gpui::test]
    fn drag_reorder_is_a_no_op_for_an_unknown_or_identical_id(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let initial_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("initial shell agent")
        });

        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(initial_id),
                false,
                cx,
            );
            app.reorder_tab(
                work_surface::TabRef::Agent(9999),
                work_surface::TabRef::Agent(initial_id),
                false,
                cx,
            );
            app.reorder_tab(
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(9999),
                false,
                cx,
            );
        });

        let after: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(
            after,
            vec![initial_id],
            "none of these malformed drops should have changed anything"
        );
    }

    /// Exactly the globally active agent's pane may poll at the frame-accurate foreground
    /// cadence (`TerminalPane::is_foreground`); every other open pane must be demoted to the
    /// background cadence - through every real mutator of "which agent is active": spawn,
    /// tab click (`select_agent`), closing the active tab, and switching to an agent-less
    /// worktree. A pane wrongly left foreground silently re-grows the measured multi-pane
    /// foreground-drain regression this flag exists to bound (see
    /// `crate::terminal::pane::BACKGROUND_POLL_INTERVAL`'s docs); one wrongly left background would lag
    /// the very pane the user is watching.
    #[gpui::test]
    fn only_the_active_agents_pane_polls_at_the_foreground_cadence(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_empty = tempfile::tempdir().expect("tempdir empty");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let foreground_ids =
            |app: &gpui::Entity<AdeApp>, cx: &mut TestAppContext| -> Vec<AgentId> {
                app.read_with(cx, |app, cx| {
                    app.agents
                        .iter()
                        .filter(|s| s.pane.read(cx).is_foreground())
                        .map(|s| s.id)
                        .collect()
                })
            };

        let (first_id, second_id) = app.update_in(cx, |app, window, cx| {
            let first_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (first_id, second_id)
        });

        // Spawning made the new agent active - it alone must be foreground.
        assert_eq!(
            foreground_ids(&app, cx),
            vec![second_id],
            "after spawn, only the newly active agent's pane may be foreground"
        );

        // A real tab click: the clicked agent's pane is promoted, the old one demoted.
        app.update_in(cx, |app, window, cx| {
            app.select_agent(first_id, window, cx);
        });
        assert_eq!(
            foreground_ids(&app, cx),
            vec![first_id],
            "selecting a tab must promote exactly that pane and demote the previous one"
        );

        // Closing the active tab promotes the surviving sibling.
        app.update_in(cx, |app, window, cx| {
            app.agents.close(first_id, false, window, cx);
        });
        assert_eq!(
            foreground_ids(&app, cx),
            vec![second_id],
            "closing the active tab must hand the foreground cadence to the promoted sibling"
        );

        // Switching to a worktree with no agents: nothing is active, nothing is watchable -
        // every pane must be background.
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_empty.path().to_path_buf(), "empty")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        assert_eq!(
            foreground_ids(&app, cx),
            Vec::<AgentId>::new(),
            "with no active agent, no pane may keep the foreground cadence"
        );
    }

    /// The real cross-kind capability GitHub issue #16 exists to unlock: a file tab dragged so it
    /// lands between two agent tabs must actually interleave them in the combined tab order,
    /// not just reorder within its own kind - the exact case the old, kind-locked
    /// `DraggedAgentTab`/`DraggedFileTab` types could never produce (GPUI's `on_drop::<T>`
    /// dispatches purely on the dragged value's concrete type, so a `DraggedFileTab` could never
    /// be dropped onto an agent tab's `on_drop::<DraggedAgentTab>` handler or vice versa).
    ///
    /// The second tab spawned here is a real `Claude` agent, not a second `Shell` - Revision
    /// R12 §3's bare-worktree suppression (`Self::current_worktree_is_bare`) treats "every open
    /// agent is a `Shell`" as bare and hides file tabs from `Self::combined_tab_order` entirely
    /// (`a_bare_worktrees_tab_strip_shows_only_its_shell_tab_and_preserves_file_tab_state`
    /// covers that directly), which would make this drag-interleaving assertion moot for reasons
    /// that have nothing to do with what this test actually exercises.
    #[gpui::test]
    fn dragging_a_file_tab_between_two_agent_tabs_interleaves_them(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("a.txt");
        std::fs::write(&file_path, "hello\n").expect("write a.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                AgentKind::Claude,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_file_view(file_path.clone(), window, cx);
            (initial_id, second_id)
        });

        let before = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            before,
            vec![
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(second_id),
                work_surface::TabRef::File(PathBuf::from("a.txt")),
            ],
            "with no drag yet, agents come first (creation order), then files - the old \
             two-block layout"
        );

        // The real cross-kind drop: drag the file tab so it lands between the two agent tabs.
        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::File(PathBuf::from("a.txt")),
                work_surface::TabRef::Agent(second_id),
                false,
                cx,
            );
        });

        let after = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            after,
            vec![
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::File(PathBuf::from("a.txt")),
                work_surface::TabRef::Agent(second_id),
            ],
            "the file tab must now sit between the two agent tabs - the real cross-group \
             interleaving this revision exists to unlock"
        );
    }

    /// GitHub issue #93: the git graph tab must be a full, kind-agnostic member of the same
    /// drag-to-reorder system as agent/file tabs, not a fixed trailing entry - dragging it so it
    /// lands between two agent tabs must actually interleave it into `Self::combined_tab_order`,
    /// mirroring `dragging_a_file_tab_between_two_agent_tabs_interleaves_them`'s own file-tab
    /// case exactly.
    #[gpui::test]
    fn dragging_the_graph_tab_between_two_agent_tabs_interleaves_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                AgentKind::Claude,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_git_graph(window, cx);
            (initial_id, second_id)
        });

        let before = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            before,
            vec![
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(second_id),
                work_surface::TabRef::Graph,
            ],
            "with no drag yet, agents come first (creation order), then the freshly opened \
             graph tab"
        );

        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::Graph,
                work_surface::TabRef::Agent(second_id),
                false,
                cx,
            );
        });

        let after = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            after,
            vec![
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Graph,
                work_surface::TabRef::Agent(second_id),
            ],
            "the graph tab must now sit between the two agent tabs, the same real \
             cross-kind interleaving file/agent tabs already get"
        );
    }

    /// `Self::start_dragging_tab`/`Self::drop_dragged_tab` must treat `TabRef::Graph` exactly
    /// like any other tab kind - recording it while dragged, then clearing that record once
    /// dropped - mirroring `start_dragging_tab_records_exactly_the_tab_that_started_the_drag`'s
    /// own agent-tab case.
    #[gpui::test]
    fn dragging_the_graph_tab_dims_its_own_slot_then_clears_on_drop(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let initial_id = app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
            app.agents.active_id().expect("initial shell agent")
        });

        app.update(cx, |app, cx| {
            app.start_dragging_tab(work_surface::TabRef::Graph, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.dragging_tab.clone()),
            Some(work_surface::TabRef::Graph),
            "starting a drag on the graph tab must record exactly that tab"
        );

        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Graph,
                work_surface::TabRef::Agent(initial_id),
                cx,
            );
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.dragging_tab.clone()),
            None,
            "a handled drop must clear the now-stale dragging-tab state, same as any other kind"
        );
        // GitHub issue #93's own extension of the settle-fade (GitHub issue #16 §5): dropping
        // the graph tab must record it into `Self::dropped_tab_settle` exactly like any other
        // kind - `Self::drop_dragged_tab` is already kind-agnostic here, so this is really
        // proving `render_graph_tab` reads the same real field, not a separate code path.
        assert_eq!(
            app.read_with(cx, |app, _| app.dropped_tab_settle.clone().map(|(t, _)| t)),
            Some(work_surface::TabRef::Graph),
            "a real drop of the graph tab must record it for the settle-in fade too"
        );
    }

    /// `Self::drop_dragged_tab` must actually honor whichever half of the target tab the cursor
    /// was last recorded over (`Self::tab_drag_insertion`, set by
    /// `Self::update_tab_drag_insertion`'s own real per-tab `on_drag_move` tracking) - "after"
    /// must land the dragged tab on the far side of the target from a plain "before" drop - and
    /// must clear that state once handled, so a later drop on a different tab can't inherit it.
    #[gpui::test]
    fn drop_dragged_tab_honors_the_recorded_insertion_side_then_clears_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (initial_id, second_id)
        });

        // Simulates what a real `on_drag_move` tick over the right half of `second_id`'s tab
        // would already have recorded, just before the drop.
        app.update(cx, |app, _cx| {
            app.tab_drag_insertion = Some((work_surface::TabRef::Agent(second_id), true));
        });

        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(second_id),
                cx,
            );
        });

        let order = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            order,
            vec![
                work_surface::TabRef::Agent(second_id),
                work_surface::TabRef::Agent(initial_id),
            ],
            "insert_after == true must land the dragged tab immediately after the target, not \
             before"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.tab_drag_insertion.clone()),
            None,
            "a handled drop must clear the now-stale insertion-caret state"
        );
    }

    /// `Self::start_dragging_tab` (the real `on_drag` constructor callback's body, GitHub issue
    /// #16's "the original slot renders dimmed" ask) must record exactly the tab that started
    /// the drag, so `Self::render_file_tab`/`Self::render_agent_tab`'s own `is_dragging` check
    /// dims the right slot and no other.
    #[gpui::test]
    fn start_dragging_tab_records_exactly_the_tab_that_started_the_drag(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let initial_id = app.read_with(cx, |app, _| app.agents.active_id().expect("shell agent"));

        assert_eq!(
            app.read_with(cx, |app, _| app.dragging_tab.clone()),
            None,
            "premise: nothing is being dragged yet"
        );

        app.update(cx, |app, cx| {
            app.start_dragging_tab(work_surface::TabRef::Agent(initial_id), cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.dragging_tab.clone()),
            Some(work_surface::TabRef::Agent(initial_id)),
            "starting a drag must record exactly the tab that started it"
        );
    }

    /// A real drop must clear `Self::dragging_tab` alongside `Self::tab_drag_insertion` - a
    /// dropped tab's slot must never stay dimmed once the drag it was dimmed for is over.
    #[gpui::test]
    fn dropping_a_tab_clears_dragging_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (initial_id, second_id)
        });

        app.update(cx, |app, cx| {
            app.start_dragging_tab(work_surface::TabRef::Agent(initial_id), cx);
        });
        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(second_id),
                cx,
            );
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.dragging_tab.clone()),
            None,
            "a handled drop must clear the now-stale dragging-tab state"
        );
    }

    /// `Self::cancel_any_tab_drag` (the workspace body's real cancelled-drag cleanup - GPUI gives
    /// no dedicated callback for Esc/release-outside-any-target, see `AdeApp::dragging_tab`'s own
    /// docs) must clear both drag-tracking fields together and report whether it actually did
    /// anything, so a click that was never a drag doesn't force an unnecessary re-render.
    #[gpui::test]
    fn cancel_any_tab_drag_clears_both_fields_and_reports_whether_anything_changed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let initial_id = app.read_with(cx, |app, _| app.agents.active_id().expect("shell agent"));

        assert!(
            !app.update(cx, |app, _cx| app.cancel_any_tab_drag()),
            "with nothing in progress, cancelling must report that nothing changed"
        );

        app.update(cx, |app, cx| {
            app.start_dragging_tab(work_surface::TabRef::Agent(initial_id), cx);
            app.tab_drag_insertion = Some((work_surface::TabRef::Agent(initial_id), true));
        });

        assert!(
            app.update(cx, |app, _cx| app.cancel_any_tab_drag()),
            "with a real in-progress drag, cancelling must report that it actually cleared \
             something"
        );
        assert_eq!(app.read_with(cx, |app, _| app.dragging_tab.clone()), None);
        assert_eq!(
            app.read_with(cx, |app, _| app.tab_drag_insertion.clone()),
            None
        );
    }

    /// `tab_settle_animation_id` (the pure logic behind the settle-in fade, GitHub issue #16's
    /// "dropping animates the tab settling into its slot") must return `Some` only for the tab
    /// that was actually dropped, and `None` for every other tab, including one dropped in a
    /// previous, no-longer-current drop.
    #[test]
    fn tab_settle_animation_id_matches_only_the_settled_tab() {
        let dropped = work_surface::TabRef::File(PathBuf::from("a.txt"));
        let other = work_surface::TabRef::File(PathBuf::from("b.txt"));
        let settle = Some((dropped.clone(), 7));

        assert_eq!(
            tab_settle_animation_id(&settle, &dropped),
            Some("tab-settle-7".to_string())
        );
        assert_eq!(tab_settle_animation_id(&settle, &other), None);
        assert_eq!(tab_settle_animation_id(&None, &dropped), None);
    }

    /// Two separate drops of the very same tab must get two different animation ids - reusing
    /// one would resume GPUI's own already-finished animation state for that id instead of
    /// starting a fresh fade (`tab_settle_animation_id`'s own docs on why).
    #[test]
    fn tab_settle_animation_id_is_fresh_across_two_drops_of_the_same_tab() {
        let tab = work_surface::TabRef::File(PathBuf::from("a.txt"));
        let first_drop = tab_settle_animation_id(&Some((tab.clone(), 1)), &tab);
        let second_drop = tab_settle_animation_id(&Some((tab.clone(), 2)), &tab);

        assert_ne!(first_drop, second_drop);
    }

    /// A real drop (`Self::drop_dragged_tab`) must record `Self::dropped_tab_settle` for exactly
    /// the tab that was dropped, and a second, later drop of a different tab must overwrite it
    /// with a fresh id rather than leaving the first tab's own id behind.
    #[gpui::test]
    fn drop_dragged_tab_records_a_fresh_settle_id_for_the_dropped_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (initial_id, second_id)
        });

        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(second_id),
                cx,
            );
        });
        let (first_settled, first_id) = app
            .read_with(cx, |app, _| app.dropped_tab_settle.clone())
            .expect("a real drop must record a settle id");
        assert_eq!(first_settled, work_surface::TabRef::Agent(initial_id));

        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Agent(second_id),
                work_surface::TabRef::Agent(initial_id),
                cx,
            );
        });
        let (second_settled, second_id_recorded) = app
            .read_with(cx, |app, _| app.dropped_tab_settle.clone())
            .expect("the second real drop must also record a settle id");
        assert_eq!(second_settled, work_surface::TabRef::Agent(second_id));
        assert_ne!(
            first_id, second_id_recorded,
            "a later drop must never reuse an earlier drop's own settle id"
        );
    }

    /// Revision R12 §3, proven end to end: two real `Claude` agents spawned into the same
    /// worktree both resolve `program_label()` to the same literal `"claude"`, so without
    /// disambiguation the tab strip would render two identical tabs.
    /// `current_worktree_agent_tab_labels` must give them distinct ordinals, in the order they
    /// were spawned.
    #[gpui::test]
    fn two_agents_of_the_same_kind_in_one_worktree_get_distinct_ordinal_tab_labels(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        let (first_id, second_id) = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let first_id = app.agents.spawn(
                AgentKind::Claude,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let second_id = app.agents.spawn(
                AgentKind::Claude,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (first_id, second_id)
        });

        let labels = app.update(cx, |app, cx| {
            app.current_worktree_agent_tab_labels(&[first_id, second_id], cx)
        });
        assert_eq!(
            labels,
            vec!["claude #1".to_string(), "claude #2".to_string()],
            "two agents of the same kind must never render two identical tab labels"
        );
    }

    /// Revision R12 §3: a worktree with only its default `Shell` agent is "bare" -
    /// `current_worktree_is_bare` must say so - and stops being bare the moment a real agent
    /// spawns into it, proven through the same live `Agents`/`AdeApp` plumbing every other test
    /// in this module uses (not just the pure `AgentKind` match this reads off of).
    #[gpui::test]
    fn a_worktree_with_only_its_default_shell_is_bare_until_a_real_agent_spawns(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        assert!(
            app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "a worktree with only a Shell agent must be reported bare"
        );

        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Claude,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        assert!(
            !app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "spawning a real agent must clear bare status - it's not just a snapshot taken once \
             at worktree creation"
        );
    }

    /// Revision R12 §3: a bare worktree's `Shell` tab reads `program \u{b7} branch` (via
    /// `work_surface::bare_worktree_shell_label`), not the generic `"terminal"` every non-bare
    /// shell tab uses - proven here against a real `TerminalPane`'s own resolved
    /// `program_label()`, not a hardcoded `"zsh"`.
    #[gpui::test]
    fn a_bare_worktrees_shell_tab_label_joins_the_real_program_with_its_branch(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        let shell_id = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            )
        });

        let (label, program) = app.update(cx, |app, cx| {
            let label = app
                .current_worktree_agent_tab_labels(&[shell_id], cx)
                .remove(0);
            let program = app
                .agents
                .iter()
                .find(|agent| agent.id == shell_id)
                .expect("shell agent")
                .pane
                .read(cx)
                .program_label();
            (label, program)
        });
        assert_eq!(
            label,
            work_surface::bare_worktree_shell_label(&program, Some("wt-a")),
            "a bare worktree's shell tab must show its real resolved program joined with its \
             branch, not the generic \"terminal\" label"
        );
        assert_ne!(
            label, "terminal",
            "the bare-worktree shell label must never fall back to the generic non-bare label"
        );
    }

    /// Revision R12 §3's exact `+` menu item list: "*New terminal* · *New agent*
    /// (`runs in <branch>`) · *Git graph* · *Open file…* · *Next changed file*." Proven against
    /// the real painted popover (`Self::render_dropdown_menu_row`'s own `debug_selector`, one per
    /// row, keyed by its label), not just the source order - and confirms there is genuinely no
    /// "New file" row any more (removed: the file tree already carries a real, always-visible
    /// replacement, `crate::sidebar::render::render_file_tree_row`/
    /// `render_right_sidebar_toggle`'s own per-directory and root-level "+" affordances).
    #[gpui::test]
    fn the_plus_menus_five_rows_match_revision_r12_3_in_order_with_no_new_file_row(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.run_until_parked();

        let new_terminal = cx
            .debug_bounds("dropdown-menu-row-New terminal")
            .expect("\"New terminal\" row must be painted");
        let new_agent = cx
            .debug_bounds("dropdown-menu-row-New agent")
            .expect("\"New agent\" row must be painted");
        let git_graph = cx
            .debug_bounds("dropdown-menu-row-Git graph")
            .expect("\"Git graph\" row must be painted");
        let open_file = cx
            .debug_bounds("dropdown-menu-row-Open file\u{2026}")
            .expect("\"Open file\u{2026}\" row must be painted");
        let next_changed = cx
            .debug_bounds("dropdown-menu-row-Next changed file")
            .expect("\"Next changed file\" row must be painted");

        assert!(
            new_terminal.origin.y < new_agent.origin.y
                && new_agent.origin.y < git_graph.origin.y
                && git_graph.origin.y < open_file.origin.y
                && open_file.origin.y < next_changed.origin.y,
            "the five rows must render top to bottom in exactly Revision R12 §3's order: New \
             terminal, New agent, Git graph, Open file\u{2026}, Next changed file"
        );

        assert!(
            cx.debug_bounds("dropdown-menu-row-New file").is_none(),
            "there must be no \"New file\" row - it is not one of §3's five items, and the file \
             tree already has a real replacement"
        );
        assert!(
            cx.debug_bounds("dropdown-menu-row-New agent pane")
                .is_none(),
            "the row must be relabelled \"New agent\", not the old \"New agent pane\""
        );
    }

    /// Revision R12 §3's `+` menu Git graph row, clicked for real (`cx.simulate_click` against
    /// the row's own painted bounds, not a direct `open_git_graph` call) - must actually open the
    /// graph tab, and close the menu behind it the same way every other row's click does.
    #[gpui::test]
    fn a_real_click_on_the_plus_menus_git_graph_row_opens_the_graph_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.graph_tab_open),
            "premise: the graph tab must not already be open before the click"
        );

        let git_graph = cx
            .debug_bounds("dropdown-menu-row-Git graph")
            .expect("\"Git graph\" row must be painted while the + menu is open");
        cx.simulate_click(git_graph.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_tab_open && app.graph_tab_active,
                "a real click on the Git graph row must genuinely open and activate the graph \
                 tab, not just close the menu"
            );
            assert!(
                !app.plus_menu_open,
                "the row's click handler must also close the + menu, matching every other row"
            );
        });
    }

    /// Revision R12 §3's `+` menu "New agent" row secondary text (`runs in <branch>`) must come
    /// from the real selected worktree's own recorded branch - the exact composition
    /// `Self::render_plus_menu` performs (`Self::current_worktree_branch` piped into
    /// `work_surface::new_agent_menu_secondary_text`) - not a hardcoded model/kind label the row
    /// used to show (`agent.kind.label()`, e.g. `"Claude"`).
    #[gpui::test]
    fn the_new_agent_rows_secondary_text_uses_the_real_selected_worktrees_branch(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(
                wt_a.path().to_path_buf(),
                "feature/real-branch",
            )];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });

        let (branch, secondary) = app.read_with(cx, |app, _| {
            let branch = app.current_worktree_branch();
            let secondary = work_surface::new_agent_menu_secondary_text(branch.as_deref());
            (branch, secondary)
        });

        assert_eq!(
            branch.as_deref(),
            Some("feature/real-branch"),
            "premise: the selected worktree's real branch must resolve to the seeded value"
        );
        assert_eq!(
            secondary, "runs in feature/real-branch",
            "the row must show the real branch, substituted in - not a literal placeholder"
        );
        assert_ne!(
            secondary, "Claude",
            "must never show a model/agent-kind label in the branch's place - the pre-fix bug"
        );
    }

    /// GitHub issue #120: an earlier revision (R12 §3, "a bare worktree shows only the shell
    /// tab") suppressed every open file tab in `Self::combined_tab_order` the moment a worktree
    /// went bare (no real agent, only its default `Shell` tab left). That's gone - a file tab
    /// opened before the worktree went bare must keep rendering, unchanged, exactly as it does
    /// with a real agent running. Bareness affects the shell's own tab *label* only (`Self::
    /// current_worktree_agent_tab_labels`'s bare-shell label - see the neighboring test), not
    /// which tabs render.
    #[gpui::test]
    fn a_bare_worktrees_tab_strip_still_shows_every_open_file_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        let claude_id = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let shell_id = app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let claude_id = app.agents.spawn(
                AgentKind::Claude,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_files_mut().push(PathBuf::from("README.md"));
            let _ = shell_id;
            claude_id
        });

        let order_with_agent = app.read_with(cx, |app, _| app.combined_tab_order());
        assert!(
            order_with_agent
                .iter()
                .any(|tab_ref| matches!(tab_ref, work_surface::TabRef::File(path) if path == &PathBuf::from("README.md"))),
            "premise: the file tab is genuinely open while a real agent is running"
        );
        assert!(
            !app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "premise: the worktree is not bare while the Claude agent is running"
        );

        app.update_in(cx, |app, window, cx| {
            app.archive_agent(claude_id, window, cx);
        });

        assert!(
            app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "archiving the only real agent must leave the worktree bare (only its default \
             Shell tab left)"
        );

        let order_while_bare = app.read_with(cx, |app, _| app.combined_tab_order());
        assert!(
            order_while_bare
                .iter()
                .any(|tab_ref| matches!(tab_ref, work_surface::TabRef::File(path) if path == &PathBuf::from("README.md"))),
            "the file tab must keep rendering while bare - opening and working across files must \
             never depend on an agent or even a shell running"
        );
        assert!(
            order_while_bare.iter().any(
                |tab_ref| matches!(tab_ref, work_surface::TabRef::Agent(id) if *id != claude_id)
            ),
            "the shell tab itself must still be there too"
        );

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![PathBuf::from("README.md")],
            "and the file's own entry in open_files_by_worktree is of course untouched"
        );
    }

    /// GitHub issue #112: switching between two terminals open in the *same* worktree - the
    /// common case for a rail/tab-strip click - must move real keyboard focus onto the newly
    /// selected terminal's own pane. Before the fix, `select_agent`'s same-worktree/no-file-tab
    /// branch never called `Window::focus` at all: `Window::focus` stayed on the previously
    /// active terminal's now-unmounted `TerminalPane` handle, so GPUI's dispatch fell back to
    /// the window root - outside the `"terminal"` key context - and typed input into the
    /// terminal that visibly looked selected silently went nowhere (or fell through to
    /// normally-suppressed global bindings instead), which reads to a user as the two terminals
    /// having "merged" into one.
    #[gpui::test]
    fn window_focus_follows_a_same_worktree_terminal_switch(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let first_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        });
        let second_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            )
        });

        app.update_in(cx, |app, window, cx| {
            app.select_agent(first_id, window, cx);
        });
        let first_pane_handle = app.update(cx, |app, cx| {
            app.agents
                .active()
                .expect("the first agent")
                .pane
                .focus_handle(cx)
        });
        assert_eq!(
            app.update_in(cx, |_app, window, cx| window.focused(cx))
                .as_ref(),
            Some(&first_pane_handle),
            "premise: selecting the first terminal moves real focus onto its own pane"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_agent(second_id, window, cx);
        });
        let second_pane_handle = app.update(cx, |app, cx| {
            app.agents
                .active()
                .expect("the second agent")
                .pane
                .focus_handle(cx)
        });
        assert_eq!(
            app.update_in(cx, |_app, window, cx| window.focused(cx))
                .as_ref(),
            Some(&second_pane_handle),
            "switching to the second terminal (no file tab open, same worktree) must move real \
             keyboard focus onto its own pane too, not leave it dangling on the first terminal's \
             now-unmounted handle"
        );
    }

    /// GitHub issue #100: opening a file via the real `Self::open_file_view` entry point (the
    /// Files tree row click handler - it never checks bareness, so a bare worktree's tree stays
    /// fully clickable) while the worktree was *already* bare, with no prior real agent, must
    /// still produce a real tab for it. GitHub issue #120 later made this the *only* real
    /// behavior left to test here - once bareness stopped suppressing file tabs at all, this
    /// stopped being a carved-out exception to anything.
    #[gpui::test]
    fn opening_a_file_in_an_already_bare_worktree_still_gets_a_real_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        std::fs::write(wt_a.path().join("README.md"), "hello\n").expect("write README.md");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });
        assert!(
            app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "premise: only a default Shell tab exists - no real agent has ever run here"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(wt_a.path().join("README.md"), window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(PathBuf::from("README.md")),
            "premise: the file really is open and active, exactly like `render_center_pane` \
             would render it - a bare worktree's file tree stays fully clickable"
        );
        let order = app.read_with(cx, |app, _| app.combined_tab_order());
        assert!(
            order
                .iter()
                .any(|tab_ref| matches!(tab_ref, work_surface::TabRef::File(path) if path == &PathBuf::from("README.md"))),
            "the file that is genuinely on screen right now must have a real tab, even while \
             the worktree is bare - the pre-fix bug left it with none at all"
        );
    }

    /// GitHub issue #116 (real dragging-while-bare tab-order corruption, fixed against an earlier
    /// revision where bareness truncated `combined_tab_order` to a single visible file tab) plus
    /// #120 (that truncation is gone entirely - see `a_bare_worktrees_tab_strip_still_shows_every_open_file_tab`).
    /// With no truncation left, dragging while bare is now just an ordinary drag - this keeps
    /// direct coverage that it still persists every open file's position correctly.
    #[gpui::test]
    fn dragging_a_tab_while_bare_persists_every_open_files_position(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        std::fs::write(wt_a.path().join("a.txt"), "a\n").expect("write a.txt");
        std::fs::write(wt_a.path().join("b.txt"), "b\n").expect("write b.txt");
        std::fs::write(wt_a.path().join("c.txt"), "c\n").expect("write c.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                AgentKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_file_view(wt_a.path().join("a.txt"), window, cx);
            app.open_file_view(wt_a.path().join("b.txt"), window, cx);
            app.open_file_view(wt_a.path().join("c.txt"), window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "premise: only the default Shell tab exists"
        );
        let bare_order = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            bare_order
                .iter()
                .filter(|t| matches!(t, work_surface::TabRef::File(_)))
                .count(),
            3,
            "premise: every open file tab renders even while bare (GitHub issue #120)"
        );

        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::File(PathBuf::from("c.txt")),
                work_surface::TabRef::File(PathBuf::from("a.txt")),
                false,
                cx,
            );
        });
        cx.run_until_parked();

        let order_after_drag = app.read_with(cx, |app, _| app.combined_tab_order());
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert!(
                order_after_drag.iter().any(
                    |tab_ref| matches!(tab_ref, work_surface::TabRef::File(path) if path == &PathBuf::from(name))
                ),
                "{name} must still be in the tab order after the drag"
            );
        }
        assert!(
            order_after_drag
                .iter()
                .position(|t| t == &work_surface::TabRef::File(PathBuf::from("c.txt")))
                < order_after_drag
                    .iter()
                    .position(|t| t == &work_surface::TabRef::File(PathBuf::from("a.txt"))),
            "the real drag must have really moved c.txt in front of a.txt"
        );

        let cwd = wt_a.path().to_path_buf();
        let persisted = app.read_with(cx, |app, _| app.tab_order_state.file_order(&cwd));
        assert_eq!(
            persisted.len(),
            3,
            "the on-disk persisted order must keep all three files"
        );
    }
}

/// GitHub issue #20's `TerminalClear` action - real coverage that dispatching it reaches
/// whichever agent is genuinely active right now, and only that one, mirroring
/// `crate::terminal::pane::clear_pty_signal_tests`' own "observe the pty's real echo, not a
/// direct call" discipline for proving `clear()`'s pty-signal half actually fired.
#[cfg(test)]
mod terminal_clear_action_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dispatching_terminal_clear_signals_only_the_active_agents_pty(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (first_id, second_id) = app.update_in(cx, |app, window, cx| {
            let first = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let second = app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (first, second)
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(second_id),
            "sanity check: spawning a second agent must make it the active one"
        );

        cx.dispatch_action(TerminalClear);

        let mut saw_caret_l_on_second = false;
        for _ in 0..50 {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(8));
            cx.run_until_parked();
            let second_lines = app.read_with(cx, |app, cx| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == second_id)
                    .expect("second agent")
                    .pane
                    .read(cx)
                    .visible_text_lines()
            });
            if second_lines.iter().any(|line| line.contains("^L")) {
                saw_caret_l_on_second = true;
                break;
            }
        }
        assert!(
            saw_caret_l_on_second,
            "expected the active (second) agent's pty to echo back the real Ctrl-L byte \
             TerminalClear's handler sends"
        );

        let first_lines = app.read_with(cx, |app, cx| {
            app.agents
                .iter()
                .find(|agent| agent.id == first_id)
                .expect("first agent")
                .pane
                .read(cx)
                .visible_text_lines()
        });
        assert!(
            !first_lines.iter().any(|line| line.contains("^L")),
            "the inactive (first) agent must never receive the clear signal - only the active \
             agent, matching handle_close_focused_tab_action's own 'act on whichever tab is \
             genuinely showing right now' target"
        );
    }
}

/// GitHub issue #16's own "the resulting layout... persists per session/worktree and restores on
/// relaunch" - real end-to-end coverage that a drag-reordered tab strip survives a genuine
/// second `AdeApp` instance, not just a worktree switch within the same one.
#[cfg(test)]
mod tab_order_persistence_tests {
    use super::*;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;

    fn open_test_app_with_real_persistence(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn a_drag_reordered_tab_strip_survives_a_real_restart(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let config_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = config_dir.path().join("settings.toml");
        std::fs::write(repo.path().join("a.txt"), "a\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "b\n").expect("write b.txt");

        // First "session": open both files (a.txt lands before b.txt, the natural open order),
        // then really drag b.txt in front of a.txt. A real, non-`Shell` agent is required first
        // - `Self::combined_tab_order` deliberately suppresses every file tab while the worktree
        // is "bare" (Revision R12 §3: "a bare worktree shows only the shell tab"), and the
        // default startup agent is a plain shell.
        let (app, cx) = open_test_app_with_real_persistence(
            cx,
            repo.path().to_path_buf(),
            settings_path.clone(),
        );
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Claude,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_file_view(repo.path().join("a.txt"), window, cx);
            app.open_file_view(repo.path().join("b.txt"), window, cx);
        });
        cx.run_until_parked();

        let order_before = app.read_with(cx, |app, _| app.combined_tab_order());
        let a_ref = work_surface::TabRef::File(PathBuf::from("a.txt"));
        let b_ref = work_surface::TabRef::File(PathBuf::from("b.txt"));
        assert!(
            order_before.iter().position(|t| t == &a_ref)
                < order_before.iter().position(|t| t == &b_ref),
            "premise: a.txt (opened first) must naturally sit before b.txt"
        );

        app.update(cx, |app, cx| {
            app.reorder_tab(b_ref.clone(), a_ref.clone(), false, cx);
        });
        cx.run_until_parked();
        let order_after_drag = app.read_with(cx, |app, _| app.combined_tab_order());
        assert!(
            order_after_drag.iter().position(|t| t == &b_ref)
                < order_after_drag.iter().position(|t| t == &a_ref),
            "premise: the real drag must have really moved b.txt in front of a.txt"
        );

        // A genuine second instance, matching exactly what a real app relaunch is: a fresh
        // `AdeApp` against the same repo and the same real settings directory, with no shared
        // in-memory state whatsoever - `expanded_folders_are_restored_exactly_after_a_simulated_
        // reload`'s own precedent (`crate::sidebar::render`) for testing a real restart.
        let (app, cx) =
            open_test_app_with_real_persistence(cx, repo.path().to_path_buf(), settings_path);
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Claude,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_file_view(repo.path().join("a.txt"), window, cx);
            app.open_file_view(repo.path().join("b.txt"), window, cx);
        });
        cx.run_until_parked();

        let restored_order = app.read_with(cx, |app, _| app.combined_tab_order());
        assert!(
            restored_order.iter().position(|t| t == &b_ref)
                < restored_order.iter().position(|t| t == &a_ref),
            "the drag-reordered position must survive a real restart - got {restored_order:?}"
        );
    }
}

/// GitHub issue #158's `TerminalCopy`/`TerminalPaste` actions - real coverage that dispatching
/// each one reaches whichever agent is genuinely active right now, and that copy really lands on
/// the real OS clipboard (checked the same way `crate::sidebar::tree_ops`' own
/// `copy_relative_path_writes_the_worktree_relative_path` checks "Copy Path", since this app has
/// exactly one clipboard mechanism and both go through it).
///
/// These fail against the pre-fix tree twice over: the actions didn't exist, and neither did any
/// selection for copy to read.
#[cfg(test)]
mod terminal_clipboard_action_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Focusable, TestAppContext};

    /// Places `text` at a fixed, addressed grid position in the active agent's pane, well below
    /// where a freshly-spawned shell's own prompt lands, so the row this test then selects can't
    /// be overwritten by real shell output arriving in the background.
    fn seed_active_pane(app: &gpui::Entity<AdeApp>, cx: &mut gpui::VisualTestContext, text: &str) {
        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|s| s.pane.clone()))
            .expect("a fresh test window has one real, active shell agent");
        pane.update(cx, |pane, cx| {
            // `ESC[10;1H` - row 10, column 1 (1-indexed), i.e. grid row 9, column 0.
            pane.inject_bytes_for_test(format!("\x1b[10;1H{text}").as_bytes(), cx);
            pane.select_cells_for_test(9, 0..text.chars().count());
        });
    }

    #[gpui::test]
    fn dispatching_terminal_copy_puts_the_real_selection_on_the_real_clipboard(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("stale".into()))
        });
        seed_active_pane(&app, cx, "ade-selected-text");

        cx.dispatch_action(TerminalCopy);

        let text = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            text.as_deref(),
            Some("ade-selected-text"),
            "TerminalCopy must reach the active agent's pane and write its real selection to \
             the real system clipboard"
        );
    }

    /// Only the *active* agent's selection is copied - the same "which pane does this act on"
    /// contract `terminal_clear_action_tests` pins for `TerminalClear`.
    #[gpui::test]
    fn dispatching_terminal_copy_uses_only_the_active_agents_selection(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // The window's own initial agent gets a selection first, then a second agent (which
        // becomes active) gets a different one.
        seed_active_pane(&app, cx, "background-agent-text");
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        seed_active_pane(&app, cx, "active-agent-text");

        cx.dispatch_action(TerminalCopy);

        let text = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("active-agent-text"));
    }

    /// The whole point of GitHub issue #158, driven by a **real keystroke** rather than
    /// `dispatch_action`: with a terminal genuinely focused, the copy shortcut must reach
    /// `TerminalCopy` and not `TerminalPane::handle_key_down`.
    ///
    /// This is the assertion that pins the fix's most load-bearing detail. Without the binding,
    /// `crate::terminal::pane::keystroke_to_bytes`'s control-byte branch ignores `shift`
    /// entirely, so `Ctrl+Shift+C` over a focused terminal produced `0x03` - it *interrupted the
    /// running process* instead of copying. GPUI dispatches matching `KeyBinding`s before any
    /// `on_key_down` listener and an action handler stops propagation by default in the bubble
    /// phase, which is what makes a bound action win here.
    #[gpui::test]
    fn the_real_copy_keystroke_over_a_focused_terminal_copies_instead_of_typing(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("stale".into()))
        });
        seed_active_pane(&app, cx, "typed-not-copied");

        // The keystroke can only reach a `"terminal"`-scoped binding while the pane genuinely
        // holds focus - which is the exact condition the issue reports the bug under.
        app.update_in(cx, |app, window, cx| {
            let pane = app.agents.active().expect("an active agent").pane.clone();
            let handle = pane.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-shift-c"
        });
        cx.run_until_parked();

        let text = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            text.as_deref(),
            Some("typed-not-copied"),
            "the real copy keystroke over a focused terminal must reach TerminalCopy - before              this fix it reached keystroke_to_bytes and sent SIGINT to the child process"
        );
    }

    /// The paste counterpart of the keystroke test above, proven by the real pty echo.
    #[gpui::test]
    fn the_real_paste_keystroke_over_a_focused_terminal_reaches_the_pty(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                "ade-keystroke-paste".into(),
            ))
        });
        app.update_in(cx, |app, window, cx| {
            let pane = app.agents.active().expect("an active agent").pane.clone();
            let handle = pane.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-shift-v"
        });

        let mut saw_pasted_text = false;
        for _ in 0..50 {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(8));
            cx.run_until_parked();
            let lines = app.read_with(cx, |app, cx| {
                app.agents
                    .active()
                    .expect("an active agent")
                    .pane
                    .read(cx)
                    .visible_text_lines()
            });
            if lines
                .iter()
                .any(|line| line.contains("ade-keystroke-paste"))
            {
                saw_pasted_text = true;
                break;
            }
        }
        assert!(
            saw_pasted_text,
            "the real paste keystroke over a focused terminal must reach TerminalPaste"
        );
    }

    /// The paste half, proven by the pty's own echo rather than by asserting `write_input` was
    /// called - mirroring `terminal_clear_action_tests`' discipline for `TerminalClear`.
    #[gpui::test]
    fn dispatching_terminal_paste_reaches_the_active_agents_real_pty(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("ade-pasted-marker".into()))
        });
        cx.dispatch_action(TerminalPaste);

        let mut saw_pasted_text = false;
        for _ in 0..50 {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(8));
            cx.run_until_parked();
            let lines = app.read_with(cx, |app, cx| {
                app.agents
                    .active()
                    .expect("an active agent")
                    .pane
                    .read(cx)
                    .visible_text_lines()
            });
            if lines.iter().any(|line| line.contains("ade-pasted-marker")) {
                saw_pasted_text = true;
                break;
            }
        }
        assert!(
            saw_pasted_text,
            "expected the active agent's real pty to echo back the clipboard text \
             TerminalPaste's handler writes"
        );
    }
}
