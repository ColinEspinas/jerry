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
    /// Spawns a new agent tab into [`Self::current_worktree_path`] - the single real chokepoint every
    /// "new terminal"/"new shell" entry point in this app funnels through: `secondary-n`/
    /// `ctrl-shift-T`'s own `handle_new_agent_action`/`handle_new_terminal_action`, the `+` menu's
    /// row, the title bar's Agent menu row (`crate::title_bar::menu::AdeApp::agent_menu_rows`),
    /// and the palette's `PaletteCommand::NewShell`.
    ///
    /// GitHub issue #90: a genuinely empty window (no [`Self::focused_repo`]) has no real repo
    /// root to spawn into at all - a real, live-reproduced bug (independent audit) found that
    /// without this guard, [`Self::current_worktree_path`] fell through to [`Self::focused_repo_path`],
    /// which itself used to fall back to *some other, unopened* known repo's real path (`Self::
    /// repos.first()`), silently spawning a real PTY - and, from there, reachable real destructive
    /// git operations (`Keep All Changes`/`Discard Worktree`) - against a repo the user never
    /// opened and can't even see (the empty-state view renders no tab strip at all, so the tab
    /// this spawned would have been invisible). A no-op here is the honest fix: there is nothing
    /// this app can offer to spawn an agent into until a real repo is opened.
    pub(crate) fn new_agent(
        &mut self,
        kind: ProcessKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focused_repo().is_none() {
            return;
        }
        // A tab is only ever attributable to a real, currently-selected worktree - there is no
        // such thing as "a repo's own tab". With no worktree genuinely selected there is nothing
        // legitimate to spawn into, so this refuses rather than falling back to the repo root as
        // it used to (see `Self::current_worktree_path`'s own docs for the family of live-reproduced
        // bugs that fallback caused).
        let Some(cwd) = self.current_worktree_path() else {
            return;
        };
        // GitHub issue #239 phase 2: a Claude agent is spawned against this instance's generated
        // `--settings` file and told, through its environment, where to report its hooks. Taken as
        // an owned snapshot because `self.agents.spawn` borrows `self.agents` mutably - see
        // `crate::hooks::HookRuntime::injection`.
        let hook_injection = self.hook_injection_for(kind);
        let id = self.agents.spawn(
            kind,
            cwd,
            self.settings.appearance.terminal_font_size,
            self.settings.terminal.shell_override(),
            hook_injection.as_ref(),
            window,
            cx,
        );
        // GitHub issue #225: capture this agent's review baseline - a real snapshot of the
        // worktree exactly as it stands right now, so "what has this agent changed" has a real
        // base point to be measured against. Hooked here, at `Agents::spawn`'s caller rather than
        // inside `Agents` itself, for the same reason `load_diff` is triggered by its caller:
        // `Agents` owns processes and tabs, not git snapshots. See
        // `crate::review::flow::AdeApp::capture_review_baseline` for the small, accepted race
        // between the process starting and the snapshot landing.
        self.capture_review_baseline(id, cx);
        // A new tab changes this worktree's real tab session - see `crate::work_surface::session`.
        self.record_worktree_session(cx);
        // A second agent in this worktree closes the single-agent gate on every agent already
        // there, so a review tab open for one of them must really close now - see
        // `crate::review::render::AdeApp::close_gated_review_tab` for why this is a real close
        // rather than just dropping the tab from the strip.
        self.close_gated_review_tab(window, cx);
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
        if !self.centre_pane_is_not_an_agent() {
            self.agents.focus_active(window, cx);
        }
    }

    pub(crate) fn handle_new_agent_action(
        &mut self,
        _action: &NewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent(ProcessKind::Shell, window, cx);
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
        // GitHub issue #225: the review tab occupies the centre pane exactly as the graph tab
        // does, so it needs the identical teardown here. Without this, `review_tab_active` stayed
        // set, `render_center_pane` kept returning the review body, and the tab this call is
        // switching *to* never mounted at all - while real focus had already moved onto it. Found
        // by an adversarial audit; the review surface's own docs claimed to copy the graph tab's
        // discipline and, in exactly this way, did not.
        self.leave_review_tab(window, cx);
        // GitHub issue #227: the run-transcript tab occupies the centre pane exactly as the
        // graph and review tabs do, so it needs the identical teardown - see
        // `crate::run_history::tab::AdeApp::leave_run_tab`.
        self.leave_run_tab(window, cx);

        let had_open_file_tab = self.open_change.is_some();
        if had_open_file_tab {
            self.open_change = None;
            self.refresh_open_diff_file_cache();
            self.dismiss_hover();
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
                let fallback = self.focus_fallback_handle();
                restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
            }
        }
        let cwd = self
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| agent.cwd.clone());
        if let Some(cwd) = cwd {
            // `crate::root::AdeApp::select_worktree_by_path`, not a plain `self.worktrees`
            // lookup: this agent may belong to a worktree in a repo that isn't the focused one
            // at all (the rail's own agent rows fold in every repo's agents, not just the
            // focused repo's - `Self::build_agent_rows`'s own docs) - a real, reported bug:
            // clicking such an agent set it globally active (`Agents::set_active` above) but
            // never switched repos, so `Self::current_worktree_path` kept resolving to whatever the
            // *focused* repo's own selection was, `Self::combined_tab_order` built the tab strip
            // from that wrong cwd, and it came up with zero tabs - visibly "the tab bar doesn't
            // appear" - even though the agent genuinely was active underneath.
            // `select_worktree_by_path` already does the real cross-repo checkout when needed;
            // its own no-op-when-nothing-to-select guard is why the "already the right
            // worktree" check happens first here, unchanged from before - a same-worktree agent
            // switch (clicking between two terminals already open here) must stay cheap, with no
            // worktree-switch reset at all.
            let already_selected = self
                .selected
                .and_then(|index| self.worktrees.get(index))
                .is_some_and(|item| item.path == cwd);
            // A real regression this exact fix introduced and an adversarial test caught: `cwd`
            // is only a real worktree row when *some* repo's own list actually contains it - an
            // agent rooted directly at a repo with no git worktrees at all (a plain shell in a
            // non-git or bare directory, exactly what this file's own tests use) has no such
            // row anywhere, and `select_worktree_by_path` is correctly a no-op for a path it
            // can't find. Unconditionally delegating to it and returning early - what this looked
            // like right after the cross-repo fix - silently skipped everything below for that
            // case too, including the real keyboard-focus restore GitHub issue #112 exists for.
            // Checking findability first keeps that fallback reachable exactly as before.
            let findable = !already_selected
                && (self.worktrees.iter().any(|item| item.path == cwd)
                    || self
                        .repos
                        .iter()
                        .any(|repo| repo.worktrees.iter().any(|item| item.path == cwd)));
            if findable {
                self.select_worktree_by_path(&cwd, window, cx);
                return;
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
        // GitHub issue #239: the second, structural signal - what the process said about itself
        // through its own terminal (title glyph, OSC 9/777 notification, OSC 9;4 progress),
        // rather than what its silence implies. Gathered for every kind and gated inside
        // `derive_status`, which consults it only for a real agent session: a shell's title can
        // say anything and must never be able to fake agent-ness (see that module's docs).
        let terminal = status::TerminalSignal {
            title: pane.title().map(title_signal::classify_title),
            attention_pinged: pane.has_pending_attention_ping(),
            progress: pane.progress(),
        };
        // GitHub issue #225: what makes an exited agent "review ready" is now whether *it* has a
        // real, unreviewed diff against *its own* baseline - not whether its worktree's branch
        // differs from the default branch, which is a different question and was producing a
        // genuinely wrong answer: an agent that changed nothing, in a worktree whose branch had
        // already diverged from `main`, was reported `Review ready` off the back of the branch's
        // diff. `derive_status` itself is unchanged - only the fact fed into it is.
        //
        // Also carries the single-agent gate (see `Self::review_available_for`): in a worktree
        // with more than one open agent this is always `false`, so no agent there claims review
        // readiness it can't honestly substantiate. Such an agent lands on `Idle` instead, which
        // is the same state it would show with an empty review.
        // GitHub issue #239 phase 2: the third and strongest signal - what the agent reported
        // about itself through Claude Code's hook side-channel. `Default` (no fact) for every
        // agent that has never fired one, which is every Codex agent, every shell, and every
        // Claude agent whose first hook hasn't arrived yet - all of which therefore land on
        // exactly the Phase 1 behaviour above.
        let hooks = match &self.hook_runtime {
            Some(runtime) => runtime.signal_for(agent.id),
            None => status::HookSignal::default(),
        };
        let has_unreviewed_changes = self.agent_has_unreviewed_changes(agent.id);
        status::derive_status(agent.kind, signal, terminal, hooks, has_unreviewed_changes)
    }

    /// The `Archive` action - closes the tab via [`Self::close_agent`] (see that method's docs
    /// for why every close path must go through it rather than `Agents::close` directly).
    ///
    /// Its homes since GitHub issue #295 deleted the agent context bar's own copy
    /// (`STAGE-A-CHANGELOG.md` §4e: "`Archive` is worktree lifecycle and belongs on the rail row
    /// with prune and delete"): the rail's agent (`Archive run`) and worktree (`Archive N
    /// agents`) context menus (`crate::rail::menu`, issue #290), and the title bar's
    /// `Agent → Archive Agent`.
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
    /// onto whichever agent becomes active) and, if `id` is the agent [`Self::merge_flow`] is
    /// running under, cleans that up too (see [`Self::clear_merge_flow_for_closed_agent`]).
    ///
    /// Every close path - [`Self::archive_agent`], [`Self::respawn_agent`]'s
    /// close-then-respawn, and the tab strip's own `×` - must go through this function rather
    /// than `Agents::close` directly: previously only `archive_agent` cleared `merge_flow`, so
    /// archiving (or retrying) a mid-merge agent left `merge_flow.agent_id` pointing at an agent
    /// that no longer existed, permanently blocking every later merge (only one flow runs at a
    /// time - `crate::merge::flow::AdeApp::start_merge_from_graph_branch`'s own single-flight
    /// refusal).
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
        // A non-agent surface occupies the centre pane instead of an agent's own `TerminalPane`
        // while it is active, so `Agents::close`'s own focus-follows-close move onto the newly
        // active agent's pane would dangle. See [`Self::centre_pane_is_not_an_agent`] - the one
        // shared predicate every such site now reads.
        let skip_focus_move = self.centre_pane_is_not_an_agent();
        // GitHub issue #227: record that this run really ended - its transcript, its ending and
        // its diffstat - *before* anything below tears down the two things that measurement needs
        // (the pane's grid, and the review baseline ref released two lines down). See
        // `crate::run_history::flow`'s own module docs on why this moment and no other.
        self.finish_run_record(id, cx);
        // GitHub issue #225: close this agent's review tab (if it's the one open) and release its
        // baseline ref, *before* `Agents::close` removes the agent - `release_review_baseline`
        // needs to still be able to look up which worktree to run `git update-ref -d` in. The
        // persisted metadata entry deliberately survives; see that method's own docs.
        if self.review_tab_open == Some(id) {
            self.close_review_tab(window, cx);
        }
        self.release_review_baseline(id, cx);
        // GitHub issue #239 phase 2: drop this agent's live hook facts. Agent ids are handed out
        // by a monotonic counter so they are not reused today, but a stale entry keeping a dead
        // agent's status alive in the inbox would be a real bug the moment that ever changed -
        // and there is no reason to keep facts about a pane that no longer exists. The *persisted*
        // record deliberately survives, exactly like the review baseline immediately above:
        // that closed agent is what GitHub issue #227 exists to show.
        if let Some(runtime) = &self.hook_runtime {
            runtime.forget(id);
        }
        self.agents.close(id, skip_focus_move, window, cx);
        if self
            .merge_flow
            .as_ref()
            .is_some_and(|flow| flow.agent_id == id)
        {
            self.clear_merge_flow_for_closed_agent(cx);
        }
        if self.agents.active_id().is_none() && !self.centre_pane_is_not_an_agent() {
            window.focus(&self.rail_focus_handle, cx);
        }
        // A closed tab is as real a session change as an opened one: relaunching must not reopen
        // a tab the user deliberately closed. See `crate::work_surface::session`.
        self.record_worktree_session(cx);
    }

    /// Whether some surface *other than an agent's own pane* currently occupies the centre column.
    ///
    /// The one shared predicate `REVISION-2026-08-13.md` §7 asks for, in this app's own terms.
    /// The design states the rule as a deny-list on "is this tab a file"
    /// (`isFileTab = !isAgent(tab) && tab !== 'terminal' && tab !== 'graph' && tab !== 'run'`) and
    /// spells out exactly what it costs to forget one: "without the last clause the editor renders
    /// *below* the new pane and the tab id is pushed into the worktree's open-file list as a
    /// phantom file tab". Jerry's equivalent question is this one - every real site asked it as an
    /// inline `open_change.is_some() || settings_open || graph_tab_active`, once per site, and one
    /// of them had already drifted (the review tab was missing from `close_agent`'s own
    /// `skip_focus_move`, so closing an agent while the Review tab was showing moved real keyboard
    /// focus onto a pane nothing was drawing).
    ///
    /// **Extend this, and only this, when a centre surface is added.** Every caller is a place
    /// that would otherwise focus, close or render an agent pane that is not on screen.
    ///
    /// Jerry's other half of §7 - "the tab id can't leak into the open-file list" - needs no
    /// predicate at all: [`work_surface::TabRef`] is an enum, its file arm carries a `PathBuf`,
    /// and the open-file list is `Vec<PathBuf>`, so a run tab is structurally incapable of landing
    /// in it.
    pub(crate) fn centre_pane_is_not_an_agent(&self) -> bool {
        self.open_change.is_some()
            || self.settings_open
            || self.graph_tab_active
            || self.review_tab_active
            || self.run_tab_active
    }

    /// The rail agent menu's `Pause` action - sends `Ctrl-C` to the agent's pty via
    /// `TerminalPane::interrupt`. The pane strip's own `Interrupt` button is gone (GitHub issue
    /// #295 / §4t: "the pane is a terminal: `⌃C` already interrupts ... a button duplicating
    /// a keystroke that works in the focused surface" is unearned space).
    pub(crate) fn interrupt_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let pane = agent.pane.clone();
        pane.update(cx, |pane, cx| pane.interrupt(cx));
    }

    /// The pane strip's `Retry ⌘R` (failed agents) / `Resume ⌘⏎` (idle agents) action, and the
    /// rail agent menu's `Resume` row - the two verbs GitHub issue #295 left on that strip.
    /// This app has no saved-agent resumability to resume *from* (see
    /// `crate::work_surface::state::pty_state_label`'s docs), so the honest equivalent is: close this
    /// tab, then spawn a fresh agent of the same kind into the same worktree - not literally
    /// "resume where it left off" (`crate::work_surface::state::ActionKind::Respawn`'s docs name this
    /// trade-off).
    pub(crate) fn respawn_agent(
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
        // A respawned agent is a freshly spawned one in every other respect, so it gets the same
        // real hook injection - otherwise "Retry" would silently produce an agent whose status
        // fell back to the quiescence heuristic.
        let hook_injection = self.hook_injection_for(kind);
        let respawned = self.agents.spawn(
            kind,
            cwd,
            self.settings.appearance.terminal_font_size,
            self.settings.terminal.shell_override(),
            hook_injection.as_ref(),
            window,
            cx,
        );
        // ...and a freshly spawned agent's other half: its own review baseline. The close above
        // has already released the *previous* agent's ref (`release_review_baseline`), so without
        // this a retried agent could never have a review at all - a real gap found while
        // verifying GitHub issue #381 against the running app, in the same "an agent-only
        // capability quietly isn't there" family as that issue's own findings. A no-op for a
        // `Shell`, like every other call to it.
        self.capture_review_baseline(respawned, cx);
        self.focus_newly_spawned_agent(window, cx);
        // The close above and the spawn here are two real session changes; both are recorded, so a
        // relaunch reopens the retried agent's slot rather than the one it replaced.
        self.record_worktree_session(cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// The no-agent empty state's `Open terminal` action
    /// ([`Self::render_no_agents_empty_state`]) - selects an already-open `Shell` agent in the
    /// same worktree, or spawns one if none exists. §4e keeps this verb there and nowhere else
    /// in the pane: everywhere else it "was always a duplicate of the `zsh` tab three rows
    /// above". Each agent is its own independent tab/
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
            .find(|agent| agent.kind == ProcessKind::Shell && agent.cwd == cwd)
            .map(|agent| agent.id);
        match existing {
            Some(id) => self.select_agent(id, window, cx),
            None => {
                self.agents.spawn(
                    ProcessKind::Shell,
                    cwd,
                    self.settings.appearance.terminal_font_size,
                    self.settings.terminal.shell_override(),
                    None,
                    window,
                    cx,
                );
                self.focus_newly_spawned_agent(window, cx);
                // Only this arm spawned anything - the `Some(id)` arm above just selects a
                // terminal that is already part of the recorded session.
                self.record_worktree_session(cx);
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
    /// never meant to depend on a shell/agent existing. Bareness no longer affects tab *labels*
    /// either (those are the pane's own live title now - `Self::agent_tab_label`); it survives
    /// only in [`Self::render_agent_context_bar`]'s `Start an agent` swap.
    pub(crate) fn combined_tab_order(&self) -> Vec<work_surface::TabRef> {
        // No worktree genuinely selected means genuinely no tabs - an honestly empty strip, not
        // whatever happens to be open in the repo root. This used to fall through to
        // `Self::current_worktree_path`'s repo-root fallback, which is how a live terminal could be
        // drawn in the strip while the centre pane showed nothing and no rail row claimed it (see
        // that method's own docs for the live repro).
        let Some(cwd) = self.current_worktree_path() else {
            return Vec::new();
        };
        let agents_for_cwd: Vec<&Agent> = self.agents.iter_for_cwd(cwd.clone()).collect();
        let agent_ids: Vec<AgentId> = agents_for_cwd.iter().map(|agent| agent.id).collect();
        // A worktree with no [`Self::tab_order`] entry reconciles against an empty slice, which is
        // the old, deliberate two-block default ("every agent, then every file" - see
        // `work_surface::state::reconcile_tab_order`'s own docs).
        //
        // This method used to read a *file-only* persisted order (`TabOrderState::file_order`)
        // here instead, as GitHub issue #16's own "restores on relaunch". That fallback is gone,
        // replaced rather than dropped: `crate::work_surface::session::AdeApp::
        // restore_worktree_session` now seeds `Self::tab_order` with the real remembered order
        // directly, at the one moment a worktree is genuinely activated - and does it for *every*
        // tab kind, agents included, which a fallback keyed off persisted file paths structurally
        // could not. Keeping both would have been actively wrong, not merely redundant: since a
        // worktree's session is now recorded on every ordinary tab change rather than only after a
        // drag, this fallback would fire for never-dragged worktrees too and silently reorder
        // their strip to "files first, then agents" - the exact inversion of the documented
        // default.
        let stored: &[work_surface::TabRef] = match self.tab_order.get(&cwd) {
            Some(order) => order.as_slice(),
            None => &[],
        };
        work_surface::reconcile_tab_order(
            stored,
            &agent_ids,
            self.open_files(),
            self.graph_tab_open,
            self.review_tab_open,
            self.run_tab_by_worktree.contains_key(&cwd),
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
        // A drag can only ever have started from a tab that was genuinely rendered, which means a
        // real worktree is selected - this refusal is defensive, not a reachable path.
        let Some(cwd) = self.current_worktree_path() else {
            return;
        };
        let mut order = self.combined_tab_order();
        work_surface::move_tab_order(&mut order, &dragged, &target, insert_after);
        self.tab_order.insert(cwd.clone(), order);

        // The same order, persisted to disk (GitHub issue #16, widened by the tab-session restore
        // work into "every tab, of every kind, in this order" - see
        // `crate::work_surface::session`). Deliberately the one shared recorder rather than a
        // second, drag-specific encoding: a drag is just one more way this worktree's tab session
        // changes, and having it write the file through a different path than every other change
        // is exactly how the two would drift.
        self.record_worktree_session(cx);
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
    ///
    /// Also records [`Self::tab_slide`] (task #65, GitHub issue #16's own remaining "neighbour
    /// tabs teleport instead of sliding" gap) *before* [`Self::reorder_tab`] actually mutates the
    /// order - `work_surface::state::tab_slide_offsets` needs the real pre-drop order to know
    /// which tabs are about to shift, and the dragged tab's own real last-measured width
    /// ([`Self::tab_bounds`]) to know by how much. `dragged_width` is `0px`
    /// ([`Pixels::default`]) whenever that tab has never actually painted yet (e.g. a test that
    /// never rendered a real window) - a real, honestly-measured `0` rather than an invented
    /// placeholder width, so the recorded tabs are still correct even though nothing would
    /// visibly move.
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

        let old_order = self.combined_tab_order();
        let dragged_width = self
            .tab_bounds
            .get(&dragged)
            .map(|bounds| bounds.size.width)
            .unwrap_or_default();
        let slide_id = self.next_tab_settle_id;
        self.tab_slide = work_surface::tab_slide_offsets(
            &old_order,
            &dragged,
            &target,
            insert_after,
            dragged_width,
        )
        .into_iter()
        .map(|(tab_ref, offset)| (tab_ref, (offset, slide_id)))
        .collect();

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

    /// Every agent open in the *currently selected* worktree (`Self::current_worktree_path`), in
    /// the same order [`Self::combined_tab_order`] renders them - never Agents' own raw
    /// creation order once a real drag has interleaved them differently, and never every agent
    /// across every worktree, per this revision's whole point (see `crate::root::mod`'s "One
    /// rail row per worktree" docs). The real per-worktree tab-strip order
    /// [`Self::render_tab_strip`] draws from, so the tabs shown and this list can never disagree.
    ///
    /// **Every** pane tab, shells included - this is "what does the strip show", and a
    /// [`ProcessKind::Shell`] genuinely gets a tab. Anything that means "a real agent session"
    /// rather than "a pane" wants [`Self::current_worktree_agent_sessions`] instead.
    pub(crate) fn current_worktree_agents(&self) -> impl Iterator<Item = &Agent> {
        let order = self.combined_tab_order();
        order.into_iter().filter_map(move |tab_ref| match tab_ref {
            work_surface::TabRef::Agent(id) => self.agents.iter().find(|agent| agent.id == id),
            work_surface::TabRef::File(_)
            | work_surface::TabRef::Graph
            | work_surface::TabRef::Review(_)
            | work_surface::TabRef::Run => None,
        })
    }

    /// [`Self::current_worktree_agents`] narrowed to **real agent sessions**
    /// (`ProcessKind::is_agent_session`) - the list every surface that says the word *agent* to
    /// the user counts and indexes by: [`Self::agent_jump_keys`]/[`Self::jump_to_agent_at`] (the
    /// `secondary-1`..`secondary-8` keycaps and the status bar's copy of them),
    /// [`Self::select_relative_agent`] (the title bar's `Next Agent`/`Previous Agent`), and that
    /// pair's own menu-enablement predicate.
    ///
    /// Same order as the strip, just with the shells dropped, so `secondary-2` still means "the
    /// second agent you can see, reading left to right" - it simply stops counting the terminal
    /// sitting between them. GitHub issue #381: `Jerry.dc.html` has never treated a terminal as
    /// an agent (`isAgent = t => activeWt.agents.indexOf(t) >= 0`, with `'terminal'` a separate
    /// tab id its `isFileTab` test excludes by name), and the design's own keybinding table calls
    /// this `Jump to session by position`. Counting shells made the keycap row advertise more
    /// positions than there were agents and pushed every real agent off its own number.
    ///
    /// A shell is not made unreachable by this: it still has a real tab to click, and - since
    /// this same issue stopped the palette filing shells under a heading that reads `Agents` - it
    /// now has its own `Terminals` group there, which is a full keyboard path to it
    /// (`crate::palette::state::build_groups`).
    pub(crate) fn current_worktree_agent_sessions(&self) -> impl Iterator<Item = &Agent> {
        self.current_worktree_agents()
            .filter(|agent| agent.kind.is_agent_session())
    }

    /// Whether the *currently selected* worktree has zero real agents - at most a default
    /// `Shell` tab (Revision R12 §3: "a bare worktree shows only the shell tab"). Now read by
    /// exactly one thing, [`Self::render_agent_context_bar`]'s `Merge`/`Archive` ->
    /// `Start an agent` swap: bareness used to also pick the shell tab's *label*
    /// (`zsh \u{b7} <branch>` instead of a generic `"terminal"`), which a tab now takes from its
    /// pane's own live title regardless of what else is open in the worktree
    /// ([`Self::agent_tab_label`]). Vacuously `true` when the worktree has no agent at all -
    /// callers that reach a context bar already know at least one agent exists
    /// ([`Self::render_center_pane`]'s `None` branch handles the empty case separately).
    pub(crate) fn current_worktree_is_bare(&self) -> bool {
        !self
            .current_worktree_agents()
            .any(|agent| agent.kind.is_agent_session())
    }

    /// The branch label for the currently selected worktree, if any is recorded for it - shared
    /// by [`Self::render_plus_menu`]'s `runs in <branch>` row and
    /// [`Self::render_agent_context_bar`]'s own branch lookup so both read the same fact the
    /// same way. Tab labels deliberately no longer consult it: a branch is a fact about the
    /// worktree, not about what the process in a given tab is doing right now.
    fn current_worktree_branch(&self) -> Option<String> {
        let cwd = self.current_worktree_path()?;
        self.worktrees
            .iter()
            .find(|item| item.path == cwd)
            .and_then(|item| item.branch.clone())
    }

    /// One agent/terminal tab's label: whatever the process inside that pane says it is *right
    /// now* - its live OSC 0/2 window title (`TerminalPane::title`, the same real fact
    /// `crate::rail::title_signal` classifies for the status pill), falling back to the resolved
    /// program name only while it has set no title at all
    /// (`work_surface::live_tab_label`'s own docs for both halves).
    ///
    /// Read fresh on every render, deliberately: the title is live, so a tab whose shell just
    /// `cd`'d or whose agent just started a task must relabel itself without any further event
    /// plumbing. GPUI makes that automatic - the `pane.read(cx)` below registers this window as a
    /// reader of the pane entity, so the pane's own `cx.notify()` (which it fires whenever real
    /// pty bytes arrive, and a title change *is* pty bytes) invalidates the window and redraws
    /// this strip.
    ///
    /// Applies identically to a shell tab and an agent-CLI tab. Both are just a process in a pty,
    /// both report through the same mechanism, and an agent's own title is the more informative
    /// of the two (`\u{2733} Claude Code` while it works) - there is no per-kind naming authority
    /// that would be regressed by preferring it. The one deliberate exception elsewhere is
    /// `crate::review::render`'s review-tab label, which answers a different question (*which*
    /// agent's diff this is) and so must stay a stable identity, not a live status.
    pub(crate) fn agent_tab_label(&self, agent: &Agent, cx: &App) -> String {
        let pane = agent.pane.read(cx);
        work_surface::live_tab_label(pane.title(), &pane.program_label())
    }

    /// The tab strip: one tab per entry of [`Self::combined_tab_order`], in that exact order -
    /// [`Self::render_agent_tab`] for a `TabRef::Agent`, [`Self::render_file_tab`] for a
    /// `TabRef::File` - so an agent tab and a file tab can sit side by side in either order
    /// (GitHub issue #16), rather than always "every agent, then every file" - followed by the
    /// `+` menu button ([`Self::render_tab_strip_plus`]) and right-aligned agent-jump keycaps.
    ///
    /// Each agent tab's label is read per tab, inside the loop below
    /// (`Self::agent_tab_label`): a label is now purely a fact about that one pane's own live
    /// title, so nothing here needs every label in hand up front. An earlier revision did need
    /// that - it appended `#1`/`#2` ordinals to labels that repeated, which requires knowing
    /// which ones repeat - but two tabs genuinely showing the same thing now honestly render the
    /// same label, exactly as any real terminal emulator's tabs do.
    pub(in crate::work_surface) fn render_tab_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // No `border_b` here, deliberately: the *children* own this column's bottom edge. GitHub
        // issue #291 / `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4v, verbatim -
        // "the centre column drew its bottom edge **twice** - once on the tab-strip container and
        // once on every tab - so under an inactive tab the rule was 1.6px of two shades stacked,
        // and the active tab's cut-out (its `border-bottom` set to its own background, so it joins
        // the pane below) was defeated by the container's line drawing straight through beneath
        // it. ... A child cannot paint over its parent's border - the parent's border sits outside
        // the child's box - so the container cannot own the edge if any child needs to cut it.
        // **The tabs own it.**" Every child below therefore carries the rule itself, the `+`, the
        // spacer and the counter cluster included, "without which the rule stopped at the last tab
        // and 398px of the window's top edge was simply missing".
        let mut bar = div()
            .id("tab-strip")
            // Lets a real test measure this column header's own painted box - §4v's
            // "column headers that share a y are one rule, not three" is only checkable against
            // all three of them at once (`crate::rail::strip_render`'s own chrome test).
            .debug_selector(|| "tab-strip".to_string())
            .flex()
            .flex_none()
            .items_stretch()
            .h(theme::band::CHROME_HEADER)
            .bg(theme::surface::TITLE_BAR);

        let order = self.combined_tab_order();

        for tab_ref in order {
            match tab_ref {
                work_surface::TabRef::Agent(id) => {
                    if let Some(agent) = self.agents.iter().find(|agent| agent.id == id) {
                        let label = self.agent_tab_label(agent, cx);
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
                // GitHub issue #225: the agent review tab, a full member of the same combined,
                // draggable order every other kind already goes through.
                work_surface::TabRef::Review(id) => {
                    bar = bar.child(crate::review::render::render_review_tab(self, id, cx));
                }
                // GitHub issue #227: the run-transcript tab, a full member of the same combined,
                // draggable order - one per worktree, replaced rather than stacked.
                work_surface::TabRef::Run => {
                    bar = bar.child(crate::run_history::tab::render_run_tab(self, cx));
                }
            }
        }

        bar = bar.child(self.render_tab_strip_plus(cx));

        let jump_keys = self.agent_jump_keys();

        bar.child(
            // The spacer carries the column rule too - see the container's own comment above.
            div()
                .flex_1()
                .border_b_1()
                .border_color(theme::border::RAIL_INNER),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .border_b_1()
                .border_color(theme::border::RAIL_INNER)
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

    /// The real `secondary-1`..`secondary-8` agent-jump keycap labels: one per **real agent
    /// session** open in the *currently selected* worktree
    /// ([`Self::current_worktree_agent_sessions`] - a plain shell is not an agent and gets no
    /// number, see that method's docs), capped at 8 since those are the only ones actually bound
    /// (`crate::default_key_bindings`) - never a keycap advertising a shortcut that silently does
    /// nothing, and (GitHub issue #381) never one more keycap than there are agents for them to
    /// land on. Shared by [`Self::render_tab_strip`]'s own right-aligned cluster and the status
    /// bar's agent hint (`status_bar::render::render_status_agent_hint`), so the two can never
    /// independently drift on what's really bound.
    pub(crate) fn agent_jump_keys(&self) -> Vec<String> {
        let agent_count = self.current_worktree_agent_sessions().count().min(8);
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
    /// close, the drop settle-fade (GitHub issue #16 §5), and the neighbour-slide animation for
    /// every *other* tab a drop shifted (task #65). Every real per-kind visual (chip, label,
    /// dirty/status dot, the close button itself) is still supplied by the caller as
    /// `args.content`, in the order it should render - only the chrome around it is shared now,
    /// which is what makes the exact bug class GitHub issue #96 was structurally impossible:
    /// there is now exactly one place that wires drag/close/settle-fade/slide for every tab kind
    /// (agent, file, graph, review), not four.
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
        let slide = self.tab_slide.get(&tab_ref).copied();
        let outer_id_for_slide = outer_id.clone();
        let this_entity = cx.entity();
        let this_entity_for_bounds = this_entity.clone();
        let tab_ref_for_drag = tab_ref.clone();
        let tab_ref_for_drag_move = tab_ref.clone();
        let tab_ref_for_bounds = tab_ref.clone();
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
            .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline))
            // Captures this tab's own painted bounds into `Self::tab_bounds` every render - the
            // same `gpui::canvas` idiom `Self::plus_button_bounds`
            // already uses. The only real source of a tab's on-screen width (GPUI's flex layout
            // means no two tabs are the same size), which `Self::drop_dragged_tab` reads for
            // whichever tab is dragged next, to compute how far a drop's shifted neighbours must
            // slide (`work_surface::state::tab_slide_offsets`'s own docs on why only the
            // *dragged* tab's own width is ever needed).
            .child({
                let this = this_entity_for_bounds;
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.tab_bounds.insert(tab_ref_for_bounds.clone(), bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            });

        // A real drop's own settle-in fade (GitHub issue #16's "dropping animates the tab
        // settling into its slot") - see `tab_settle_animation_id`'s own docs for why a fresh id
        // is required, and why this branches to `gpui::AnyElement` rather than a plain
        // `.when_some` (`gpui::AnimationExt::with_animation` returns a different wrapper type,
        // not `Self`). Mutually exclusive with the neighbour-slide branch below: the dropped tab
        // itself is never in `Self::tab_slide` (`work_surface::state::tab_slide_offsets`'s own
        // docs), so a tab is never asked to fade and slide at once.
        match (settle_animation_id, slide) {
            (Some(id), _) => tab_div
                .with_animation(
                    id,
                    Animation::new(TAB_SETTLE_ANIMATION_DURATION),
                    |el, delta| el.opacity(0.55 + 0.45 * delta),
                )
                .into_any_element(),
            // The remaining GitHub issue #16 gap this closes (task #65): a tab whose slot shifted
            // as a side effect of a drop slides from its own real pre-drop position (`offset`)
            // down to `0` - its already-correct new position - rather than teleporting there.
            // `.left()`, not `.opacity()`, and `tab_div` stays `position: relative` (never
            // `.absolute()`): a `position: relative` element's `inset`/`left` is a pure paint-time
            // offset from its own normal flex slot (`taffy`, this app's real flex layout engine -
            // see `vendor/zed/crates/gpui/src/taffy.rs`'s `inset` translation, and `taffy` itself:
            // "Offset is the relative position from the item's natural flow position ... Does not
            // include margin/padding/border") - so every *other* tab's own flex position is
            // completely unaffected by this tab's temporary offset, exactly like the dropped
            // tab's own opacity dim above never shifts its neighbours. The animation id mixes in
            // this tab's own `outer_id` (unlike the settle-fade above, more than one tab can slide
            // from the same drop at once, so the batch id alone would collide across siblings) and
            // `slide_id` (the same "GPUI keys animation progress purely by id string, so two
            // different drops must never share an id" reason `tab_settle_animation_id`'s own docs
            // give).
            (None, Some((offset, slide_id))) => tab_div
                .with_animation(
                    format!("tab-slide-{outer_id_for_slide}-{slide_id}"),
                    Animation::new(TAB_SLIDE_ANIMATION_DURATION),
                    move |el, delta| el.left(offset * (1.0 - delta)),
                )
                .into_any_element(),
            (None, None) => tab_div.into_any_element(),
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
            // A child of the tab strip, so it carries the column rule - see
            // `Self::render_tab_strip`'s own comment for why the container no longer does.
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
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
                let opening = !this.plus_menu_open;
                // GitHub issue #176: opening this menu closes whatever other menu was open, so
                // two popovers can never be painted at once. Read *before* the sweep and applied
                // after it, because the sweep clears `plus_menu_open` itself.
                let _ = this.close_menu_surfaces_except(Some(menus::MenuSurface::Plus));
                this.plus_menu_open = opening;
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
    /// ([`Self::new_agent`] with [`ProcessKind::Shell`]), *New agent* (`runs in <branch>` -
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
        // The tab strip's own `+` is the only control that opens this popover, so its painted
        // bounds are the only anchor - see `Self::plus_button_bounds`'s own docs for the rail
        // per-repo anchor that used to exist here and why it's gone.
        let bounds = self.plus_button_bounds;

        let resolved_kind = ProcessKind::from(self.resolved_new_agent_kind());
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
                            this.new_agent(ProcessKind::Shell, window, cx);
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
    ///
    /// Returns the narrow [`AgentKind`], not [`ProcessKind`]: the "New agent" row can only ever
    /// resolve to a real agent CLI, so "could this be a shell?" isn't a question a caller has to
    /// answer - the type says it can't be.
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
    pub(crate) fn new_agent_pane(&mut self, cx: &mut Context<Self>) {
        // GitHub issue #90: the same real "nothing to spawn into yet" guard [`Self::new_agent`]'s
        // own docs explain - see those for the concrete bug this closes.
        if self.focused_repo().is_none() {
            return;
        }
        // The identical "no worktree selected means nothing legitimate to spawn into" refusal
        // [`Self::new_agent`] applies - see its own docs.
        let Some(cwd) = self.current_worktree_path() else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move {
                    settings::AGENT_KINDS
                        .into_iter()
                        .find(|kind| pty_core::resolve_on_path(kind.binary_name()).is_some())
                })
                .await;
            // Needs `Window` access to move focus onto the newly spawned agent's pane
            // (`Self::focus_newly_spawned_agent`) - `Entity::update_in` provides it.
            let _ = this.update_in(cx, |this, window, cx| {
                let kind = installed.unwrap_or(settings::AGENT_KINDS[0]);
                let hook_injection = this.hook_injection_for(ProcessKind::Agent(kind));
                let id = this.agents.spawn(
                    ProcessKind::Agent(kind),
                    cwd,
                    this.settings.appearance.terminal_font_size,
                    this.settings.terminal.shell_override(),
                    hook_injection.as_ref(),
                    window,
                    cx,
                );
                // See `Self::new_agent`'s own identical call. Missing here until GitHub issue
                // #381's live verification tripped over it: this door (`ctrl-shift-N`, the title
                // bar's `New Agent Pane` row, and the empty pane's own `Start an agent` CTA) is
                // how most agents in this app are actually started, and every one of them was
                // spawned without a review baseline - so the whole #225 review surface could
                // never open for it, no matter how many agents shared the worktree.
                this.capture_review_baseline(id, cx);
                this.focus_newly_spawned_agent(window, cx);
                // See `Self::new_agent`'s own identical call - this is the same real new tab,
                // reached through the `+` menu's background `$PATH` search instead.
                this.record_worktree_session(cx);
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

    /// The tab strip's agent-jump keycaps (`secondary-1`..`secondary-8`) - jumps to the **real
    /// agent session** at 1-indexed `position` in the same order [`Self::render_tab_strip`]
    /// iterates ([`Self::current_worktree_agent_sessions`]), via [`Self::select_agent`]. No-op if
    /// fewer than `position` agent sessions are currently open in the selected worktree.
    ///
    /// GitHub issue #381: a plain [`ProcessKind::Shell`] does not take a number. It used to, and
    /// the position it consumed was the user's own live report - the *worktree's startup shell*
    /// occupies position 1 in essentially every worktree, so `secondary-1` selected a terminal
    /// and every real agent sat one place further along than the keycap beside it claimed. See
    /// [`Self::current_worktree_agent_sessions`] for why the design agrees.
    pub(crate) fn jump_to_agent_at(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = position
            .checked_sub(1)
            .and_then(|index| self.current_worktree_agent_sessions().nth(index))
            .map(|agent| agent.id)
        else {
            return;
        };
        self.select_agent(id, window, cx);
    }

    /// The Windows/Linux title bar's Agent menu "Next agent"/"Previous agent" rows
    /// (`crate::title_bar::menu::AdeApp::render_title_menu`) - `delta` is `1`/`-1`. Cycles
    /// through [`Self::current_worktree_agent_sessions`] in the same order
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
    /// over the existing per-worktree list. No-op with no agent session in the selected worktree
    /// at all, or with no active agent at all (both real, reachable states - the latter only
    /// while every agent has been closed).
    ///
    /// GitHub issue #381: a plain [`ProcessKind::Shell`] is not a stop on this cycle. A row that
    /// says `Next Agent` and lands on a terminal is the same conflation the jump keycaps had, and
    /// this one bit harder in practice - the startup shell sits in every worktree, so a two-tab
    /// worktree of `[shell, claude]` made `Next Agent` a toggle that spent half its presses
    /// leaving the only agent there was.
    ///
    /// **The active tab not being an agent session is a first-class case, not a no-op.** It is
    /// the *normal* state (a focused terminal), and `Next Agent` from it must mean something:
    /// there is no "current" position on the cycle to step from, so `delta > 0` enters at the
    /// first agent session and `delta < 0` at the last. Falling out through the old
    /// `position()?` here would have made the row silently dead in exactly the situation a user
    /// reaches for it.
    pub(crate) fn select_relative_agent(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<AgentId> = self
            .current_worktree_agent_sessions()
            .map(|s| s.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let Some(active_id) = self.agents.active_id() else {
            return;
        };
        let next_index = match ids.iter().position(|id| *id == active_id) {
            Some(current_index) => {
                if ids.len() < 2 {
                    // The one agent session here is already active - cycling has nowhere to go.
                    return;
                }
                let len = ids.len() as isize;
                (current_index as isize + delta).rem_euclid(len) as usize
            }
            // Active on a shell (or on some other pane that isn't an agent session at all):
            // enter the cycle from whichever end `delta` is heading towards.
            None if delta >= 0 => 0,
            None => ids.len() - 1,
        };
        self.select_agent(ids[next_index], window, cx);
    }

    /// [`NewTerminal`]'s `ctrl-shift-T` action handler - the `+` menu's "New terminal" row's own
    /// keybinding, spawning a [`ProcessKind::Shell`] agent like the row's click handler does.
    pub(crate) fn handle_new_terminal_action(
        &mut self,
        _action: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent(ProcessKind::Shell, window, cx);
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

    /// One tab: a 14×14 kind chip, `label` (this pane's own live title, resolved by the caller
    /// through [`Self::agent_tab_label`]), and a `×` that closes it
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
        let is_active = self.active_agent_pane_id() == Some(id);
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
                // The label is now whatever the process inside the pane set as its title
                // (`Self::agent_tab_label`), i.e. arbitrary-length text this app doesn't
                // control - a shell reporting a deep absolute path, or an agent echoing a long
                // task line, would otherwise stretch one tab across the whole strip and push
                // every other tab out of reach. Capped and ellipsised the same way the pty
                // header caps its own cwd (`Self::render_pty_header`), rather than shortening
                // the title itself: the tab shows as much of the real title as fits.
                .flex_none()
                .max_w(px(200.0))
                .overflow_hidden()
                .truncate()
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
    /// bar never wraps when the centre narrows), and a status pill. **Identity and status only**
    /// (GitHub issue #295, `STAGE-A-CHANGELOG.md` §4e): the `Merge` and `Archive` buttons that
    /// used to close this bar are deleted, not hidden.
    ///
    /// Both were **worktree** verbs gated only on "there is an agent", sitting in an **agent**
    /// header. §4e's three reasons, verbatim: a two-agent worktree "offered `Merge` **twice for
    /// one worktree**"; it was offered "while the agent was `Needs input`, blocked mid-run on a
    /// question about two conflicting migrations"; and "merge has preconditions - committed, base
    /// current, no live writers - and the header can show none of them". Their homes now:
    /// merging is the git graph's job (GitHub issue #241 - its "merge branch into base" half is
    /// still open, so that direction is genuinely unavailable until it lands), and archiving is
    /// the rail's agent/worktree context menus (`crate::rail::menu`, issue #290) plus the title
    /// bar's Agent menu.
    ///
    /// The one button that stays is a bare worktree's (Revision R12 §3:
    /// [`Self::current_worktree_is_bare`]) `Start an agent`, with the agent name greyed to
    /// `no agent` - it is not one of the removed worktree verbs, and §4e's own rule ("an action
    /// belongs in the scope of the object it acts on") puts starting an agent squarely in an
    /// agent surface.
    ///
    /// **Not rendered over a `Shell` tab in a worktree that does have agents** - see
    /// [`Self::render_center_pane`]'s `show_context_bar`, which reproduces `Jerry.dc.html`'s
    /// `showAgentBar: noAgents || activeWt.agents.indexOf(tab) >= 0` and the mock's own reason:
    /// "The whole row is agent identity, so it belongs to agent panes only… in a terminal pane
    /// there is no agent to describe. Kept when the worktree has no agents at all, since it holds
    /// that empty state's CTA."
    pub(in crate::work_surface) fn render_agent_context_bar(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.agent_status(agent, cx);
        let (agent_fg, agent_bg) = work_surface::agent_tint(agent.kind);
        let agent_initial = work_surface::agent_initial(agent.kind);
        let is_bare = self.current_worktree_is_bare();
        // `ProcessKind` only tracks which CLI binary is running, not which model it's
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

        let bar = div()
            .id("agent-context-bar")
            .debug_selector(|| "agent-context-bar".to_string())
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
            bar.child(self.render_start_agent_button("context-bar-start-agent", cx))
        } else {
            bar
        }
    }

    /// The `Start an agent` button - a filled blue button with a `mod+shift+N` keycap hint. Real,
    /// not decorative: dispatches [`Self::new_agent_pane`], the same entry point the tab strip's
    /// `+` menu row and its own global `secondary-shift-n` keybinding already use.
    ///
    /// Rendered in exactly two places, both of which genuinely have no agent to act on: a bare
    /// worktree's context bar (Revision R12 §3), where it replaced the deleted `Merge`/`Archive`
    /// pair; and the no-agent empty state ([`Self::render_no_agents_empty_state`]), which §4t
    /// keeps buttoned because "with no agent there is no keystroke to duplicate and no readout to
    /// show". `element_id` distinguishes the two so GPUI never sees one id painted twice.
    pub(in crate::work_surface) fn render_start_agent_button(
        &self,
        element_id: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = work_surface::action_button_colors(work_surface::ActionStyle::PrimaryBlue);
        let macos = self.window_controls_style().is_macos();
        let parts = keymap::resolve_combo("mod+shift+N", macos);

        div()
            .id(element_id)
            .debug_selector(move || element_id.to_string())
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
    /// An agent pane's pid rides **this header**, and a shell's rides its info footer - which is
    /// exactly where `Jerry.dc.html` puts each. The mock's two pane branches are mutually
    /// exclusive (`isChat: isAgent(tab)` vs `isTerminal: tab === 'terminal'`) and only the
    /// `isTerminal` one has a `pid`/`termSize` bottom bar; the `isChat` header reads
    /// `{{ focus.cli }} · pid {{ focus.pid }} … {{ focus.ptyHint }}`. See
    /// [`Self::render_pty_info_footer`] and [`Self::render_pty_footer`] for the bottom half of
    /// that same split.
    ///
    /// GitHub issue #20 moved `clear` into the **terminal's** info footer, alongside
    /// pid/grid-dims/env - see that method's own docs for the click entry point, and
    /// [`Self::handle_terminal_clear_action`] for the real, rebindable keybinding, which is
    /// scoped to a focused `TerminalPane` and so still works in an agent pane that no longer
    /// paints the hint.
    pub(in crate::work_surface) fn render_pty_header(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = agent.pane.read(cx);
        let program_label = pane.program_label();
        let is_running = pane.is_running();
        let pid = pane.pid();
        let exit_code = pane.exit_status().map(|status| status.exit_code());
        let status_value = self.agent_status(agent, cx);
        let state_label = work_surface::pty_state_label(is_running, status_value, exit_code);
        let is_wsl_shell = agent.kind == ProcessKind::Shell && env_info::is_wsl();
        let label_text = if is_wsl_shell {
            format!("{program_label} \u{b7} wsl")
        } else {
            program_label
        };

        let header = div()
            .id("pty-header")
            .debug_selector(|| "pty-header".to_string())
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
            ProcessKind::Shell => header.child(
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
            // An agent pane has no info footer under it (that bar is the terminal pane's, per
            // the mock's `isTerminal` branch), so its pid rides the header - `{{ focus.cli }}
            // pid {{ focus.pid }}` in the mock's `isChat` branch, in the same `#4a5057`
            // (`theme::text::PATH`) the terminal footer's own `pid` uses.
            ProcessKind::Agent(_) => header.children(pid.map(|pid| {
                div()
                    .id("pty-header-pid")
                    .debug_selector(|| "pty-header-pid".to_string())
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::PATH)
                    .child(format!("pid {pid}"))
            })),
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

    /// The **shell** pane's info footer: pid, grid dimensions, the environment chip, `clear`
    /// (GitHub issue #20 - moved here from the header, see [`Self::render_pty_header`]'s own
    /// docs), and a hint about file:line references.
    ///
    /// [`ProcessKind::Shell`] only, and mutually exclusive with [`Self::render_pty_footer`] - a
    /// pane gets one bottom bar, never both. `Jerry.dc.html`'s two pane branches are `sc-if`
    /// siblings on mutually exclusive conditions (`isTerminal: tab === 'terminal'` vs
    /// `isChat: isAgent(tab)`), and only the `isTerminal` one carries this bar:
    /// `pid {{ focus.pid }} │ {{ termSize }} │ [{{ footRemote }}] … file:line references open in
    /// a tab`. Its `isChat` sibling's only bottom bar is the `hasBar` readout strip. Issue #20
    /// named this bar "the **terminal** footer" too ("2. Terminal: move Clear into the footer"),
    /// and §4b gates the environment chip "in both the window bar and **the terminal footer**".
    ///
    /// Rendering it under an agent as well was the double-bar bug the user reported live against
    /// the shipped build ("both are displayed at the same time in both terminal and agents but
    /// should not"): `TerminalPane` being one component behind both tab kinds is a fact about the
    /// widget, not a licence for the chrome around it, and the mock draws the two kinds' chrome
    /// differently on purpose. An agent's pid is not lost with this bar - it moved to the header,
    /// which is where the `isChat` branch has always drawn it.
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
            .debug_selector(|| "pty-info-footer".to_string())
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

    /// The **agent** pane's bottom strip - **a readout, not an action bar** (GitHub issue #295,
    /// `STAGE-A-CHANGELOG.md` §4t).
    ///
    /// [`ProcessKind::Agent`] only, and mutually exclusive with
    /// [`Self::render_pty_info_footer`] - a pane gets one bottom bar, never both. §4t's "the bar
    /// now renders **whenever there is an agent**" is a statement about *status*, not about pane
    /// kind: it means the strip no longer waits for a status that offers a button. A `Shell` tab
    /// is not "an agent" in that sentence's sense - `Jerry.dc.html` gates the whole strip on
    /// `isChat: isAgent(tab)`, and §4u′ says so from the other direction when it accepts that the
    /// budget popover is "reachable only from an agent pane. **On a terminal tab**, the graph or
    /// Settings there is no way to open it… those surfaces have no agent spending anything."
    /// Reading it as "whenever there is a pane" is what stacked this strip under the terminal's
    /// own info footer in every tab.
    ///
    /// It renders whenever there is an agent, not only when that agent's status happens to
    /// offer an action, which is what turns the now-buttonless `ask`/`run`/`finished` states from
    /// absent strips into useful ones. Left to right: whatever run-scoped verbs the status really
    /// earns ([`work_surface::footer_actions`] - none at all for three of the five statuses), a
    /// spacer, then this one agent's own cost, right-aligned
    /// ([`Self::render_agent_cost_readout`]).
    ///
    /// Then, past a 1px divider, the per-agent provider budget §4t puts to the right of the cost
    /// (`5h ▓▓▓▓▓▓░ 92%   7d ▓▓▓▓░░░ 65%`, plus the popover §4u′ moved into this strip) -
    /// GitHub issue #294, [`Self::render_agent_budget_readout`]. That readout is itself `None`
    /// for a pane that spends no provider, which a `Shell` never can - a second, narrower reason
    /// this whole strip is agent-only.
    ///
    /// No `JERRY` wordmark (deliberate deviation from the design mockup, per direct user request -
    /// see this crate's `lib.rs`/`BUILD-LOG.md` for context, not a bug fix).
    pub(in crate::work_surface) fn render_pty_footer(
        &self,
        agent: &Agent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.agent_status(agent, cx);
        let (is_running, pid) = {
            let pane = agent.pane.read(cx);
            (pane.is_running(), pane.pid())
        };
        let actions = work_surface::footer_actions(status_value);
        let id = agent.id;

        let mut footer = div()
            .id("pty-footer")
            .debug_selector(|| "pty-footer".to_string())
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
            // `Discard worktree` shares the worktree-history in-flight guard
            // (`Self::worktree_history_op_in_flight`) with the title bar's Agent menu row and the
            // rail's own `Remove worktree…` - see `crate::worktree_history::flow`'s module docs.
            // Disabled, not just relabelled, while busy - mirrors `Self::render_rail_footer`'s own
            // `prune_in_flight` gating.
            if action.kind == work_surface::ActionKind::DiscardWorktree
                && self.worktree_history_op_in_flight.is_some()
            {
                enabled = false;
            }
            // Busy labels are keyed off the *specific* in-flight kind, not just "something is
            // running" - a real, live-reproduced bug an audit caught: keying this off the bare
            // in-flight flag alone made every visible `Discard worktree` button across every
            // agent read "discarding…" while an unrelated worktree-history op was running.
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
                _ => action.label.to_string(),
            };
            footer = footer.child(self.render_footer_action_button(id, action, label, enabled, cx));
        }

        footer = footer.child(div().flex_1());
        // GitHub issue #294: this agent's provider budget, to the right of its cost (§4t).
        let budget = self.render_agent_budget_readout(agent.kind, cx);
        if let Some(cost) = self.render_agent_cost_readout(is_running, pid) {
            footer = footer.child(cost);
            // §4t's own grouping: the two readouts are separate facts (what this agent costs
            // *this machine*, and what it costs *its provider*), so they get the footer's own 1px
            // divider between them rather than running together as one string. No divider when
            // only one of the two is there - the same "never a hairline separating nothing from
            // nothing" rule §4t applied when it deleted the footer's own budget slot.
            if budget.is_some() {
                footer = footer.child(
                    div()
                        .flex_none()
                        .w(px(1.0))
                        .h(px(13.0))
                        .bg(theme::status_bar::DIVIDER),
                );
            }
        }
        match budget {
            Some(budget) => footer.child(budget),
            None => footer,
        }
    }

    /// §4t's per-agent cost readout: `6.2% cpu · 0.51 GB` for **this** agent's own pid, at the
    /// status bar's own recessive tier so the pane strip and the window footer speak in the same
    /// type sizes rather than inventing a second scale.
    ///
    /// `None` - genuinely nothing rendered, not a dimmed placeholder - in the two cases §4t and
    /// `crate::status_bar::render::AdeApp::render_status_resources_readout` already agree on:
    /// "blank for an agent that is not running" (an exited pane has no pid to sample and no cost
    /// to report), and a build whose platform has no real sampling backend at all
    /// (`process_stats::PLATFORM_SAMPLING_SUPPORTED`), where a permanent `...% cpu` could never
    /// resolve.
    ///
    /// Reads the same `AdeApp::process_stats` sample map and the same
    /// `crate::status_bar::resources` formatters the Resources popover and the status bar total
    /// use - one sampling pipeline, three surfaces, never a second derivation that could disagree
    /// with the total this agent is part of.
    pub(in crate::work_surface) fn render_agent_cost_readout(
        &self,
        is_running: bool,
        pid: Option<u32>,
    ) -> Option<impl IntoElement> {
        if !crate::status_bar::process_stats::PLATFORM_SAMPLING_SUPPORTED {
            return None;
        }
        let pid = pid.filter(|_| is_running)?;
        let (cpu, memory) = crate::status_bar::resources::row_sample(
            pid,
            &self.process_stats,
            crate::status_bar::render::available_cores(),
        );
        let tier = crate::status_bar::render::StatusTier::Recessive;

        Some(
            div()
                .id("pty-footer-cost")
                .debug_selector(|| "pty-footer-cost".to_string())
                .flex_none()
                .font(font(theme::font::MONO))
                .font_weight(tier.weight())
                .text_size(self.ui_text_size(tier.text_size()))
                .text_color(tier.color())
                .tooltip(text_tooltip(
                    "What this agent is costing this machine right now".to_string(),
                ))
                .child(crate::status_bar::resources::agent_readout(cpu, memory)),
        )
    }

    /// One footer action button - interactive (`cursor_pointer`, hover, `on_click` dispatch on
    /// `action.kind`) when `enabled`, otherwise dimmed with no cursor/hover/click at all - never
    /// a button that looks clickable but silently does nothing.
    pub(in crate::work_surface) fn render_footer_action_button(
        &self,
        id: AgentId,
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
        let selector = format!("footer-action-{kind:?}");
        let mut button = div()
            .id(format!("footer-action-{id}-{kind:?}"))
            .debug_selector(move || selector)
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
                        work_surface::ActionKind::Respawn => this.respawn_agent(id, window, cx),
                        work_surface::ActionKind::DiscardWorktree => {
                            this.request_discard_worktree(id, window, cx)
                        }
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

        // GitHub issue #225: the review tab occupies the centre pane exactly as the graph tab
        // does. Checked first because `open_review_tab` always leaves the graph tab on its way in,
        // so the two flags are never both set - the order only matters as a defence against a
        // future path that forgets that.
        if self.review_tab_active {
            let body = self.render_review_view(cx);
            return surface.child(body).into_any_element();
        }

        // GitHub issue #227: and so does the run-transcript tab. Same reasoning as above - the
        // flags are never both set, because every opener leaves the others.
        if self.run_tab_active {
            let body = self.render_run_view(cx);
            return surface.child(body).into_any_element();
        }

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
                        .debug_selector(|| "pty-surface".to_string())
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
                                .id("pty-surface-content")
                                .debug_selector(|| "pty-surface-content".to_string())
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .overflow_hidden()
                                .child(agent.pane.clone().into_any_element()),
                        )
                        // One bottom bar per pane, picked by pane kind - never both stacked.
                        // `Jerry.dc.html`'s two pane branches are mutually exclusive `sc-if`
                        // siblings: `isTerminal` gets `pid │ 148×38 │ [wsl] … file:line
                        // references open in a tab`, `isChat` gets the `hasBar` readout strip
                        // (actions · cost · budget) and puts its pid in the header instead.
                        // Rendering both under every pane is the duplication reported live
                        // against the shipped build - see the two methods' own docs.
                        .child(match agent.kind {
                            ProcessKind::Shell => {
                                self.render_pty_info_footer(agent, cx).into_any_element()
                            }
                            ProcessKind::Agent(_) => {
                                self.render_pty_footer(agent, cx).into_any_element()
                            }
                        })
                        .into_any_element()
                };
                // `showAgentBar: noAgents || activeWt.agents.indexOf(tab) >= 0` - the identity
                // bar belongs to agent panes, plus the bare-worktree case that holds the
                // `Start an agent` CTA. The mock's own comment: "The whole row is agent
                // identity, so it belongs to agent panes only… in a terminal pane there is no
                // agent to describe. Kept when the worktree has no agents at all, since it holds
                // that empty state's CTA." Same shell-tab-wearing-agent-chrome mistake as the
                // stacked footers below, and #295 listed "the bar stays scoped to agent panes"
                // as an acceptance criterion.
                let show_context_bar =
                    agent.kind.is_agent_session() || self.current_worktree_is_bare();
                surface
                    .children(show_context_bar.then(|| self.render_agent_context_bar(agent, cx)))
                    .child(body)
            }
            None => surface.child(self.render_no_agents_empty_state(cx)),
        }
        .into_any_element()
    }

    /// Whether `id` is genuinely the agent the centre pane is showing right now - the exact same
    /// cascade [`Self::render_center_pane`] itself follows, mirrored here so that nothing which
    /// draws a "this is the selected one" edge from [`Agents::active_id`] - the rail's own agent
    /// row (`crate::rail::render::AdeApp::render_agent_row`) and this tab strip's own agent tab
    /// ([`Self::render_agent_tab`]) - can disagree with what the centre pane is actually showing.
    ///
    /// [`Agents::active_id`]'s own docs are explicit that it is "read directly by the centre pane
    /// with no repo-scoping of its own": every *other* centre-pane occupant - the review tab, the
    /// run tab, the graph tab - is a separate `bool` layered on top precisely because `Agents`
    /// itself has no way to know about them ([`Self::review_tab_active`]/[`Self::run_tab_active`]/
    /// [`Self::graph_tab_active`]), so any caller that wants "is the agent pane really what's on
    /// screen" has to apply the same layering [`Self::render_center_pane`] does, rather than
    /// reading [`Agents::active_id`] straight.
    ///
    /// Before this existed, both call sites above did exactly that - read [`Agents::active_id`]
    /// straight - so opening a history run (GitHub issue #227) left whichever agent had been
    /// active still drawn as the selected rail row *and* the active tab, alongside the run's own
    /// now-correctly-selected row: two rows/tabs reading as selected at once, a live user report.
    /// The review and graph tabs share the identical shape and were silently carrying the same
    /// bug; this closes it for all three at the one root, rather than teaching each caller a
    /// fourth flag to check.
    ///
    /// GitHub issue #382: the File/Diff surface (Surface C) shares the identical shape and was
    /// still missing here, left out of the original #227 fix because that fix only chased the
    /// three flag-gated occupants (review/run/graph) and never the fourth, structurally different
    /// one - `Self::open_change`, a `PathBuf` rather than a `bool`. Opening a file tab while an
    /// agent tab was active left the agent's rail row *and* tab strip entry still drawn as
    /// selected, with the file tab now also selected: the exact "two selected at once" shape
    /// #227 fixed, just for a fourth occupant nobody had chased down yet.
    ///
    /// `open_change.is_some()` alone is not enough to conclude Surface C is genuinely on screen -
    /// see [`crate::code_surface::editing::AdeApp::active_edit_target`]'s own docs (which mirror
    /// this exact predicate for the identical reason) for the real, reachable state that predicate
    /// is guarding against: a tab can be "active" (its path still in `open_change`) without a diff
    /// to show it (`open_diff_file_cache` is `None`) and `code_view` left on `Diff`, in which case
    /// [`Self::render_center_pane`] falls through to the agent/merge surface with `open_change`
    /// still `Some` the whole time - the weaker check would wrongly report the agent pane as not
    /// showing while it genuinely is.
    pub(crate) fn active_agent_pane_id(&self) -> Option<AgentId> {
        let file_or_diff_surface_showing = self.open_change.is_some()
            && (self.open_diff_file_cache.is_some() || self.code_view == code_view::CodeView::File);
        if self.review_tab_active
            || self.run_tab_active
            || self.graph_tab_active
            || file_or_diff_surface_showing
        {
            return None;
        }
        self.agents.active_id()
    }

    /// The centre pane with no agent open in this worktree at all.
    ///
    /// The one surface GitHub issue #295 leaves buttoned, quoting §4t verbatim: "The empty-worktree
    /// case keeps its buttons: with no agent there is no keystroke to duplicate and no readout to
    /// show." `Start an agent` (the primary CTA, [`Self::new_agent_pane`]) and `Open terminal`
    /// (§4e: "`Open terminal` survives only in the no-agents empty state, where it is the
    /// legitimate secondary CTA", [`Self::open_companion_terminal`]) - the same two the design's
    /// `noAgents` branch renders, and the only home either verb has left in this pane.
    ///
    /// `open_companion_terminal` is given the worktree that is genuinely selected right now
    /// ([`Self::current_worktree_path`]), not a remembered agent cwd: with no agent open there is
    /// no agent cwd to inherit, and spawning a shell anywhere but the selected worktree is what
    /// that method's own docs already forbid.
    fn render_no_agents_empty_state(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let cwd = self.current_worktree_path();
        let colors = work_surface::action_button_colors(work_surface::ActionStyle::Outline);

        div()
            .flex()
            .flex_1()
            .flex_col()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap(px(14.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::FAINT)
                    .child("no agents open in this worktree"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(self.render_start_agent_button("empty-state-start-agent", cx))
                    // Only when a real worktree is genuinely selected: with none,
                    // `open_companion_terminal` would have nowhere to spawn, and a button that
                    // could only no-op is exactly what this app renders disabled or not at all.
                    .children(cwd.map(|cwd| {
                        div()
                            .id("empty-state-open-terminal")
                            .debug_selector(|| "empty-state-open-terminal".to_string())
                            .flex_none()
                            .cursor_pointer()
                            .h(px(20.0))
                            .px(px(8.0))
                            .rounded(theme::radius::BUTTON)
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .flex()
                            .items_center()
                            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                            .child(
                                div()
                                    .font(font(theme::font::SANS))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_size(px(10.5))
                                    .text_color(colors.fg)
                                    .child("Open terminal"),
                            )
                            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                                this.open_companion_terminal(cwd.clone(), window, cx);
                            }))
                    })),
            )
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
pub(in crate::work_surface) fn render_tab_chip(
    kind: ProcessKind,
    active: bool,
) -> gpui::AnyElement {
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
    /// GitHub issue #225 - the agent review tab. Carries the agent id it reviews, since (unlike
    /// the graph tab) a review is always *of* a specific agent.
    Review {
        id: AgentId,
        label: String,
    },
    /// GitHub issue #227 - the run-transcript tab. No id payload, for
    /// [`work_surface::TabRef::Run`]'s own reason: a worktree's strip holds at most one.
    Run {
        label: String,
    },
}

impl DraggedTab {
    fn label(&self) -> &str {
        match self {
            DraggedTab::Agent { label, .. } => label,
            DraggedTab::File { label, .. } => label,
            DraggedTab::Graph { label } => label,
            DraggedTab::Review { label, .. } => label,
            DraggedTab::Run { label } => label,
        }
    }

    /// This dragged value's own identity as a [`work_surface::TabRef`] - what
    /// [`AdeApp::reorder_tab`] actually moves, regardless of which concrete kind was dragged.
    pub(in crate::work_surface) fn tab_ref(&self) -> work_surface::TabRef {
        match self {
            DraggedTab::Agent { id, .. } => work_surface::TabRef::Agent(*id),
            DraggedTab::File { path, .. } => work_surface::TabRef::File(path.clone()),
            DraggedTab::Graph { .. } => work_surface::TabRef::Graph,
            DraggedTab::Review { id, .. } => work_surface::TabRef::Review(*id),
            DraggedTab::Run { .. } => work_surface::TabRef::Run,
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

/// How long a neighbour tab's own slide-into-place animation runs, when a drop shifts its slot
/// (task #65, the remaining GitHub issue #16 gap: before this, every tab other than the one
/// actually dropped just teleported to its new slot). Deliberately the same value as
/// [`TAB_SETTLE_ANIMATION_DURATION`] - one drop kicks off both animations together, and having
/// them run for different durations would read as two unrelated effects rather than one drop
/// settling everything it touched.
pub(in crate::work_surface) const TAB_SLIDE_ANIMATION_DURATION: Duration =
    TAB_SETTLE_ANIMATION_DURATION;

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
        // Measured by `agent_pane_readout_tests` to prove the context bar really ends here
        // (GitHub issue #295 / §4e: "identity and status only") - any re-added trailing button
        // would push this pill left of the bar's own right padding.
        .debug_selector(|| "agent-context-bar-status".to_string())
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            app.agents.spawn(
                ProcessKind::claude(),
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
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

    /// GitHub issue #227's resume flow: `Agents::spawn_resume` must really produce a
    /// `claude --resume <session_id> --settings <path>` spawn - `--resume` prepended ahead of the
    /// real hook injection's own `--settings`, not dropped in favour of it, and the hook
    /// injection's `JERRY_*` environment still riding along so a resumed conversation keeps
    /// reporting its status exactly like a fresh one. Uses a real [`crate::hooks::HookRuntime`]
    /// (a real bound loopback listener, real generated files) rather than a hand-built
    /// injection, so this exercises the exact object `crate::hooks::flow::AdeApp::
    /// hook_injection_for` would hand a real spawn.
    #[gpui::test]
    fn spawn_resume_prepends_resume_ahead_of_the_real_hook_injection(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let hook_temp = tempfile::tempdir().expect("hook temp dir");
        let runtime = crate::hooks::HookRuntime::start(hook_temp.path())
            .expect("the hook runtime must start in a test sandbox");
        let injection = runtime.injection();
        let session_id = "5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned();

        let pane = app.update_in(cx, |app, window, cx| {
            let id = app.agents.spawn_resume(
                AgentKind::Claude,
                repo.path().to_path_buf(),
                12.0,
                None,
                Some(&injection),
                session_id.clone(),
                window,
                cx,
            );
            app.agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("the agent this call just spawned")
                .pane
                .clone()
        });

        let spec = pane.read_with(cx, |pane, _| pane.spec_for_test().clone());
        assert_eq!(
            spec.program,
            PathBuf::from("claude"),
            "a resumed agent must still spawn the real claude binary"
        );
        assert_eq!(
            spec.args[0..2],
            ["--resume".to_owned(), session_id],
            "--resume <session_id> must lead the argument list"
        );
        assert_eq!(
            spec.args[2], "--settings",
            "the real hook injection's own --settings must still follow, not be dropped"
        );
        assert!(
            !spec.env.is_empty(),
            "the hook injection's JERRY_* environment must still ride along on a resume spawn, \
             so a resumed conversation keeps reporting its status like a fresh one"
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            app.select_worktree(1, window, cx);
            let id_b = app.agents.spawn(
                ProcessKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            let id2 = app.agents.spawn(
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            let id3 = app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::claude(),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::claude(),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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

    /// The remaining GitHub issue #16 gap this closes (task #65): a drop must not just
    /// settle-fade the tab that was dragged - every *other* tab whose slot it passed over must
    /// slide too. Proven end to end against `Self::tab_bounds`'s own real, `gpui::canvas`-painted
    /// width (`cx.run_until_parked()` lets that canvas actually paint, exactly like the plus
    /// button's own bounds tests do) - not a fabricated placeholder offset. Three real agent tabs
    /// dragging tab 1 to land after tab 3 must shift both 2 and 3 (each by tab 1's own real
    /// width), and must never record tab 1 itself - it already has its own settle-fade
    /// (`drop_dragged_tab_records_a_fresh_settle_id_for_the_dropped_tab`, just above).
    #[gpui::test]
    fn drop_dragged_tab_records_real_slide_offsets_for_every_shifted_neighbour(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (first_id, second_id, third_id) = app.update_in(cx, |app, window, cx| {
            let first_id = app.agents.active_id().expect("initial shell agent");
            let second_id = app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            let third_id = app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            (first_id, second_id, third_id)
        });
        cx.run_until_parked();

        let dragged_width = app
            .read_with(cx, |app, _| {
                app.tab_bounds
                    .get(&work_surface::TabRef::Agent(first_id))
                    .map(|bounds| bounds.size.width)
            })
            .expect(
                "tab 1 must have really painted at least once (via the shared chrome's own \
                 `gpui::canvas`) before a drag off it can be measured",
            );
        assert!(
            dragged_width > px(0.0),
            "a real, already-painted tab must have nonzero measured width"
        );

        // Simulates what a real `on_drag_move` tick over the right half of tab 3's own tab would
        // already have recorded, just before the drop - the same precedent
        // `dropping_a_tab_clears_dragging_tab` (just below) uses.
        app.update(cx, |app, _cx| {
            app.tab_drag_insertion = Some((work_surface::TabRef::Agent(third_id), true));
        });
        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Agent(first_id),
                work_surface::TabRef::Agent(third_id),
                cx,
            );
        });

        let slide = app.read_with(cx, |app, _| app.tab_slide.clone());
        assert_eq!(
            slide.len(),
            2,
            "tabs 2 and 3 both sat between tab 1's old slot (0) and its new one (2) - both, and \
             only both, must slide: {slide:?}"
        );
        let (offset_2, _) = slide[&work_surface::TabRef::Agent(second_id)];
        let (offset_3, _) = slide[&work_surface::TabRef::Agent(third_id)];
        assert_eq!(
            offset_2, dragged_width,
            "tab 2 must slide by exactly tab 1's own real measured width"
        );
        assert_eq!(
            offset_3, dragged_width,
            "tab 3 must slide by exactly tab 1's own real measured width too - not by its own \
             (different) width"
        );
        assert!(
            !slide.contains_key(&work_surface::TabRef::Agent(first_id)),
            "the dragged tab itself must never be recorded here - it already got its own \
             settle-fade, never both at once"
        );
    }

    /// A no-op drop (dragging a tab onto its own current slot) must record no slide state at all
    /// - nothing actually changed position, so nothing may animate
    /// (`drag_reorder_is_a_no_op_for_an_unknown_or_identical_id`'s own proof for the underlying
    /// reorder itself; this is that same guarantee for the slide it now also kicks off).
    #[gpui::test]
    fn drop_dragged_tab_records_no_slide_state_for_a_no_op_drop(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let initial_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("initial shell agent")
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Agent(initial_id),
                work_surface::TabRef::Agent(initial_id),
                cx,
            );
        });

        let slide = app.read_with(cx, |app, _| app.tab_slide.clone());
        assert!(
            slide.is_empty(),
            "dropping the only tab onto itself changed nothing - nothing may slide: {slide:?}"
        );
    }

    /// What the tab strip will really label agent `id` - the exact per-tab method
    /// [`AdeApp::render_tab_strip`] itself calls, not a test-local reimplementation of it.
    fn tab_label(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        id: AgentId,
    ) -> String {
        app.read_with(cx, |app, cx| {
            let agent = app
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("a live agent");
            app.agent_tab_label(agent, cx)
        })
    }

    /// Feeds `title` into agent `id`'s pane as a real OSC 0 window-title sequence - byte for
    /// byte what `printf '\033]0;<title>\007'` puts on a pty - through the pane's own real
    /// `TerminalGrid` parser, the same one the poll loop hands live pty bytes to. The real-pty
    /// transport in front of that parser has its own end-to-end proof against a real child
    /// process in `crate::terminal::pane`
    /// (`a_real_pty_process_setting_its_title_is_captured_and_classified`); injecting here keeps
    /// *this* module's subject - what the tab strip does with a title once one exists - free of
    /// a real process's scheduling.
    fn set_live_title(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        id: AgentId,
        title: &str,
    ) {
        let pane = app.read_with(cx, |app, _| {
            app.agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("a live agent")
                .pane
                .clone()
        });
        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(format!("\x1b]0;{title}\x07").as_bytes(), cx);
        });
    }

    /// That agent tab's real, `gpui::canvas`-painted width from the last drawn frame
    /// (`Self::tab_bounds`) - the same real measurement the drag tests above take.
    fn painted_tab_width(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        id: AgentId,
    ) -> gpui::Pixels {
        app.read_with(cx, |app, _| {
            app.tab_bounds
                .get(&work_surface::TabRef::Agent(id))
                .map(|bounds| bounds.size.width)
        })
        .expect("the tab must have really painted at least once before it can be measured")
    }

    /// The complaint this whole design answers: "when testing I don't see any changes in the
    /// title of terminal or agents, they just say `terminal #1/#2`". A tab is now labelled with
    /// whatever the process inside it says it is *right now* - its live OSC 0/2 title - and that
    /// label really moves when the title does.
    ///
    /// Both halves are asserted: the label itself (`Self::agent_tab_label`, the one method the
    /// strip calls per tab), and that the strip genuinely *repaints* with it. The repaint half
    /// is measured against the tab's own real painted width (`Self::tab_bounds`): nothing here
    /// touches `AdeApp` at all between the two frames - only the pane's own `cx.notify()` off
    /// the injected bytes - so a wider painted tab can only mean that notify really re-drove
    /// `Self::render_tab_strip` with the new title.
    #[gpui::test]
    fn a_tabs_label_is_its_panes_live_title_and_the_strip_repaints_when_that_title_changes(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let shell_id = app.read_with(cx, |app, _| {
            app.agents
                .active_id()
                .expect("the real startup shell agent")
        });
        cx.run_until_parked();

        let program = app.read_with(cx, |app, cx| {
            app.agents
                .iter()
                .find(|agent| agent.id == shell_id)
                .expect("shell agent")
                .pane
                .read(cx)
                .program_label()
        });
        assert_eq!(
            tab_label(&app, cx, shell_id),
            program,
            "a shell that hasn't set a title yet must show its real resolved program name"
        );
        let width_before = painted_tab_width(&app, cx, shell_id);

        set_live_title(&app, cx, shell_id, "~/src/jerry/crates/app \u{2014} vim");
        cx.run_until_parked();

        assert_eq!(
            tab_label(&app, cx, shell_id),
            "~/src/jerry/crates/app \u{2014} vim",
            "the tab must show the title the process actually set, verbatim"
        );
        assert!(
            painted_tab_width(&app, cx, shell_id) > width_before,
            "the strip must have really repainted with the longer live title - only the pane's \
             own notify happened between the two frames"
        );

        // And again, to prove this is a live reading rather than a one-shot latch taken the
        // first time a title ever arrived.
        set_live_title(&app, cx, shell_id, "cargo test");
        cx.run_until_parked();
        assert_eq!(
            tab_label(&app, cx, shell_id),
            "cargo test",
            "a second title change must move the label again"
        );
    }

    /// "Why the name at all? Just name the tab what it is and stop, no need to add names or
    /// numbers." Two panes really showing the same thing get the same label - no `#1`/`#2`, no
    /// branch suffix, nothing synthesised to tell them apart, exactly like every real terminal
    /// emulator's tabs. The human tells them apart by position, which is the truth about them.
    #[gpui::test]
    fn two_tabs_with_the_same_live_title_render_the_same_label_with_nothing_added(
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
                ProcessKind::claude(),
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            let second_id = app.agents.spawn(
                ProcessKind::claude(),
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            (first_id, second_id)
        });
        cx.run_until_parked();

        set_live_title(&app, cx, first_id, "\u{2733} Claude Code");
        set_live_title(&app, cx, second_id, "\u{2733} Claude Code");
        cx.run_until_parked();

        let first = tab_label(&app, cx, first_id);
        let second = tab_label(&app, cx, second_id);
        assert_eq!(
            first, "\u{2733} Claude Code",
            "the label is the live title and nothing else"
        );
        assert_eq!(
            first, second,
            "two tabs genuinely showing the same thing must render the same label - \
             synthesising a difference between them would be inventing one"
        );
        assert!(
            !first.contains('#') && !first.contains('\u{b7}'),
            "no ordinal and no branch suffix may be bolted onto a live title: {first}"
        );

        // The real production strip must build fine with two identically labelled tabs in it.
        app.update(cx, |app, cx| {
            let _ = app.render_tab_strip(cx);
        });
    }

    /// The honest fallback, and the only one: a pane whose process has set no title yet (many
    /// shells only set one from their prompt hook, and plenty of setups never set one at all)
    /// shows its real resolved program name - never a blank tab, and never the old generic
    /// `"terminal"` literal that started this whole complaint.
    #[gpui::test]
    fn a_pane_that_has_reported_no_title_shows_its_real_program_name_not_an_empty_tab(
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (label, program) = app.read_with(cx, |app, cx| {
            let agent = app
                .agents
                .iter()
                .find(|agent| agent.id == shell_id)
                .expect("shell agent");
            (
                app.agent_tab_label(agent, cx),
                agent.pane.read(cx).program_label(),
            )
        });
        assert_eq!(
            label, program,
            "a titleless pane must fall back to its own real resolved program name"
        );
        assert!(!label.is_empty(), "a tab must never render an empty label");
        assert_ne!(
            label, "terminal",
            "the generic placeholder label is gone - a tab says what is really in it"
        );
        assert!(
            !label.contains('\u{b7}'),
            "no branch suffix is appended to a shell tab any more: {label}"
        );

        // A process clearing its title (`\x1b]0;\x07`) is back to "nothing to show", not a
        // blank tab.
        set_live_title(&app, cx, shell_id, "");
        cx.run_until_parked();
        assert_eq!(
            tab_label(&app, cx, shell_id),
            program,
            "an emptied title must fall back to the program name, not blank the tab"
        );
    }

    /// Uniformity across both tab kinds (`work_surface::TabChipKind::Cli` and `::Term`): an
    /// agent CLI's tab is titled by the same live mechanism a plain shell's is. An agent's own
    /// title is the *more* informative of the two - it's what `crate::rail::title_signal`
    /// already reads to tell whether that agent is working - so there is no per-kind naming
    /// authority that preferring it would regress.
    #[gpui::test]
    fn an_agent_clis_tab_takes_its_live_title_exactly_like_a_shells_does(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        let agent_id = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::claude(),
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| work_surface::tab_chip_kind(
                app.agents
                    .iter()
                    .find(|agent| agent.id == agent_id)
                    .expect("agent")
                    .kind
            )),
            work_surface::TabChipKind::Cli,
            "sanity check: this is the CLI chip kind, the other half of the uniformity claim"
        );
        assert_eq!(
            tab_label(&app, cx, agent_id),
            "claude",
            "before it says anything, an agent tab shows its real binary name"
        );

        set_live_title(&app, cx, agent_id, "\u{25d0} Claude Code");
        cx.run_until_parked();

        assert_eq!(
            tab_label(&app, cx, agent_id),
            "\u{25d0} Claude Code",
            "an agent tab must follow its live title just as a shell tab does"
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
    /// with a real agent running. Bareness no longer affects tab *labels* either - a tab is
    /// named by its own pane's live title (`Self::agent_tab_label`) whatever else is open in
    /// the worktree - so this is now the only thing bareness could have touched here.
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            let claude_id = app.agents.spawn(
                ProcessKind::claude(),
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
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

    /// GitHub issue #382: [`AdeApp::active_agent_pane_id`]'s own cascade - the fix for GitHub
    /// issue #227 - only ever zeroed out for the review/run/graph tabs, because those three are
    /// plain `bool` flags. The File/Diff surface is a fourth centre-pane occupant with the
    /// identical shape, but tracked through `Self::open_change` (a `PathBuf`, not a `bool`), and
    /// was left out of the original fix. Opening a file tab while an agent was the active centre-
    /// pane content left that agent's own tab strip entry (and rail row, which reads the same
    /// method) still drawn as selected, alongside the file tab that had genuinely taken over the
    /// centre pane - the exact "two selected at once" shape #227 fixed, just for a fourth occupant
    /// nobody had chased down yet. Mirrors
    /// `crate::run_history::render::tab_scoping_tests::switching_between_an_agent_and_a_history_run_leaves_exactly_one_selected`'s
    /// own "exactly one selected" idiom, but for a file tab, and checks the real painted surfaces
    /// (`"pty-surface"`/`"file-view-code-list"`) rather than only the logical predicate, so this
    /// fails if the fix is real but `render_center_pane` itself ever drifts from
    /// `active_agent_pane_id`'s cascade.
    #[gpui::test]
    fn switching_from_an_agent_to_a_file_tab_leaves_exactly_one_selected(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("mod.rs"), "fn main() {}\n").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // A real second agent (the startup shell is `Agents`' first entry) so this exercises a
        // genuine agent tab rather than the guaranteed startup shell.
        app.update_in(cx, |app, window, cx| {
            app.new_agent(ProcessKind::Agent(AgentKind::Claude), window, cx)
        });
        cx.run_until_parked();
        let agent_id = app.read_with(cx, |app, _| {
            app.agents.iter().last().expect("a spawned agent").id
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.active_agent_pane_id()),
            Some(agent_id),
            "premise: the freshly spawned agent is the one genuinely selected right now"
        );
        assert!(
            cx.debug_bounds("pty-surface").is_some(),
            "premise: the agent's own pty surface is genuinely on screen"
        );

        // The real click path: `Self::open_file_view`, the same call go-to-definition, a
        // file-tree row click and a palette file result all make.
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("mod.rs"), window, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-view-code-list").is_some(),
            "the file's own code view must now genuinely be on screen"
        );
        assert!(
            cx.debug_bounds("pty-surface").is_none(),
            "the agent's pty surface must no longer be on screen - the file tab replaced it in \
             the centre pane"
        );
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.active_agent_pane_id(),
                None,
                "the agent's rail row/tab must no longer read as selected - the centre pane is \
                 showing the file tab, not that agent's pane. Before the fix this was still \
                 `Some(agent_id)`, because `active_agent_pane_id` only zeroed out for the \
                 review/run/graph tabs and never checked `open_change`"
            );
            assert_eq!(
                app.agents.active_id(),
                Some(agent_id),
                "the *underlying* remembered agent must be untouched, though - it's what \
                 `select_agent` returns to, not something opening a file tab should ever clear"
            );
        });

        // Switch back to the agent - the real click path (`render_agent_tab`'s/`render_agent_row`'s
        // own `on_click`, both of which call `select_agent`).
        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_id, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.active_agent_pane_id()),
            Some(agent_id),
            "selecting the agent tab again must make it read as selected once more"
        );
        assert!(
            cx.debug_bounds("pty-surface").is_some(),
            "and its pty surface must genuinely be back on screen"
        );
        assert!(
            cx.debug_bounds("file-view-code-list").is_none(),
            "with the file view genuinely gone, `select_agent` clears `open_change` on its way \
             back to the agent pane"
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

    /// The real pty's OS reader thread and the real child shell it feeds run entirely outside
    /// GPUI's deterministic scheduler - `cx.background_executor.advance_clock` only fast-forwards
    /// a *simulated* clock, which grants that real thread and that real process zero actual
    /// wall-clock scheduling time. A retry loop that only ever advances the virtual clock (the
    /// pre-fix shape here) can race arbitrarily far ahead of them: standalone, with an otherwise
    /// idle CPU, the real echo lands within real microseconds so the race is never noticed, but
    /// under real full-suite parallel load (dozens of other tests' own real subprocesses - other
    /// ptys, `rust-analyzer`, `pyright`, `typescript-language-server` - contending for the same
    /// cores) the OS can genuinely take real milliseconds to schedule the reader thread, and a
    /// loop that burns through its whole retry budget in real microseconds finds every single
    /// check empty and fails despite the real echo being on its way. This is exactly the same
    /// class of bug `lsp::client::lsp_diagnostics_wiring_tests`' own `wait_for_real_diagnostics`/
    /// `wait_until` exist to avoid one layer up (a real notification arriving on a real OS thread
    /// outside GPUI's scheduler) - the fix is the same discipline: keep re-checking over *real*
    /// wall-clock time (a real `std::thread::sleep` between checks), bounded by a real deadline,
    /// not a fixed count of virtual-clock advances with no real-time floor at all.
    fn wait_for_real_pty_output(
        cx: &mut gpui::VisualTestContext,
        deadline: std::time::Instant,
        mut has_arrived: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) -> bool {
        loop {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(8));
            cx.run_until_parked();
            if has_arrived(cx) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[gpui::test]
    fn dispatching_terminal_clear_signals_only_the_active_agents_pty(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (first_id, second_id) = app.update_in(cx, |app, window, cx| {
            let first = app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            let second = app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let saw_real_output_on_second = wait_for_real_pty_output(cx, deadline, |cx| {
            let second_lines = app.read_with(cx, |app, cx| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == second_id)
                    .expect("second agent")
                    .pane
                    .read(cx)
                    .visible_text_lines()
            });
            // `TerminalPane::clear` wipes its own local grid *first*, synchronously, before it
            // ever writes the real Ctrl-L byte to the pty - so any real, non-blank content that
            // reappears here can only be this real round trip's own echo. That echo can
            // honestly take either shape: a raw `^L` (ECHOCTL, if the shell's own readline
            // hasn't taken over the tty yet - the common case for a just-spawned shell) or a
            // redrawn prompt (readline's own real `clear-screen` binding, once it has) - see
            // `title_bar::render::agent_state_chip_live_tests`'s own docs for the identical real
            // ambiguity, live-observed there first. Searching for the literal `^L` text alone
            // made this test racy against exactly which one a real shell happens to pick under
            // real full-suite load, where the extra real time before Ctrl-L is dispatched can
            // let a freshly spawned shell's readline win a race it would usually lose on an
            // otherwise-idle machine.
            second_lines.iter().any(|line| !line.trim().is_empty())
        });
        assert!(
            saw_real_output_on_second,
            "expected the active (second) agent's real pty to echo something back after \
             TerminalClear's real Ctrl-L byte reached it"
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
                ProcessKind::claude(),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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
                ProcessKind::claude(),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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

    /// See `terminal_clear_action_tests::wait_for_real_pty_output`'s own docs for why this real
    /// wall-clock-bounded retry (not a fixed count of virtual-clock advances) is genuinely
    /// required here too: the real pty reader thread and the real child shell it feeds are
    /// outside GPUI's deterministic scheduler, so only real elapsed time - not simulated time -
    /// gives them a real chance to run under full-suite parallel load.
    fn wait_for_real_pty_output(
        cx: &mut gpui::VisualTestContext,
        deadline: std::time::Instant,
        mut has_arrived: impl FnMut(&mut gpui::VisualTestContext) -> bool,
    ) -> bool {
        loop {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(8));
            cx.run_until_parked();
            if has_arrived(cx) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

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
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let saw_pasted_text = wait_for_real_pty_output(cx, deadline, |cx| {
            let lines = app.read_with(cx, |app, cx| {
                app.agents
                    .active()
                    .expect("an active agent")
                    .pane
                    .read(cx)
                    .visible_text_lines()
            });
            lines
                .iter()
                .any(|line| line.contains("ade-keystroke-paste"))
        });
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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let saw_pasted_text = wait_for_real_pty_output(cx, deadline, |cx| {
            let lines = app.read_with(cx, |app, cx| {
                app.agents
                    .active()
                    .expect("an active agent")
                    .pane
                    .read(cx)
                    .visible_text_lines()
            });
            lines.iter().any(|line| line.contains("ade-pasted-marker"))
        });
        assert!(
            saw_pasted_text,
            "expected the active agent's real pty to echo back the clipboard text \
             TerminalPaste's handler writes"
        );
    }
}

/// The reported "clicking an agent from another worktree/repo, the tab bar does not appear" -
/// `Self::select_agent` used to look the clicked agent's own worktree up only in `Self::
/// worktrees`, the *focused* repo's own list - the rail's own agent rows fold in every repo's
/// agents, not just the focused one's (`crate::rail::render::AdeApp::build_agent_rows`'s own
/// docs), so an agent from a non-focused repo was findable and clickable but its own worktree
/// never was. `Agents::set_active` still ran, so the agent genuinely became active - but nothing
/// switched repos, so `Self::current_worktree_path` kept resolving to the *focused* repo's own
/// selection, `Self::combined_tab_order` built the strip from that wrong cwd, and it came up
/// empty: a real agent, active underneath, with no tab visible for it at all.
#[cfg(test)]
mod select_agent_cross_repo_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::work_surface::agents::ProcessKind;
    use gpui::TestAppContext;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    #[gpui::test]
    fn selecting_an_agent_in_a_non_focused_repo_switches_to_it_and_shows_its_tab(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });
        cx.run_until_parked();

        // A real agent, spawned directly into repo B while repo A stays focused - exactly the
        // state a real cross-repo-persisted agent is in.
        let repo_b_agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::claude(),
                repo_b.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_a.path(),
                "sanity check: repo A is still focused - repo B's agent was spawned in the \
                 background, not through a real repo switch"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.select_agent(repo_b_agent_id, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, cx| {
            assert_eq!(
                app.focused_repo_path(),
                repo_b.path(),
                "selecting an agent in a non-focused repo must really switch focus to that repo"
            );
            assert_eq!(
                app.agents.active_id(),
                Some(repo_b_agent_id),
                "and the agent itself must really be the active one"
            );
            let tab_order = app.combined_tab_order();
            assert!(
                tab_order
                    .iter()
                    .any(|tab_ref| matches!(tab_ref, work_surface::TabRef::Agent(id) if *id == repo_b_agent_id)),
                "the tab strip's own real tab order must include the selected agent - if it \
                 doesn't, the tab bar has nothing to show for it even though the agent is active, \
                 exactly the reported bug"
            );
            let _ = cx;
        });
    }
}

/// GitHub issue #295: the agent pane's context bar is identity-only, and its bottom strip is a
/// readout rather than an action bar (`design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md`
/// §4e/§4r/§4t).
///
/// These drive the real painted surface - real spawned child processes, real
/// `VisualTestContext::debug_bounds` measurements of what actually reached the screen, and real
/// simulated clicks - rather than re-asserting `footer_actions`'s pure output, which
/// `crate::work_surface::state`'s own tests already pin. The point of measuring is that a removed
/// button must be absent from the *paint*, not merely absent from a list.
#[cfg(test)]
mod agent_pane_readout_tests {
    use super::*;
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("base.txt"), "base\n").expect("write");
        git(repo.path(), &["add", "base.txt"]);
        git(repo.path(), &["commit", "-m", "initial"]);
        repo
    }

    /// Spawns one real shell agent in `cwd` (optionally under a `shell_override` that exits by
    /// itself) and selects it, so the centre pane really is that agent's pty surface.
    fn spawn_and_select(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        cwd: PathBuf,
        shell_override: Option<&'static str>,
    ) -> AgentId {
        let id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                cwd,
                12.0,
                shell_override,
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| app.select_agent(id, window, cx));
        cx.run_until_parked();
        id
    }

    /// Waits for a real child process to genuinely exit - the same real-clock-plus-advance loop
    /// `crate::sidebar::render`'s own `/bin/false` test uses, not an assumption that it has.
    fn wait_for_exit(app: &gpui::Entity<AdeApp>, cx: &mut gpui::VisualTestContext, id: AgentId) {
        for _ in 0..200 {
            app.update(cx, |_app, cx| cx.notify());
            cx.run_until_parked();
            let exited = app.read_with(cx, |app, cx| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == id)
                    .is_some_and(|agent| !agent.pane.read(cx).is_running())
            });
            if exited {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(50));
        }
        panic!("premise: the spawned child must really have exited");
    }

    /// A real, non-bare worktree row for `path`. Seeded explicitly because a test app whose
    /// worktree list has not loaded reads as *bare* (`AdeApp::current_worktree_is_bare`), which is
    /// the branch that legitimately still renders `Start an agent` in the context bar - not the
    /// branch these tests are about.
    fn worktree_row(path: PathBuf, branch: &str) -> WorktreeItem {
        WorktreeItem {
            path,
            label: branch.to_string(),
            branch: Some(branch.to_string()),
            is_main: true,
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

    /// §4e: the context bar "now carries identity and status only" - `Merge` and `Archive` are
    /// **deleted**, not hidden or disabled. Both were worktree verbs sitting in an agent header:
    /// offered twice for one two-agent worktree, offered while the agent was `Needs input`, and
    /// gated on preconditions the header cannot show.
    ///
    /// Measured rather than read off the source: the status pill is now the bar's **last** child,
    /// so its painted right edge must sit within the bar's own 12px right padding. Any re-added
    /// trailing button pushes the pill left by that button's full width. Mutation-verified - a
    /// re-added `Archive` button leaves the pill 83px short and fails this assertion.
    #[gpui::test]
    fn the_context_bar_ends_at_the_status_pill_with_no_merge_or_archive(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.worktrees = vec![worktree_row(repo.path().to_path_buf(), "main")];
            app.select_worktree(0, window, cx);
        });
        let id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);
        // A real spawned child, relabelled to a real agent kind - `current_worktree_is_bare`
        // counts agent *sessions*, and a plain shell leaves the worktree bare (which is the
        // branch that legitimately keeps `Start an agent`). Same real-process-plus-relabel the
        // Runs-section tests use.
        app.update(cx, |app, cx| {
            app.agents.set_kind_for_test(id, ProcessKind::claude());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "premise: a real, non-bare worktree - the bare branch keeps its own `Start an agent`"
        );

        let bar = cx
            .debug_bounds("agent-context-bar")
            .expect("the agent context bar must really paint for a selected agent");
        let pill = cx
            .debug_bounds("agent-context-bar-status")
            .expect("its status pill must really paint");
        assert!(
            cx.debug_bounds("context-bar-start-agent").is_none(),
            "premise: a worktree that already has an agent renders no `Start an agent` either"
        );

        let trailing_gap = (bar.origin.x + bar.size.width) - (pill.origin.x + pill.size.width);
        assert!(
            trailing_gap <= px(13.0),
            "the status pill must be the last thing in the context bar - it ends {trailing_gap:?} \
             from the bar's right edge, more than the bar's own 12px padding, so something is \
             painted after it. \u{a7}4e deleted `Merge` (now the git graph's job, issue #241) and \
             `Archive` (now the rail's agent/worktree menus, issue #290)."
        );
    }

    /// §4t: "The bar now renders whenever there is an agent, not only when there are actions - so
    /// the finished and asking states, which §4r emptied, are useful strips again rather than
    /// absent ones."
    ///
    /// A freshly spawned pane has just written its prompt, so it is `Status::Run` - the status
    /// §4t emptied of buttons entirely (`⌃C` in the focused pty is the interrupt). The strip must
    /// still paint, and must carry this agent's own cost readout.
    ///
    /// Relabelled to a real agent kind first: §4t's "whenever there is an agent" is about
    /// *status*, not pane kind, and a `Shell` tab gets the terminal pane's own info footer
    /// instead ([`AdeApp::render_pty_info_footer`]). This test asserted the strip painted for a
    /// plain shell before that was fixed - which is exactly the duplication the user reported.
    #[gpui::test]
    fn a_running_agents_strip_still_paints_and_carries_its_own_cost(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);
        app.update(cx, |app, cx| {
            app.agents.set_kind_for_test(id, ProcessKind::claude());
            cx.notify();
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, cx| {
            let agent = app
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("the spawned agent");
            app.agent_status(agent, cx)
        });
        assert_eq!(
            status,
            Status::Run,
            "premise: a pane that just printed its prompt is `Run`, the status §4t emptied"
        );
        assert!(
            work_surface::footer_actions(status).is_empty(),
            "premise: `Run` offers no footer actions at all"
        );

        assert!(
            cx.debug_bounds("pty-footer").is_some(),
            "the strip must paint for an agent with no actions - that is exactly the state §4t \
             turned from an absent bar into a readout"
        );
        assert!(
            cx.debug_bounds("pty-footer-cost").is_some(),
            "and it must carry this one agent's own `X% cpu \u{b7} Y GB`, from the same per-pid \
             sampling the status bar's total sums (issue #283)"
        );
        assert!(
            cx.debug_bounds("footer-action-Respawn").is_none()
                && cx.debug_bounds("footer-action-DiscardWorktree").is_none(),
            "and no action button at all may paint on a running agent"
        );
    }

    /// One bottom bar per pane, picked by pane kind - reported live against the shipped build as
    /// "the footer of the terminals and agents… right now both are displayed at the same time in
    /// both terminal and agents but should not".
    ///
    /// `Jerry.dc.html`'s two pane branches are mutually exclusive `sc-if` siblings
    /// (`isTerminal: tab === 'terminal'` and `isChat: isAgent(tab)`). The `isTerminal` one is the
    /// only one with a `pid │ {{ termSize }} │ [{{ footRemote }}] … file:line references open in
    /// a tab` bar; the `isChat` one's only bottom bar is the `hasBar` readout strip. So a `Shell`
    /// tab paints the info footer and **not** the readout strip.
    #[gpui::test]
    fn a_shell_tab_paints_the_info_footer_and_not_the_agent_readout_strip(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);

        assert_eq!(
            app.read_with(cx, |app, _| app
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("the spawned agent")
                .kind),
            ProcessKind::Shell,
            "premise: `spawn_and_select` really spawns a plain shell, not an agent CLI"
        );
        assert!(
            cx.debug_bounds("pty-info-footer").is_some(),
            "a shell tab keeps the terminal pane's own bottom bar - the mock's `isTerminal` \
             branch, and issue #20's \"the terminal footer owns Clear\""
        );
        assert!(
            cx.debug_bounds("pty-footer").is_none(),
            "and it must not also paint the agent readout strip underneath it. \u{a7}4t's \"the \
             bar now renders whenever there is an agent\" is about *status*, not pane kind - the \
             mock gates the whole strip on `isChat: isAgent(tab)`, and \u{a7}4u\u{2032} accepts \
             that the budget popover is unreachable \"on a terminal tab\" precisely because a \
             terminal has no such strip"
        );
        assert!(
            cx.debug_bounds("pty-footer-cost").is_none()
                && cx.debug_bounds("pty-footer-budget").is_none(),
            "and neither of the strip's readouts may leak into a shell tab on their own"
        );
    }

    /// The other half of the same split: a real `Claude`/`Codex` tab paints the readout strip and
    /// **not** the terminal's info footer - and its `pid` is not lost with that bar, because the
    /// mock's `isChat` header carries it (`{{ focus.cli }}  pid {{ focus.pid }}`).
    #[gpui::test]
    fn an_agent_tab_paints_only_the_readout_strip_and_keeps_its_pid_in_the_header(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);
        app.update(cx, |app, cx| {
            app.agents.set_kind_for_test(id, ProcessKind::claude());
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, cx| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == id)
                    .expect("the spawned agent")
                    .pane
                    .read(cx)
                    .pid()
                    .is_some()
            }),
            "premise: the real spawned child has a real pid to show"
        );
        assert!(
            cx.debug_bounds("pty-footer").is_some(),
            "an agent tab keeps \u{a7}4t's readout strip"
        );
        assert!(
            cx.debug_bounds("pty-info-footer").is_none(),
            "and must not also paint the terminal pane's pid/dimensions/clear bar above it - the \
             mock's `isChat` branch has no such bar at all"
        );
        assert!(
            cx.debug_bounds("pty-header-pid").is_some(),
            "the pid moves to the header rather than being lost with the info footer - \
             `{{ focus.cli }}  pid {{ focus.pid }}` in the mock's `isChat` header"
        );
    }

    /// **Live report: "the height of the agent terminal is not good... it get a scrollbar" against
    /// a genuinely long real Claude agent transcript** - investigated for both of this project's
    /// two prior sizing-bug classes (#368's fresh-pane grid/pty measurement race, #356's stacked
    /// header/footer chrome) and a third (a font-fallback-driven row-height blowup); none
    /// reproduced under real, repeated measurement. This is the real, direct check of the
    /// remaining candidate root cause `debug_bounds` can answer: does this pane's own real,
    /// painted geometry - header band + the terminal's own content band + its bottom bar - ever
    /// stop summing to the pane's real total height, on a pane that has had genuinely long real
    /// output (well past #356's chrome rebuild and #368's fresh-pane latch, both scoped to a
    /// pane's very first frames)? A gap here would mean the grid is sized against a region
    /// different from what is really on screen - exactly the shape "the height is not good" would
    /// take, independent of whether a scrollbar is also, separately, correct.
    #[gpui::test]
    fn header_content_and_footer_bands_sum_to_the_real_pane_height_on_a_long_transcript(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);
        app.update(cx, |app, cx| {
            app.agents.set_kind_for_test(id, ProcessKind::claude());
            cx.notify();
        });
        cx.run_until_parked();

        let pane = app
            .read_with(cx, |app, _| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == id)
                    .map(|agent| agent.pane.clone())
            })
            .expect("the spawned agent");

        // A genuinely long real transcript - matches the user's own repro (~165 numbered lines),
        // well past one screen's worth of content, so a scrollbar is *expected*; what this test
        // checks is whether the pane's own real geometry stays self-consistent, not whether
        // scrolling exists at all.
        pane.update(cx, |pane, cx| {
            for i in 1..=165 {
                pane.inject_bytes_for_test(format!("line {i}\r\n").as_bytes(), cx);
            }
        });
        cx.run_until_parked();

        // Checked once at the pane's initial size, and again below after a *real* window resize
        // on this now-long-running pane - hypothesis: does the grid's own row/col count, and the
        // chrome around it, stay correct across a real resize once a pane has real accumulated
        // output, or can it drift/go stale the way a fresh, empty pane's once did (#368)?
        let check = |cx: &mut gpui::VisualTestContext, when: &str| {
            let surface = cx
                .debug_bounds("pty-surface")
                .unwrap_or_else(|| panic!("the pty surface must paint ({when})"));
            let header = cx
                .debug_bounds("pty-header")
                .unwrap_or_else(|| panic!("the header must paint ({when})"));
            let content = cx
                .debug_bounds("pty-surface-content")
                .unwrap_or_else(|| panic!("the content band must paint ({when})"));
            let footer = cx.debug_bounds("pty-footer").unwrap_or_else(|| {
                panic!("a real agent tab's own readout strip must paint ({when})")
            });
            let terminal = cx
                .debug_bounds("terminal-pane")
                .unwrap_or_else(|| panic!("the terminal pane itself must paint ({when})"));

            let summed_height = header.size.height + content.size.height + footer.size.height;
            assert!(
                (summed_height.as_f32() - surface.size.height.as_f32()).abs() < 1.0,
                "{when}: header ({:?}) + content ({:?}) + footer ({:?}) = {summed_height:?} must \
                 sum to the pane's real total height {:?}, with no unaccounted-for gap or overlap",
                header.size.height,
                content.size.height,
                footer.size.height,
                surface.size.height,
            );

            // The terminal itself must exactly fill the content band it was given - not claim
            // less (dead space) or more (painting over the header/footer).
            assert_eq!(
                terminal.size, content.size,
                "{when}: the terminal pane must exactly fill its own content band, got \
                 terminal={terminal:?} content={content:?}"
            );
        };
        check(cx, "at initial size, after 165 real lines");

        // The terminal's *own* internally-measured content-area bounds - what
        // `TerminalPane::maybe_resize_pty` actually sizes the grid from - must match its
        // externally-measured painted bounds. If these ever disagreed, the grid would be sized
        // for a region different from what is really on screen.
        let assert_internal_matches_external = |cx: &mut gpui::VisualTestContext, when: &str| {
            let terminal = cx.debug_bounds("terminal-pane").expect("checked above");
            let internal = pane
                .read_with(cx, |pane, _| pane.content_bounds_for_test())
                .expect("the pane must have painted at least once");
            assert!(
                (internal.size.width.as_f32() - terminal.size.width.as_f32()).abs() < 1.0
                    && (internal.size.height.as_f32() - terminal.size.height.as_f32()).abs() < 1.0,
                "{when}: the terminal's own internal measurement {internal:?} must match its \
                 real, externally painted bounds {terminal:?}"
            );
        };
        assert_internal_matches_external(cx, "at initial size, after 165 real lines");

        // A real resize - a window resize, a panel opening, a sidebar toggle all land here -
        // happening *after* this pane has real, long-accumulated output, not just at spawn.
        let initial_size = cx.debug_bounds("pty-surface").expect("checked above").size;
        let resized = gpui::size(
            initial_size.width - px(280.0),
            initial_size.height - px(140.0),
        );
        cx.simulate_resize(resized);
        // A couple of parked passes - matching `maybe_resize_pty`'s own documented one-frame
        // measurement lag - not the many an unfixed pane would actually need (it never caught up
        // on its own at all; see the fix this test guards).
        cx.run_until_parked();
        cx.run_until_parked();

        check(cx, "after a real resize on a long-running pane");
        assert_internal_matches_external(cx, "after a real resize on a long-running pane");

        // And the grid itself must have really followed - immediately, without needing any
        // *unrelated* event (e.g. new pty output) to force a fresh render first: its own
        // reported column/row count must be consistent with the terminal's newly measured
        // content area and cell size, not left over from before the resize.
        let (cell_size, (cols, rows), content_bounds) = pane.update_in(cx, |pane, window, _cx| {
            (
                pane.cell_size_for_test(window),
                pane.grid_dimensions(),
                pane.content_bounds_for_test()
                    .expect("the pane must have painted at least once"),
            )
        });
        let expected_cols =
            ((content_bounds.size.width.as_f32() - 16.0) / cell_size.width.as_f32()) as u16;
        let expected_rows =
            ((content_bounds.size.height.as_f32() - 16.0) / cell_size.height.as_f32()) as u16;
        assert!(
            cols.abs_diff(expected_cols) <= 1 && rows.abs_diff(expected_rows) <= 1,
            "after a real resize on a long-running pane, the grid's own dimensions ({cols}x{rows}) \
             must track its newly measured content area (expected ~{expected_cols}x{expected_rows} \
             from content_bounds={content_bounds:?}, cell_size={cell_size:?}) - not be stale from \
             before the resize"
        );
    }

    /// `showAgentBar: noAgents || activeWt.agents.indexOf(tab) >= 0`, and the mock's own comment:
    /// "The whole row is agent identity, so it belongs to agent panes only… **in a terminal pane
    /// there is no agent to describe.** Kept when the worktree has no agents at all, since it
    /// holds that empty state's CTA."
    ///
    /// #295 listed "the bar stays scoped to agent panes" as an acceptance criterion. It was not
    /// true: a shell tab in a worktree that *does* have agents wore the whole identity row.
    #[gpui::test]
    fn a_shell_tab_beside_a_real_agent_gets_no_identity_bar(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.worktrees = vec![worktree_row(repo.path().to_path_buf(), "main")];
            app.select_worktree(0, window, cx);
        });
        let agent_id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);
        app.update(cx, |app, cx| {
            app.agents
                .set_kind_for_test(agent_id, ProcessKind::claude());
            cx.notify();
        });
        let shell_id = spawn_and_select(&app, cx, repo.path().to_path_buf(), None);
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.current_worktree_is_bare()),
            "premise: this worktree really does hold a real agent session, so the `noAgents` \
             clause that legitimately keeps the bar does not apply"
        );
        assert!(
            cx.debug_bounds("agent-context-bar").is_none(),
            "a shell tab in a worktree with agents describes no agent - the identity row must \
             not paint over it"
        );
        assert!(
            cx.debug_bounds("pty-info-footer").is_some(),
            "premise: the shell tab really is the one showing in the centre pane"
        );

        app.update_in(cx, |app, window, cx| app.select_agent(agent_id, window, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("agent-context-bar").is_some(),
            "and switching back to the real agent tab restores it - the bar is scoped, not deleted"
        );
        let _ = shell_id;
    }

    /// §4t: the cost is "blank for an agent that is not running". Not a dimmed `...`, not a
    /// fabricated `0% cpu` - nothing at all, which is what the `Option` return states.
    #[gpui::test]
    fn the_cost_readout_is_blank_for_an_agent_that_is_not_running(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.render_agent_cost_readout(false, Some(1234)).is_none(),
                "a pid that is no longer running has no cost to report"
            );
            assert!(
                app.render_agent_cost_readout(true, None).is_none(),
                "and a pane with no pid at all has nothing to sample"
            );
        });
    }

    /// §4e/§4r's one surviving pair, and its two-click safety: `failed` keeps `Retry` and
    /// `Discard worktree`, and the discard really arms before it destroys.
    ///
    /// Driven through the genuinely painted button (`debug_bounds` + `simulate_click`), not
    /// through `request_discard_worktree` directly - the point is that this strip still offers
    /// those two, and that `Open terminal` (which used to sit between them) does not paint.
    #[gpui::test]
    fn a_failed_run_keeps_retry_and_a_two_click_discard_and_nothing_else(cx: &mut TestAppContext) {
        let repo = init_repo();
        let worktree_container = tempfile::tempdir().expect("tempdir");
        let worktree_path = worktree_container.path().join("feature-wt");
        drop(worktree_container);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf8 path"),
            ],
        );

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        // `/bin/false` is a real child that really exits non-zero, so `Status::Fail` is derived
        // from a genuine `ProcessSignal::Exited { success: false }` rather than being set.
        let id = spawn_and_select(&app, cx, worktree_path.clone(), Some("/bin/false"));
        wait_for_exit(&app, cx, id);
        // Relabelled to a real agent kind: the strip this test measures is the *agent* pane's
        // (`isChat` in the mock), and a `Shell` tab gets the terminal info footer instead. The
        // failure is a genuine non-zero exit either way - only the chrome around it is gated.
        app.update(cx, |app, cx| {
            app.agents.set_kind_for_test(id, ProcessKind::claude());
            cx.notify();
        });
        cx.run_until_parked();

        let status = app.read_with(cx, |app, cx| {
            let agent = app
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .expect("the spawned agent");
            app.agent_status(agent, cx)
        });
        assert_eq!(status, Status::Fail, "premise: the run really failed");

        assert!(
            cx.debug_bounds("footer-action-Respawn").is_some(),
            "a failed run keeps its `Retry`"
        );
        let discard = cx
            .debug_bounds("footer-action-DiscardWorktree")
            .expect("and its two-click `Discard worktree`");

        cx.simulate_click(discard.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.discard_confirm_armed),
            Some(id),
            "the first click must only arm - this is the one irreversible thing in the pane"
        );
        assert!(
            worktree_path.exists(),
            "and an armed-but-unconfirmed discard must not have touched the real worktree"
        );
    }

    /// §4t, verbatim: "The empty-worktree case keeps its buttons: with no agent there is no
    /// keystroke to duplicate and no readout to show." `Open terminal` survives here and nowhere
    /// else in this pane (§4e: "the legitimate secondary CTA"), and it really spawns a shell.
    #[gpui::test]
    fn the_no_agent_empty_state_really_starts_a_terminal(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // Close whatever the app opened on startup, so the centre pane really is the empty state.
        let startup: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|agent| agent.id).collect()
        });
        app.update_in(cx, |app, window, cx| {
            for id in startup {
                app.close_agent(id, window, cx);
            }
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.iter().count()),
            0,
            "premise: no agents left, so the empty state is what paints"
        );

        assert!(
            cx.debug_bounds("empty-state-start-agent").is_some(),
            "the empty state keeps its primary `Start an agent` CTA"
        );
        let open_terminal = cx
            .debug_bounds("empty-state-open-terminal")
            .expect("and its secondary `Open terminal` CTA - §4e's one surviving home for it");

        cx.simulate_click(open_terminal.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                1,
                "clicking it must really spawn a shell, not decorate the empty state"
            );
            assert!(
                app.agents
                    .iter()
                    .all(|agent| agent.kind == ProcessKind::Shell),
                "and that spawn is a terminal, which is what the button says"
            );
        });
    }

    /// Revision R12 §3's one context-bar button that #295 does *not* remove: a bare worktree has
    /// no agent, so it has no agent verbs to offer and no worktree verbs to misplace - just the
    /// primary CTA that gives it one.
    #[gpui::test]
    fn a_bare_worktree_keeps_its_start_an_agent_button(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        spawn_and_select(&app, cx, repo.path().to_path_buf(), None);

        // A real bare worktree row, selected - the exact condition
        // `AdeApp::current_worktree_is_bare` reads.
        app.update_in(cx, |app, window, cx| {
            app.worktrees = vec![WorktreeItem {
                path: repo.path().to_path_buf(),
                label: "bare".to_string(),
                branch: None,
                is_main: true,
                is_bare: true,
                is_detached: false,
                short_sha: None,
                is_locked: false,
                lock_reason: None,
                is_broken: false,
                broken_reason: None,
                error: None,
            }];
            app.select_worktree(0, window, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("context-bar-start-agent").is_some(),
            "a bare worktree's context bar keeps `Start an agent` - it is not one of the \
             worktree verbs §4e removed"
        );
    }
}
