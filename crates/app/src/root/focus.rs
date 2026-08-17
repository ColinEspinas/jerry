//! Opens/closes Surface C (Diff/File), the command palette, and Settings. All three are
//! overlay-shaped: each has its own `*_focus_handle` and `*_focus: OverlayFocus` pair, moves
//! real focus onto its handle on open, and must restore focus through [`OverlayFocus`]/
//! [`restore_focus`] on close - see those types' docs in `root::mod` for the dangling-focus
//! invariant this file exists to satisfy. This project has hit "close forgot to restore" bugs
//! repeatedly; the fix each time was routing through that shared mechanism
//! rather than hand-rolling capture/restore again, so any new overlay added here should do the
//! same instead of re-deriving it.

use super::*;

impl AdeApp {
    /// The one focus target this window is *always* rendering right now - [`restore_focus`]'s
    /// last-resort landing spot when an overlay closes with nothing real left to hand focus back
    /// to, and the answer to GitHub issue #255 ("sometimes the command palette can't be opened
    /// like when there is no tab open").
    pub(crate) fn focus_fallback_handle(&self) -> FocusHandle {
        if self.settings_open {
            self.settings_focus_handle.clone()
        } else if self.focused_repo().is_none() {
            self.empty_state_focus_handle.clone()
        } else {
            self.rail_focus_handle.clone()
        }
    }

    /// Captures the pre-open focus target only on the closed-to-open transition
    /// (`Self::open_change` was `None`) - a second file opened while one is already showing must
    /// not overwrite the real original target with `Self::code_focus_handle` itself (already
    /// focused by then). Always moves focus onto [`Self::code_focus_handle`] regardless.
    pub(crate) fn focus_code_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_change.is_none() && !self.focus_is_on_an_overlay(window, cx) {
            self.code_focus.capture(window, &self.agents, cx);
        }
        window.focus(&self.code_focus_handle, cx);
    }

    /// Whether keyboard focus is currently on one of this app's overlay handles - the palette,
    /// Settings, or the "New file" prompt. See [`Self::focus_code_surface`] for why capturing one
    /// as a return target is always wrong.
    pub(crate) fn focus_is_on_an_overlay(&self, window: &Window, cx: &App) -> bool {
        window.focused(cx).is_some_and(|focused| {
            focused == self.palette_focus_handle
                || focused == self.settings_focus_handle
                || focused == self.new_file_focus_handle
        })
    }

    /// Opens the command palette (⌘P): resets query/scope/selection to a fresh "browse
    /// everything" state, captures the pre-open focus target into [`Self::palette_focus`], and
    /// moves focus onto [`Self::palette_focus_handle`]. Also disarms a pending rail prune
    /// confirmation ([`Self::prune_confirm_armed`]) - opening the palette counts as "did
    /// something else".
    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        // Every floating menu is an unconditional sibling of the palette - see
        // `crate::root::menus`. This used to close only the `+` menu and the title bar's
        // dropdown by hand, which meant opening the palette on top of an open commit-composer or
        // git-graph menu left that popover painted over it (GitHub issue #176).
        let _ = self.close_menu_surfaces_except(None);
        // Not a `MenuSurface`: the "New file" prompt is a focus-owning modal, closed here for the
        // separate reason `Self::new_file_input`'s own docs give.
        self.new_file_input = None;
        self.palette_focus.capture(window, &self.agents, cx);
        self.palette_scope = palette::PaletteScope::default();
        // A reopened palette always starts on its root list, never inside a half-answered
        // drill-down step (`crate::palette::state::PaletteStep`).
        self.palette_step = palette::PaletteStep::Root;
        // A reopened palette is a genuinely new widget instance, so its predecessor's undo
        // history must not be reachable from it - `reset`, not `clear` (which is itself a real,
        // undoable step). See `crate::text_history::TextField`'s own docs.
        self.palette_query.reset();
        self.palette_selected = 0;
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        window.focus(&self.palette_focus_handle, cx);
        cx.notify();
    }

    /// Closes the palette overlay (scrim click, Esc, or running a result that moved no focus)
    /// and restores focus via [`restore_focus`]. A result that *did* move focus closes through
    /// [`Self::close_palette_keeping_result_focus`] instead - see
    /// [`crate::palette::render::AdeApp::run_selected_palette_entry`].
    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_step = palette::PaletteStep::Root;
        if self.settings_open {
            // Settings is showing underneath (either `OpenSettings` just opened it, or the
            // palette was opened while Settings was already open and is now dismissing back
            // down to it) - focus belongs on `settings_focus_handle`, the same handle
            // `open_settings` itself uses, not on `palette_focus`'s restore target or the
            // active agent's pane (neither is actually rendered right now).
            window.focus(&self.settings_focus_handle, cx);
            self.palette_focus.clear();
            cx.notify();
            return;
        }
        let fallback = self.focus_fallback_handle();
        restore_focus(&self.agents, &mut self.palette_focus, fallback, window, cx);
        cx.notify();
    }

    /// Closes the palette *without* touching focus - for the one case
    /// [`crate::palette::render::AdeApp::run_selected_palette_entry`] detects: the entry that
    /// just ran moved keyboard focus onto its own result, and that is where focus belongs
    /// (GitHub issue #15's "an action focuses its result"). See that method's docs for how the
    /// two closing paths are chosen between.
    pub(crate) fn close_palette_keeping_result_focus(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_step = palette::PaletteStep::Root;
        self.palette_focus.clear();
        cx.notify();
    }

    /// Opens the Settings surface - same capture-and-focus shape as [`Self::open_palette`].
    /// Unlike the palette, this does **not** reset [`Self::settings_page`]: which page was
    /// showing persists across opens, matching ordinary settings-window UX. Closes the palette
    /// first (via [`Self::close_palette`], run while `settings_open` is still `false` so that
    /// call takes its normal, non-Settings-aware restore path) if it happened to be open too.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(window, cx);
        }
        // Settings *replaces* the workspace body, so any floating menu left open would either
        // float over the Settings page or keep painting its own full-window scrim over it,
        // swallowing the first click a user aimed at it (an adversarial audit's own finding, for
        // the git graph's two menus in particular: unlike opening a *different* tab, opening
        // Settings does not clear `graph_tab_active`, so `leave_graph_tab` never runs). One sweep
        // now, across all six - see `crate::root::menus`; this used to be three separate hand-kept
        // lists that each covered a different subset (GitHub issue #176).
        let _ = self.close_menu_surfaces_except(None);
        self.new_file_input = None;
        // GitHub issue #241: the graph tab's branch-name prompt (the row menu's "Create branch
        // here" and the branch menu's "Rename Branch…" share one) is the same kind of
        // focus-owning modal overlay `new_file_input` is (not a `MenuSurface`, for the identical
        // reason documented on that enum), so it needs the same explicit clear here.
        self.graph_state.branch_prompt = None;
        // Not a `MenuSurface`: a half-typed inline name would keep the tree's `"tree-editing"` key
        // context alive on a node that is no longer rendered (GitHub issue #19). The armed
        // *delete confirmation* is deliberately left alone: it is a real, window-level modal the
        // user is mid-way through answering, and `crate::root::AdeApp::render` keeps it hidden
        // while Settings is up rather than silently disarming it.
        self.tree_inline_edit = None;
        self.settings_open = true;
        self.settings_focus.capture(window, &self.agents, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.custom_theme_remove_armed = None;
        window.focus(&self.settings_focus_handle, cx);
        self.load_agent_rows(cx);
        self.load_lsp_rows(cx);
        // GitHub issue #213: the General page's shell field shows a real found/not-found hint,
        // and `$PATH` (or the file it names) can have changed since the last time Settings was
        // open - re-probe once here, on open, rather than per render.
        self.refresh_shell_status();
        // Same reasoning for that field's suggestion list (issue #213's follow-up): detected here,
        // on a real gesture, so the dropdown has real entries the instant it is opened rather than
        // needing a frame of detection. Its own open flag is left alone - opening Settings does
        // not open a dropdown.
        self.refresh_shell_suggestions();
        cx.notify();
    }

    /// Closes the Settings surface (the nav header's Esc keycap, or a real Esc keystroke via
    /// [`Self::handle_settings_key_down`]) and restores focus via [`restore_focus`].
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        // The Shell field's suggestion dropdown belongs to the page being left (GitHub issue
        // #213's follow-up). `AdeApp::render` already gates it on `settings_open`, so this is not
        // what keeps it off the workspace - it is what stops it from silently reappearing the next
        // time Settings is opened, still holding the state a user walked away from.
        self.shell_suggestions_open = false;
        // A live keybinding-recording intercept (`Self::_keymap_intercept`) is a real, global
        // `App::intercept_keystrokes` subscription - it must never survive leaving the Settings
        // surface, or every keystroke in the whole app would keep being silently swallowed.
        self.cancel_keybinding_recording(cx);
        let fallback = self.focus_fallback_handle();
        restore_focus(&self.agents, &mut self.settings_focus, fallback, window, cx);
        cx.notify();
    }
}

/// Interactive regression coverage for the palette's ⌘P entry point, driven through GPUI's
/// `TestAppContext`/`VisualTestContext` harness (a real window, focus tracking, action dispatch
/// and keystroke simulation). A plain unit test can't catch this bug class: it requires a real
/// window with real GPUI dispatch to reproduce a dangling `Window::focus`.
#[cfg(test)]
pub(crate) mod palette_focus_tests {
    use super::*;
    use gpui::TestAppContext;

    /// The window fixture now lives in [`crate::test_support`]; re-exported here so the call
    /// sites that still say `palette_focus_tests::open_test_app` keep resolving while they are
    /// migrated (GitHub issue #425).
    pub(crate) use crate::test_support::open_test_app;

    #[gpui::test]
    fn toggle_palette_reopens_after_being_closed(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "first secondary-p should open the palette"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "second secondary-p should close the palette"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "third secondary-p - reopening after a close - is exactly the case that was broken: \
             without restoring real focus in close_palette, this dispatch had nowhere real to \
             land and silently did nothing"
        );
    }

    #[gpui::test]
    fn scope_prefix_only_fires_on_an_empty_query(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        cx.simulate_input(">");
        app.read_with(cx, |app, _| {
            assert_eq!(app.palette_scope, palette::PaletteScope::Commands);
            assert_eq!(app.palette_query.as_str(), "");
        });

        // Back to a fresh, empty-query palette state before the mid-query case.
        cx.dispatch_action(TogglePalette);
        cx.dispatch_action(TogglePalette);
        app.read_with(cx, |app, _| {
            assert_eq!(app.palette_scope, palette::PaletteScope::All);
            assert_eq!(app.palette_query.as_str(), "");
        });

        cx.simulate_input("x");
        cx.simulate_input(">");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.palette_scope,
                palette::PaletteScope::All,
                "a `>` typed mid-query (query is non-empty) must not switch scope"
            );
            assert_eq!(app.palette_query.as_str(), "x>");
        });
    }

    #[gpui::test]
    fn toggle_palette_still_works_after_a_palette_spawned_new_shell(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        let initial_agent_id = app.read_with(cx, |app, _| app.agents.active_id());

        cx.dispatch_action(TogglePalette);
        app.update_in(cx, |app, window, cx| {
            app.execute_palette_command(palette::PaletteCommand::NewShell, window, cx);
        });
        // `execute_palette_command` alone (as used directly here) doesn't close the palette -
        // that's `run_selected_palette_entry`'s own job - so close it the same way Escape does,
        // to reach the exact `close_palette` code path under test.
        app.update_in(cx, |app, window, cx| {
            app.close_palette(window, cx);
        });

        let new_agent_id = app.read_with(cx, |app, _| app.agents.active_id());
        assert_ne!(
            initial_agent_id, new_agent_id,
            "sanity check: New Shell should have made a different agent active"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "secondary-p after a palette-spawned New Shell should still open the palette - the \
             center pane now renders a different agent's terminal pane than the one focus \
             was captured from, so close_palette must not restore that now-stale handle"
        );
    }

    /// Unlike every other test in this module, which dispatches `TogglePalette` directly, this
    /// binds `crate::default_key_bindings()` via `App::bind_keys`
    /// (`vendor/zed/crates/gpui/src/app.rs:2130`) and simulates the real keystroke via
    /// `VisualTestContext::simulate_keystrokes` (`vendor/zed/crates/gpui/src/app/
    /// test_context.rs:794`) - so a wrong keystroke spec in `default_key_bindings` (e.g. `cmd-p`,
    /// which GPUI resolves to Super/Windows on Linux, never Ctrl) fails this test even though a
    /// direct `dispatch_action` test would stay green. `secondary_p` tracks the same
    /// `cfg!(target_os = "macos")` resolution `default_key_bindings` itself uses, rather than
    /// hardcoding `ctrl-p`.
    #[gpui::test]
    fn secondary_keystroke_opens_the_palette_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        let secondary_p = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(secondary_p);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real, simulated {secondary_p} keystroke - not a direct TogglePalette action \
             dispatch - must open the palette through crate::default_key_bindings' real \
             KeyBinding registration; this is exactly the path the old \"cmd-k\" binding broke \
             on Linux (Ctrl+K did nothing) without any test catching it"
        );
    }
}

/// Regression coverage for the tab strip's global keybindings (`ctrl-shift-T`,
/// `secondary-shift-n`, `]`, `secondary-1`..`secondary-8`), using the same
/// `bind_keys`-plus-`simulate_keystrokes` shape as
/// [`palette_focus_tests::secondary_keystroke_opens_the_palette_through_the_real_key_bindings`] -
/// a test that only dispatches the action directly can't catch a wrong keystroke string in
/// `crate::default_key_bindings`.
#[cfg(test)]
mod tab_strip_keybinding_tests {
    use super::*;
    use gpui::TestAppContext;

    fn bind_real_keys(cx: &mut gpui::VisualTestContext) {
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
    }

    #[gpui::test]
    fn opening_the_palette_or_settings_closes_an_already_open_plus_menu(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.dispatch_action(ToggleSettings);
        assert!(app.read_with(cx, |app, _| app.settings_open));
        assert!(
            !app.read_with(cx, |app, _| app.plus_menu_open),
            "opening Settings should have closed the still-open plus menu"
        );

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.dispatch_action(TogglePalette);
        assert!(app.read_with(cx, |app, _| app.palette_open));
        assert!(
            !app.read_with(cx, |app, _| app.plus_menu_open),
            "and opening the palette should have closed it too"
        );
    }

    #[gpui::test]
    fn ctrl_shift_t_spawns_a_real_shell_agent_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let agents_before = app.read_with(cx, |app, _| app.agents.iter().count());

        cx.simulate_keystrokes("ctrl-shift-t");

        let (agents_after, active_kind) = app.read_with(cx, |app, _| {
            (
                app.agents.iter().count(),
                app.agents.active().map(|agent| agent.kind),
            )
        });
        assert_eq!(
            agents_after,
            agents_before + 1,
            "a real, simulated ctrl-shift-t keystroke should have spawned exactly one new \
             agent through crate::default_key_bindings' real ctrl-shift-t -> NewTerminal \
             binding"
        );
        assert_eq!(
            active_kind,
            Some(ProcessKind::Shell),
            "New terminal always spawns a real Shell agent"
        );
    }

    #[gpui::test]
    fn secondary_shift_n_spawns_a_real_agent_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let agents_before = app.read_with(cx, |app, _| app.agents.iter().count());

        let secondary_shift_n = if cfg!(target_os = "macos") {
            "cmd-shift-n"
        } else {
            "ctrl-shift-n"
        };
        cx.simulate_keystrokes(secondary_shift_n);
        // `AdeApp::new_agent_pane`'s real `$PATH` detection runs on the background executor -
        // drive it to completion before checking the real result, the same real reason every
        // other background-dispatching test in this crate calls `run_until_parked`.
        cx.run_until_parked();

        let agents_after = app.read_with(cx, |app, _| app.agents.iter().count());
        assert_eq!(
            agents_after,
            agents_before + 1,
            "a real, simulated {secondary_shift_n} keystroke should have spawned exactly one \
             new agent through crate::default_key_bindings' real \
             secondary-shift-n -> NewAgentPane binding"
        );
    }

    #[gpui::test]
    fn ctrl_p_opens_the_palette_even_while_a_terminal_is_focused(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        // Explicitly focus the initial shell agent - the real, concrete "a terminal pane
        // genuinely has keyboard focus" state this test's own name promises, rather than relying
        // on whatever `AdeApp::new_with_settings` happens to leave focus on by default.
        app.update_in(cx, |app, window, cx| {
            app.agents.focus_active(window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.agents.active_id().is_some()),
            "sanity check: a real terminal agent must be focused for this test to mean anything"
        );
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "sanity check: the palette must start closed"
        );

        let secondary_p = secondary_p();
        cx.simulate_keystrokes(secondary_p);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real, simulated {secondary_p} keystroke must open the palette - it is now the \
             real, unscoped global TogglePalette binding (crate::default_key_bindings' own \
             docs), even with a terminal focused; the terminal's own readline Ctrl+P is \
             shadowed as a deliberate, accepted tradeoff, not a bug"
        );
    }

    /// The palette's real global shortcut, matching `crate::default_key_bindings`' own
    /// `cfg!(target_os = "macos")` resolution for `"secondary-p"` rather than hardcoding
    /// `ctrl-p`/`cmd-p`.
    fn secondary_p() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        }
    }

    #[gpui::test]
    fn ctrl_p_still_works_after_ctrl_shift_t_with_a_file_tab_active(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        std::fs::write(repo.path().join("a.txt"), "hello\n").expect("write a.txt");
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("a.txt"), window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_some()),
            "sanity check: a file tab should now be active"
        );

        cx.simulate_keystrokes("ctrl-shift-t");
        assert!(
            app.read_with(cx, |app, _| app.agents.iter().count()) >= 2,
            "sanity check: ctrl-shift-t should have spawned a real new agent"
        );

        let key = secondary_p();
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after ctrl-shift-t, with a file tab active, must still open \
             the palette - before the fix, Agents::spawn's unconditional focus pointed \
             Window::focus at the new agent's pane even though render_center_pane was still \
             showing the file tab, leaving no real dispatch path to any on_action handler"
        );
    }

    #[gpui::test]
    fn ctrl_p_still_works_after_closing_the_active_agent_tab(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let first_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        });
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_agent(first_id, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(first_id),
            "sanity check: the first agent should be active again before closing it"
        );

        app.update_in(cx, |app, window, cx| {
            app.close_agent(first_id, window, cx);
        });

        let key = secondary_p();
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after closing the active agent's own tab must still open \
             the palette - Agents::close must move real keyboard focus onto whichever agent \
             became active as a result, not leave Window::focus dangling"
        );
    }

    #[gpui::test]
    fn ctrl_p_still_works_after_archiving_the_active_agent(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let first_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        });
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_agent(first_id, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(first_id),
            "sanity check: the first agent should be active again before archiving it"
        );

        app.update_in(cx, |app, window, cx| {
            app.archive_agent(first_id, window, cx);
        });

        let key = secondary_p();
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after archiving the active agent must still open the \
             palette - archiving goes through the same Agents::close real focus-restore path \
             closing a tab does"
        );
    }

    #[gpui::test]
    fn bracket_advances_to_the_next_changed_file_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        // `]` is scoped to `Some("diff")` (`crate::default_key_bindings`), not global - a global
        // bare `]` would swallow a literal `]` typed into any focused terminal instead of
        // forwarding it to the pty. This opens a file first (`AdeApp::open_change_diff`) to
        // establish "diff"-context focus before simulating the keystroke.
        let repo = crate::test_support::temp_root();
        test_support::git(repo.path(), &["init", "-b", "main"]);
        test_support::git(repo.path(), &["config", "user.email", "test@example.com"]);
        test_support::git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\n").expect("write b.txt");
        test_support::git(repo.path(), &["add", "."]);
        test_support::git(repo.path(), &["commit", "-m", "initial"]);
        test_support::git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\nchanged\n").expect("rewrite b.txt");

        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        bind_real_keys(cx);

        let order: Vec<PathBuf> = app.read_with(cx, |app, _| {
            app.current_diff()
                .expect("a real diff against main should have loaded")
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect()
        });
        assert_eq!(order.len(), 2, "sanity check: both files should be changed");

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(order[0].clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[0].clone())
        );

        cx.simulate_keystrokes("]");

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[1].clone()),
            "a real, simulated ] keystroke, with a real file tab already focused, must advance \
             to the next real changed file through crate::default_key_bindings' real, \
             \"diff\"-scoped ] -> NextChangedFile binding"
        );
    }

    #[gpui::test]
    fn bracket_does_not_fire_globally_while_a_terminal_is_focused(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        test_support::git(repo.path(), &["init", "-b", "main"]);
        test_support::git(repo.path(), &["config", "user.email", "test@example.com"]);
        test_support::git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        test_support::git(repo.path(), &["add", "."]);
        test_support::git(repo.path(), &["commit", "-m", "initial"]);
        test_support::git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");

        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        bind_real_keys(cx);

        assert!(
            app.read_with(cx, |app, _| app
                .current_diff()
                .is_some_and(|diff| !diff.files.is_empty())),
            "sanity check: a real diff with at least one changed file must actually have loaded, \
             or the negative assertion below would trivially pass for the wrong reason (nothing \
             to navigate to at all, rather than the real ] scoping doing its job)"
        );

        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);

        cx.simulate_keystrokes("]");

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            None,
            "with only a terminal focused (no real \"diff\" context anywhere in the dispatch \
             path), a real ] keystroke must not open anything - it should be free to reach the \
             focused terminal as literal input instead"
        );
    }

    #[gpui::test]
    fn secondary_3_jumps_to_the_third_real_agent_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        // Tab 1 is `open_test_app`'s own startup shell. Four more, then retagged so the strip
        // reads `[shell, claude, shell, codex, claude]` - every process is a real spawned shell,
        // `set_kind_for_test` only changes what this app *calls* each one, which is exactly the
        // discriminator under test.
        for _ in 0..4 {
            app.update_in(cx, |app, window, cx| {
                app.new_agent(ProcessKind::Shell, window, cx);
            });
        }
        let tab_ids: Vec<crate::work_surface::agents::AgentId> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|agent| agent.id).collect()
        });
        assert_eq!(tab_ids.len(), 5, "five real tabs should exist by now");
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(tab_ids[1], ProcessKind::claude());
            app.agents
                .set_kind_for_test(tab_ids[3], ProcessKind::codex());
            app.agents
                .set_kind_for_test(tab_ids[4], ProcessKind::claude());
        });
        let third_agent_id = tab_ids[4];
        let third_tab_id = tab_ids[2];

        assert_eq!(
            app.read_with(cx, |app, _| app.current_worktree_agent_sessions().count()),
            3,
            "premise: exactly three real agent sessions exist (not five tabs) for \
             secondary-1..secondary-3 to have anything to number"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_agent(tab_ids[0], window, cx);
        });

        let secondary_3 = if cfg!(target_os = "macos") {
            "cmd-3"
        } else {
            "ctrl-3"
        };
        cx.simulate_keystrokes(secondary_3);

        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(third_agent_id),
            "a real, simulated {secondary_3} keystroke must activate the *third agent session* \
             through crate::default_key_bindings' real secondary-3 -> JumpToAgent3 binding"
        );
        assert_ne!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(third_tab_id),
            "it must not activate the third *tab*, which is a plain shell - a terminal is not an \
             agent and takes no jump number (GitHub issue #381)"
        );
    }

    #[gpui::test]
    fn jump_to_agent_at_activates_the_right_agent_by_position(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        let mut ids = vec![app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        })];
        for _ in 0..3 {
            let id = app.update_in(cx, |app, window, cx| {
                app.agents.spawn(
                    ProcessKind::Shell,
                    repo.path().to_path_buf(),
                    app.settings.appearance.terminal_font_size,
                    app.settings.terminal.shell_override(),
                    None,
                    window,
                    cx,
                )
            });
            ids.push(id);
        }
        app.update(cx, |app, _cx| {
            for id in &ids {
                app.agents.set_kind_for_test(*id, ProcessKind::claude());
            }
        });

        for (position, expected_id) in ids.iter().enumerate() {
            let position = position + 1;
            app.update_in(cx, |app, window, cx| {
                app.jump_to_agent_at(position, window, cx);
            });
            assert_eq!(
                app.read_with(cx, |app, _| app.agents.active_id()),
                Some(*expected_id),
                "position {position} should activate agent {expected_id}"
            );
        }

        let active_before = app.read_with(cx, |app, _| app.agents.active_id());
        app.update_in(cx, |app, window, cx| {
            app.jump_to_agent_at(5, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            active_before,
            "jumping to a position with no real agent there must be a no-op"
        );
    }
}

/// Regression coverage for the Settings surface's lifecycle - the same dangling-focus bug class
/// `palette_focus_tests` covers, exercised against Settings instead. Settings is a bigger risk
/// for it than the palette: it *replaces* the whole three-zone body
/// ([`AdeApp::render_workspace_body`]) rather than drawing on top of it, so a broken restore
/// here leaves every bound action unreachable, not just one.
#[cfg(test)]
mod settings_focus_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn toggle_settings_action_opens_then_real_escape_closes_it_and_focus_stays_live(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "secondary-, should open the Settings surface"
        );

        cx.simulate_keystrokes("escape");
        assert!(
            !app.read_with(cx, |app, _| app.settings_open),
            "a real Esc keystroke, dispatched to whatever has real focus, should close Settings \
             - this only reaches AdeApp::handle_settings_key_down if track_focus/on_key_down \
             actually wired real focus onto the Settings surface"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "secondary-p after closing Settings must still reach handle_toggle_palette_action - the \
             exact bug class this module exists to catch is close_settings leaving \
             Window::focus dangling on settings_focus_handle instead of restoring it"
        );
    }

    /// With the old `"cmd-,"` binding, `Ctrl+,` was never recognized as matching any
    /// `KeyBinding` (`"cmd"`/`"super"`/`"win"` only ever mean the platform modifier, never Ctrl,
    /// on Linux - `vendor/zed/crates/gpui/src/platform/keystroke.rs`), so the keystroke fell
    /// through to whatever text input had focus and got typed as a literal `,`. This test binds
    /// `default_key_bindings()` and simulates the real keystroke, unlike every other test in
    /// this module which dispatches `ToggleSettings` directly and so couldn't catch this.
    #[gpui::test]
    fn secondary_comma_keystroke_opens_settings_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        let secondary_comma = if cfg!(target_os = "macos") {
            "cmd-,"
        } else {
            "ctrl-,"
        };
        cx.simulate_keystrokes(secondary_comma);

        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "a real, simulated {secondary_comma} keystroke must reach \
             handle_toggle_settings_action through crate::default_key_bindings' real KeyBinding \
             registration - the old \"cmd-,\" binding let this keystroke fall through unhandled \
             instead, which (outside this deterministic test harness) meant it was typed as a \
             literal ',' into whatever real text input had focus"
        );
    }

    #[gpui::test]
    fn closing_settings_leaves_open_agent_tabs_intact(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        let agents_before = app.read_with(cx, |app, _| app.agents.iter().count());
        let active_before = app.read_with(cx, |app, _| app.agents.active_id());
        assert_eq!(
            agents_before, 1,
            "AdeApp::new starts with exactly one real shell tab"
        );

        cx.dispatch_action(ToggleSettings);
        assert!(app.read_with(cx, |app, _| app.settings_open));

        cx.simulate_keystrokes("escape");
        assert!(!app.read_with(cx, |app, _| app.settings_open));

        let agents_after = app.read_with(cx, |app, _| app.agents.iter().count());
        let active_after = app.read_with(cx, |app, _| app.agents.active_id());
        assert_eq!(
            agents_after, agents_before,
            "the real agent tab opened before Settings was shown must still exist after \
             closing Settings"
        );
        assert_eq!(
            active_after, active_before,
            "the active tab must be unchanged by a Settings open/close round-trip"
        );
    }

    #[gpui::test]
    fn settings_page_selection_persists_across_a_close_and_reopen(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_page),
            SettingsPage::General,
            "a fresh window opens Settings on General"
        );
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Worktrees, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_page),
            SettingsPage::Worktrees
        );

        cx.simulate_keystrokes("escape");
        assert!(!app.read_with(cx, |app, _| app.settings_open));

        cx.dispatch_action(ToggleSettings);
        assert!(app.read_with(cx, |app, _| app.settings_open));
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_page),
            SettingsPage::Worktrees,
            "which page was showing should persist across a close/reopen, unlike the palette's \
             own query/scope which intentionally resets every open"
        );
    }

    #[gpui::test]
    fn open_settings_palette_command_leaves_real_focus_on_settings(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        app.update_in(cx, |app, window, cx| {
            app.execute_palette_command(palette::PaletteCommand::OpenSettings, window, cx);
        });
        // `execute_palette_command` alone doesn't close the palette (`run_selected_palette_entry`
        // does), so close it explicitly to reach the `close_palette` path under test.
        app.update_in(cx, |app, window, cx| {
            app.close_palette(window, cx);
        });

        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "the Open Settings command should have opened Settings"
        );
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "sanity check: the palette itself should be closed"
        );

        cx.simulate_keystrokes("escape");
        assert!(
            !app.read_with(cx, |app, _| app.settings_open),
            "a real Esc must still reach Settings' own key handler after this palette-driven \
             open - proof close_palette's Settings-aware branch left real focus on Settings, \
             not dangling on palette_focus_handle"
        );
    }

    #[gpui::test]
    fn opening_settings_populates_both_row_lists_from_a_background_path_search(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        app.read_with(cx, |app, _| {
            assert!(
                app.agent_rows.is_empty() && app.lsp_rows.is_empty(),
                "neither list may be populated before Settings has ever been opened - nothing \
                 should eagerly run a $PATH search that's only ever shown on those pages"
            );
        });

        cx.dispatch_action(ToggleSettings);
        cx.run_until_parked();

        let rows = app.read_with(cx, |app, _| app.agent_rows.clone());
        assert_eq!(
            rows.len(),
            settings::AGENT_KINDS.len(),
            "opening Settings should populate exactly one real row per AGENT_KINDS entry, the \
             same count the Agents page nav badge (`self.agent_rows.len()` via \
             render_settings_nav) shows"
        );
        for kind in settings::AGENT_KINDS {
            assert!(
                rows.iter().any(|row| row.kind == kind),
                "{kind:?} should have a real row after a real $PATH search"
            );
        }

        let lsp_rows = app.read_with(cx, |app, _| app.lsp_rows.clone());
        let languages = settings::lsp_languages();
        assert_eq!(lsp_rows.len(), languages.len());
        for def in languages {
            assert!(lsp_rows.iter().any(|row| row.language == def.language));
        }
    }

    /// Every page in `SettingsPage::ALL` must render without panicking. Running the test
    /// executor to a parked state (`cx.run_until_parked`) is what actually exercises
    /// `AdeApp::render_settings_content`'s per-page render, not just the state transition
    /// `select_settings_page` performs.
    #[gpui::test]
    fn every_settings_page_renders_without_panicking(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        cx.run_until_parked();

        for page in SettingsPage::ALL {
            app.update_in(cx, |app, window, cx| {
                app.select_settings_page(page, window, cx);
            });
            cx.run_until_parked();
            assert_eq!(
                app.read_with(cx, |app, _| app.settings_page),
                page,
                "{:?} should have actually become the selected page",
                page.label()
            );
        }
    }

    #[gpui::test]
    fn window_controls_style_change_updates_the_real_persisted_settings_field(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        assert_eq!(
            app.read_with(cx, |app, _| app.window_controls_style()),
            WindowControlsStyle::System,
            "System is the real documented default"
        );

        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.window_controls_style()),
            WindowControlsStyle::WindowsLinuxStyle
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.window.controls),
            WindowControlsStyle::WindowsLinuxStyle,
            "the real Settings struct field, not a second independent copy, must have changed"
        );
    }
}

/// The same dangling-focus bug class as `palette_focus_tests`/`settings_focus_tests`, this time
/// for Surface C's Diff/File view (`AdeApp::code_focus_handle`). Uses a plain `.txt` file
/// deliberately - the bug is about a file view being mounted at all, not about hover/
/// go-to-definition/LSP content.
#[cfg(test)]
mod code_focus_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    fn open_test_app_with_a_plain_text_file(
        cx: &mut TestAppContext,
    ) -> (Entity<AdeApp>, &mut gpui::VisualTestContext, PathBuf) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        (app, cx, file_path)
    }

    /// Every window-level action must still reach its real handler once a plain `.txt` File
    /// view is mounted. Without [`AdeApp::code_focus_handle`] tracking real focus there, these
    /// dispatches silently fall back to the window's internal dispatch root (GPUI's own
    /// `Window::focus_node_id_in_rendered_frame` falls back to `dispatch_tree.root_node_id()`
    /// whenever the focused `FocusId` isn't found in the last rendered frame) instead of
    /// reaching the handler at all.
    ///
    /// `GotoDefinition` is included even though a `.txt` file has no hover entry for it to act
    /// on: `handle_goto_definition_action`'s early return with `hover == None` is harmless, and
    /// what is under test is that dispatch reached the handler, not what it did once there.
    #[gpui::test]
    fn every_window_action_still_reaches_its_handler_with_a_file_view_open(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, file_path) = open_test_app_with_a_plain_text_file(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_some()),
            "sanity check: the File view should actually be showing"
        );

        cx.dispatch_action(ToggleSettings);
        assert!(app.read_with(cx, |app, _| app.settings_open));

        cx.dispatch_action(TogglePalette);
        assert!(app.read_with(cx, |app, _| app.palette_open));

        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
        cx.dispatch_action(GotoDefinition);
        cx.run_until_parked();
        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
    }

    #[gpui::test]
    fn closing_the_file_view_restores_real_focus_and_actions_keep_working(cx: &mut TestAppContext) {
        let (app, cx, file_path) = open_test_app_with_a_plain_text_file(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.close_change_diff(window, cx);
        });
        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);

        cx.dispatch_action(ToggleSettings);
        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "secondary-, must still reach handle_toggle_settings_action after closing the File view - \
             AdeApp::close_change_diff must have restored real focus onto the active agent's \
             terminal pane, not left it dangling on code_focus_handle"
        );
    }
}

/// GitHub issue #17 §1/§3: real, dispatched `secondary-z` routing across every overlapping-scope
/// case this app actually has. Deliberately `simulate_keystrokes`-driven throughout - the whole
/// risk here is *which handler a keystroke reaches*, which no state-assertion-only test can see.
#[cfg(test)]
mod text_undo_scoping_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    const SECONDARY_Z: &str = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    const SECONDARY_SHIFT_Z: &str = if cfg!(target_os = "macos") {
        "cmd-shift-z"
    } else {
        "ctrl-shift-z"
    };

    fn bind_real_keys(cx: &mut gpui::VisualTestContext) {
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
    }

    /// Opens `path` in the File view and drives the render/park cycle the background load needs
    /// before `edit_buffers` is really seeded - the same shape
    /// `crate::code_surface::editing::editing_tests::open_file_for_editing` uses.
    fn open_file_for_editing(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        path: PathBuf,
    ) {
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(path, window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
    }

    #[gpui::test]
    fn secondary_z_with_a_terminal_focused_does_not_reach_text_undo(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("notes.txt");

        cx.simulate_input("TYPED");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .unwrap()
                .content
                .clone()),
            "TYPEDhello\n"
        );

        // Close the file tab: `close_file_tab` deliberately keeps the `edit_buffers` entry (and
        // so its whole undo history) alive while restoring real focus to the active agent's
        // terminal pane - exactly the overlapping state this test needs.
        app.update_in(cx, |app, window, cx| {
            app.close_change_diff(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);
        assert!(
            app.read_with(cx, |app, _| app.edit_buffer_contains(&relative)),
            "sanity check: the buffer (and its history) must still be alive, or this test would \
             pass for the wrong reason"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        cx.simulate_keystrokes(SECONDARY_SHIFT_Z);
        cx.simulate_keystrokes("ctrl-y");

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .unwrap()
                .content
                .clone()),
            "TYPEDhello\n",
            "with a real terminal focused, secondary-z must not undo a background buffer's text"
        );
    }

    #[gpui::test]
    fn secondary_z_with_the_palette_open_undoes_the_query_not_the_file_editor_behind_it(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("notes.txt");

        cx.simulate_input("EDITED");
        let content_before = app.read_with(cx, |app, _| {
            app.edit_buffer(&relative).unwrap().content.clone()
        });
        assert_eq!(content_before, "EDITEDhello\n");

        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();
        cx.simulate_input("query");
        assert_eq!(
            app.read_with(cx, |app, _| app.palette_query.as_str().to_string()),
            "query"
        );

        cx.simulate_keystrokes(SECONDARY_Z);

        assert_eq!(
            app.read_with(cx, |app, _| app.palette_query.as_str().to_string()),
            "",
            "the focused widget's own history is what secondary-z must step - a handler that \
             inspected app state instead of relying on GPUI's focused-node dispatch would have \
             undone the file editor here, since its buffer is just as live"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .unwrap()
                .content
                .clone()),
            content_before,
            "the file editor behind the palette must be completely untouched"
        );

        cx.simulate_keystrokes(SECONDARY_SHIFT_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app.palette_query.as_str().to_string()),
            "query",
            "redo must replay the query"
        );
    }

    #[gpui::test]
    fn reopening_the_palette_starts_a_genuinely_fresh_history(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();
        cx.simulate_input("gone");
        app.update_in(cx, |app, window, cx| app.close_palette(window, cx));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app.palette_query.as_str().to_string()),
            "",
            "a fresh palette must have nothing to undo back to"
        );
    }

    #[gpui::test]
    fn secondary_z_in_the_settings_keybindings_filter_undoes_the_filter(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.settings_page = settings::SettingsPage::Keymap;
            window.focus(&app.settings_keymap_filter_focus_handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_input("palette");
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_keymap_filter.as_str().to_string()),
            "palette"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_keymap_filter.as_str().to_string()),
            "",
            "the focused settings text field's own history is what secondary-z must step"
        );

        cx.simulate_keystrokes("ctrl-y");
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_keymap_filter.as_str().to_string()),
            "palette",
            "ctrl-y must redo here too"
        );
    }

    #[gpui::test]
    fn secondary_z_in_the_rail_filter_undoes_it_including_a_real_escape_clear(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_input("main");
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "main"
        );
        cx.simulate_keystrokes("escape");
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            ""
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "main",
            "Esc clearing a filter must be a real, undoable step, not a silent loss"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "",
            "and the typing burst before it is its own further step"
        );
    }

    #[gpui::test]
    fn secondary_z_in_the_new_file_prompt_undoes_the_name_and_a_fresh_prompt_has_no_history(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("notes.md");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .new_file_input
                .as_ref()
                .unwrap()
                .name
                .as_str()
                .to_string()),
            "notes.md"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app
                .new_file_input
                .as_ref()
                .unwrap()
                .name
                .as_str()
                .to_string()),
            "",
            "the focused prompt's own history is what secondary-z must step"
        );

        app.update_in(cx, |app, window, cx| app.cancel_new_file(window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            app.read_with(cx, |app, _| app
                .new_file_input
                .as_ref()
                .unwrap()
                .name
                .as_str()
                .to_string()),
            "",
            "a fresh prompt must have no predecessor history reachable from it"
        );
    }
}

/// GitHub issue #15: "an action focuses its result". Every one of these drives the *real*
/// palette - open it, type a real query, press a real `enter` - and then asserts with a real
/// keystroke, because the whole risk is which widget the keystroke reaches, which no state
/// assertion can see. `cx.simulate_input` goes through GPUI's real platform input path, so
/// "the character landed in the buffer" is only true if a real `EntityInputHandler` was really
/// registered against the really-focused editor.
#[cfg(test)]
mod palette_result_focus_tests {
    use super::*;
    use crate::palette::state as palette;
    use gpui::{Entity, TestAppContext};
    use std::fs;

    /// `src/main.rs` (the file every test opens) plus a sibling, so the palette's own filtering
    /// has something to actually discriminate between.
    fn seed(repo: &crate::test_support::TempRoot) {
        fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        fs::write(repo.path().join("src/other.rs"), "pub fn o() {}\n").expect("write");
    }

    fn open_seeded(
        cx: &mut TestAppContext,
    ) -> (
        crate::test_support::TempRoot,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = crate::test_support::temp_root();
        seed(&repo);
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();
        (repo, app, cx)
    }

    /// Opens the palette and drives it to the file result for `name` the way a user does - typing
    /// the query and pressing `enter` - after asserting that the highlighted row really is that
    /// file, so a test can never pass by running some unrelated entry that happened to be first.
    fn open_file_through_the_palette(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        query: &str,
        expected: &std::path::Path,
    ) {
        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();
        cx.simulate_input(query);
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            let groups = app.build_palette_groups(cx);
            let flat = palette::flatten(&groups);
            let entry = flat
                .get(app.palette_selected)
                .expect("the palette must have a highlighted row");
            assert_eq!(
                entry.target,
                palette::EntryTarget::File(expected.to_path_buf()),
                "premise: typing {query:?} must highlight {expected:?}, not some other entry"
            );
        });

        cx.simulate_keystrokes("enter");
        // The file's content arrives on a background read; `edit_buffers` is seeded from its
        // completion handler, and the caret's row has to paint once for the real input handler to
        // be registered. Same render/park cycle `text_undo_scoping_tests::open_file_for_editing`
        // drives for the same reason.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
    }

    #[gpui::test]
    fn selecting_a_file_in_the_palette_opens_and_reveals_it(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        open_file_through_the_palette(&app, cx, "main.rs", &target);

        let relative = PathBuf::from("src/main.rs");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change.as_deref(),
                Some(relative.as_path()),
                "the palette must have opened the file's tab"
            );
            assert_eq!(app.open_files(), vec![relative.clone()]);
            assert!(
                app.edit_buffer_contains(&relative),
                "and a real edit buffer must be backing it"
            );
        });
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "and the palette must have closed behind it"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.selected_tree_path.clone()),
            Some(target),
            "the same action must also highlight the file in the tree - reveal and open are one \
             action, not two that might disagree"
        );
    }

    #[gpui::test]
    fn the_keystroke_after_a_palette_file_result_lands_in_the_buffer(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        open_file_through_the_palette(&app, cx, "main.rs", &target);
        let relative = PathBuf::from("src/main.rs");

        cx.simulate_input("X");
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .content
                .clone()),
            "Xfn main() {}\n",
            "the very next keystroke after the palette closes must land in the file that was \
             just opened - not in the palette, and not in the terminal that had focus before"
        );
    }

    #[gpui::test]
    fn arrow_keys_after_a_palette_file_result_move_the_caret(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        open_file_through_the_palette(&app, cx, "main.rs", &target);
        let relative = PathBuf::from("src/main.rs");

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .cursor_offset()),
            0,
            "a freshly opened file starts at 1:1"
        );

        cx.simulate_keystrokes("right");
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .cursor_offset()),
            1,
            "arrow keys must move the caret in the opened file"
        );
    }

    #[gpui::test]
    fn reopening_an_already_open_file_focuses_it_without_duplicating_its_tab(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        let relative = PathBuf::from("src/main.rs");

        open_file_through_the_palette(&app, cx, "main.rs", &target);
        cx.simulate_input("A");
        cx.run_until_parked();

        // Park focus off the editor, the way clicking into the file tree would - so this test
        // genuinely exercises the re-open path rather than passing because focus never left.
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.tree_focus_handle, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("!");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.edit_buffer(&relative).expect("buffer").content,
                "Afn main() {}\n",
                "premise: with the tree focused, a keystroke must not reach the buffer"
            );
        });

        open_file_through_the_palette(&app, cx, "main.rs", &target);
        cx.simulate_input("B");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_files(),
                vec![relative.clone()],
                "reopening the same file must reuse its tab, never append a second one"
            );
            assert_eq!(
                app.edit_buffer(&relative).expect("buffer").content,
                "ABfn main() {}\n",
                "both keystrokes must have landed in the same real buffer - and `B` lands \
                 *after* `A` because the buffer kept its own caret (offset 1) across the \
                 reopen. That half is pre-existing `edit_buffers` behaviour, not this change; \
                 the visible half of the same clause is covered by \
                 `code_surface::tabs::reopened_file_caret_tests`"
            );
        });
    }

    #[gpui::test]
    fn escaping_the_palette_restores_focus_to_the_editor_it_was_opened_over(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        open_file_through_the_palette(&app, cx, "main.rs", &target);
        let relative = PathBuf::from("src/main.rs");

        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();
        cx.simulate_input("some query");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!app.read_with(cx, |app, _| app.palette_open));

        cx.simulate_input("Z");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .content
                .clone()),
            "Zfn main() {}\n",
            "an Esc-dismissed palette must hand focus straight back to the editor it opened \
             over - the query it swallowed must not have taken the editor's focus with it"
        );
    }

    #[gpui::test]
    fn a_palette_file_result_run_over_settings_leaves_a_usable_keyboard(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        let relative = PathBuf::from("src/main.rs");

        app.update_in(cx, |app, window, cx| app.open_settings(window, cx));
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "premise: Settings must really be up"
        );

        open_file_through_the_palette(&app, cx, "main.rs", &target);

        app.read_with(cx, |app, _| {
            assert!(
                !app.settings_open,
                "opening a file must not leave Settings covering it - focusing a surface that \
                 isn't rendered is the dangling-focus bug this app has shipped repeatedly"
            );
            assert_eq!(app.open_change.as_deref(), Some(relative.as_path()));
        });

        cx.simulate_input("K");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .content
                .clone()),
            "Kfn main() {}\n",
            "and the next keystroke must land in the file that was asked for"
        );
    }

    #[gpui::test]
    fn opening_the_first_file_from_the_palette_never_captures_the_palettes_own_handle(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        let relative = PathBuf::from("src/main.rs");

        app.read_with(cx, |app, _| {
            assert!(
                app.open_change.is_none(),
                "premise: no tab yet, so this really is the capturing transition"
            );
        });
        open_file_through_the_palette(&app, cx, "main.rs", &target);

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(relative.clone(), window, cx);
        });
        cx.run_until_parked();

        let (focused, palette_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.palette_focus_handle.clone())
        });
        assert_ne!(
            focused.as_ref(),
            Some(&palette_handle),
            "closing the last tab must not restore focus onto the palette's own handle - the \
             palette has not been rendered since it closed"
        );
    }

    #[gpui::test]
    fn cycling_off_files_from_the_palette_does_not_restore_focus_onto_the_unrendered_tree(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded(cx);

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.tree_focus_handle, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();

        let ran = app.update(cx, |app, cx| {
            let groups = app.build_palette_groups(cx);
            let index = palette::flatten(&groups).iter().position(|entry| {
                entry.target
                    == palette::EntryTarget::Command(palette::PaletteCommand::CycleRightPanel)
            });
            match index {
                Some(index) => {
                    app.palette_selected = index;
                    true
                }
                None => false,
            }
        });
        assert!(ran, "the palette must offer the real right-panel cycle");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.right_sidebar_view,
                RightSidebarView::Search,
                "premise: the cycle really ran (Files -> Search) and the tree really is unrendered"
            );
        });
        let (focused, tree_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.tree_focus_handle.clone())
        });
        assert_ne!(
            focused.as_ref(),
            Some(&tree_handle),
            "focus must not be restored onto the file tree's handle once the tree is gone - \
             GPUI would fall back to the dispatch root and silently kill every scoped binding"
        );
    }

    #[gpui::test]
    fn a_palette_entry_that_opens_nothing_still_restores_the_previous_focus(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded(cx);
        let target = repo.path().join("src/main.rs");
        open_file_through_the_palette(&app, cx, "main.rs", &target);
        let relative = PathBuf::from("src/main.rs");

        app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
        cx.run_until_parked();
        let ran = app.update(cx, |app, cx| {
            let groups = app.build_palette_groups(cx);
            let index = palette::flatten(&groups).iter().position(|entry| {
                entry.target
                    == palette::EntryTarget::Command(palette::PaletteCommand::WindowControlsSystem)
            });
            match index {
                Some(index) => {
                    app.palette_selected = index;
                    true
                }
                None => false,
            }
        });
        assert!(
            ran,
            "the palette must offer the real window-controls-style command to run"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "premise: the entry really ran and closed the palette"
        );

        cx.simulate_input("Q");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .content
                .clone()),
            "Qfn main() {}\n",
            "an entry that focuses nothing must restore the pre-palette focus, or every \
             non-opening palette command would silently strand the next keystroke"
        );
    }

    #[gpui::test]
    fn every_palette_command_is_findable_by_its_own_label(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());

        for command in palette::PaletteCommand::ALL {
            if matches!(
                command,
                palette::PaletteCommand::OpenGitGraph
                    | palette::PaletteCommand::RestartLanguageServer
            ) {
                continue;
            }
            app.update_in(cx, |app, window, cx| app.open_palette(window, cx));
            app.update(cx, |app, _| {
                app.palette_query
                    .insert_str(command.label(), std::time::Instant::now());
            });
            cx.run_until_parked();

            let found = app.update(cx, |app, cx| {
                let groups = app.build_palette_groups(cx);
                palette::flatten(&groups)
                    .iter()
                    .any(|entry| entry.target == palette::EntryTarget::Command(command))
            });
            assert!(
                found,
                "{:?} has a real handler but searching its own label {:?} finds nothing in the \
                 palette - the exact shape of the live-reported bug",
                command,
                command.label()
            );

            app.update_in(cx, |app, window, cx| app.close_palette(window, cx));
            cx.run_until_parked();
        }
    }
}

/// GitHub issue #255 ("sometimes the command palette can't be opened like when there is no tab
/// open"): real, keystroke-driven coverage for a window that has genuinely run out of tabs.
#[cfg(test)]
mod tabless_window_keybinding_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    fn bind_real_keys(cx: &mut gpui::VisualTestContext) {
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
    }

    /// `crate::default_key_bindings`' own `"secondary-p"`, resolved the same way that list does.
    const SECONDARY_P: &str = if cfg!(target_os = "macos") {
        "cmd-p"
    } else {
        "ctrl-p"
    };

    /// A test window over a throwaway repo directory holding one real file, with the initial
    /// shell agent already spawned (the same `palette_focus_tests::open_test_app` setup every
    /// other keybinding test in this file uses). The `TempDir` is returned so it outlives the
    /// test body.
    fn open_app_with_a_file(
        cx: &mut TestAppContext,
    ) -> (
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
        crate::test_support::TempRoot,
        PathBuf,
    ) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("a.txt");
        std::fs::write(&file_path, "hello\n").expect("write a.txt");
        let (app, cx) = crate::test_support::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        (app, cx, repo, file_path)
    }

    /// Closes every open agent through [`AdeApp::close_agent`] - the real entry point the tab
    /// strip's `×`, middle-click and the Agent menu's Archive row all go through.
    fn close_every_agent(app: &Entity<AdeApp>, cx: &mut gpui::VisualTestContext) {
        let ids: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|agent| agent.id).collect()
        });
        app.update_in(cx, |app, window, cx| {
            for id in ids {
                app.close_agent(id, window, cx);
            }
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.agents.is_empty()),
            "setup: every agent must really be closed"
        );
    }

    fn assert_palette_opened(app: &Entity<AdeApp>, cx: &mut gpui::VisualTestContext, state: &str) {
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {SECONDARY_P} keystroke must open the palette {state} - GitHub issue #255: \
             with no tab left to hand focus back to, restore_focus used to move Window::focus \
             nowhere at all, leaving it on the surface that had just stopped being rendered and \
             every global keybinding silently dead until the next click"
        );
    }

    #[gpui::test]
    fn ctrl_p_opens_the_palette_with_no_tab_open_at_all(cx: &mut TestAppContext) {
        let (app, cx, _repo, _file) = open_app_with_a_file(cx);
        bind_real_keys(cx);

        close_every_agent(&app, cx);

        cx.simulate_keystrokes(SECONDARY_P);
        assert_palette_opened(&app, cx, "with no tab open at all");
    }

    #[gpui::test]
    fn ctrl_p_opens_the_palette_after_the_last_agent_then_the_last_file_tab_are_closed(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, _repo, file_path) = open_app_with_a_file(cx);
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_some()),
            "setup: a real file tab must be open"
        );

        close_every_agent(&app, cx);
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_some()),
            "setup: the file tab must survive closing the agents - the point of this ordering"
        );

        app.update_in(cx, |app, window, cx| {
            app.close_change_diff(window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_none()),
            "setup: the file tab must really be closed - zero tabs now"
        );
        // The mechanism, not just the symptom: focus has to land on something this frame really
        // renders. The rail's root container is that thing whenever the workspace body is showing
        // (`AdeApp::focus_fallback_handle`).
        assert!(
            app.update_in(cx, |app, window, _cx| app
                .rail_focus_handle
                .is_focused(window)),
            "closing the last tab must leave real keyboard focus on the rail's own root \
             container, not dangling on the code surface handle that just stopped rendering"
        );

        cx.simulate_keystrokes(SECONDARY_P);
        assert_palette_opened(
            &app,
            cx,
            "after the last agent and then the last file tab closed",
        );
    }

    #[gpui::test]
    fn ctrl_p_opens_the_palette_after_closing_settings_that_outlived_the_last_agent(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, _repo, _file) = open_app_with_a_file(cx);
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| app.open_settings(window, cx));
        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "setup: Settings must really be open"
        );

        let ids: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|agent| agent.id).collect()
        });
        app.update_in(cx, |app, window, cx| {
            for id in ids {
                app.archive_agent(id, window, cx);
            }
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.agents.is_empty()),
            "setup: every agent must really be archived"
        );

        app.update_in(cx, |app, window, cx| app.close_settings(window, cx));
        assert!(
            app.read_with(cx, |app, _| !app.settings_open
                && app.open_change.is_none()
                && app.agents.is_empty()),
            "setup: Settings closed onto a genuinely tabless workspace"
        );

        cx.simulate_keystrokes(SECONDARY_P);
        assert_palette_opened(
            &app,
            cx,
            "after closing Settings that outlived the last agent",
        );
    }

    #[gpui::test]
    fn ctrl_p_opens_the_palette_after_closing_a_graph_tab_that_outlived_the_last_agent(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, _repo, _file) = open_app_with_a_file(cx);
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| app.open_git_graph(window, cx));
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.graph_tab_active),
            "setup: the graph tab must really be showing"
        );

        close_every_agent(&app, cx);

        app.update_in(cx, |app, window, cx| app.close_git_graph_tab(window, cx));
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| !app.graph_tab_active
                && !app.graph_tab_open
                && app.open_change.is_none()),
            "setup: the graph tab must really be closed - zero tabs now"
        );

        cx.simulate_keystrokes(SECONDARY_P);
        assert_palette_opened(
            &app,
            cx,
            "after closing a graph tab that outlived the last agent",
        );
    }
}
