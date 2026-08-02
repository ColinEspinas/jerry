//! Opens/closes Surface C (Diff/File), the command palette, and Settings. All three are
//! overlay-shaped: each has its own `*_focus_handle` and `*_focus: OverlayFocus` pair, moves
//! real focus onto its handle on open, and must restore focus through [`OverlayFocus`]/
//! [`restore_focus`] on close - see those types' docs in `root::mod` for the dangling-focus
//! invariant this file exists to satisfy. This project has hit "close forgot to restore" bugs
//! repeatedly (BUILD-LOG.md); the fix each time was routing through that shared mechanism
//! rather than hand-rolling capture/restore again, so any new overlay added here should do the
//! same instead of re-deriving it.

use super::*;

impl AdeApp {
    /// Captures the pre-open focus target only on the closed-to-open transition
    /// (`Self::open_change` was `None`) - a second file opened while one is already showing must
    /// not overwrite the real original target with `Self::code_focus_handle` itself (already
    /// focused by then). Always moves focus onto [`Self::code_focus_handle`] regardless.
    ///
    /// A handle belonging to one of this app's *other* overlays is never captured, even on that
    /// transition. An overlay's handle is by definition about to stop being rendered - the
    /// overlay is closing, which is why something is being opened underneath it - so storing one
    /// as this surface's return target just relocates the dangling-focus bug to whenever the last
    /// file tab is closed ([`Self::close_file_tab`] restores through the same
    /// [`OverlayFocus`]). Reproduced by this branch's own adversarial audit: opening a file from
    /// the palette with no tab yet captured `palette_focus_handle`, and closing that tab then
    /// focused it. Each overlay already holds the real pre-overlay target in its own
    /// `OverlayFocus`; declining to capture here leaves `restore_focus`'s active-agent-pane
    /// fallback as the honest answer instead of a handle that is certain to be wrong.
    pub(crate) fn focus_code_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_change.is_none() && !self.focus_is_on_an_overlay(window, cx) {
            self.code_focus.capture(window, &self.agents, cx);
        }
        window.focus(&self.code_focus_handle, cx);
    }

    /// Whether keyboard focus is currently on one of this app's overlay handles - the palette,
    /// Settings, or the "New file" prompt. See [`Self::focus_code_surface`] for why capturing one
    /// as a return target is always wrong.
    ///
    /// `pub(crate)`, not private: `crate::graph_view::render::AdeApp::open_git_graph` reuses this
    /// exact check for its own pre-open focus capture (the git graph tab is real tab-strip
    /// content, the same shape as [`Self::code_focus_handle`] - not a fourth entry in this list -
    /// but it still must never capture the palette/Settings/new-file handles as its own return
    /// target, for the same reason [`Self::focus_code_surface`] mustn't).
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
        // The tab strip's `+` menu, the title bar's File/Edit/View/Agent/Help dropdown, and
        // the "New file" prompt are all unconditional siblings of the palette - see
        // `Self::plus_menu_open`'s/`Self::title_menu_open`'s/`Self::new_file_input`'s own docs.
        self.plus_menu_open = false;
        self.title_menu_open = None;
        self.new_file_input = None;
        self.palette_focus.capture(window, &self.agents, cx);
        self.palette_scope = palette::PaletteScope::default();
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
        restore_focus(&self.agents, &mut self.palette_focus, window, cx);
        cx.notify();
    }

    /// Closes the palette *without* touching focus - for the one case
    /// [`crate::palette::render::AdeApp::run_selected_palette_entry`] detects: the entry that
    /// just ran moved keyboard focus onto its own result, and that is where focus belongs
    /// (GitHub issue #15's "an action focuses its result"). See that method's docs for how the
    /// two closing paths are chosen between.
    ///
    /// Deliberately *not* `restore_focus` with a pre-cleared [`OverlayFocus`]: that function
    /// falls back to the active agent's terminal pane when it has no captured target, so
    /// clearing first would have moved focus off the file that was just opened and into the
    /// terminal - the same class of wrong-place focus this whole mechanism exists to stop. The
    /// captured target is discarded via [`OverlayFocus::clear`] instead, which is exactly what
    /// that method is for and what `Self::close_palette`'s own Settings branch already does.
    ///
    /// This does not violate [`OverlayFocus`]' dangling-focus invariant, but only because the
    /// entries that move focus each move it onto something they have also made *rendered*. That
    /// is a property of those entries, not something this function can check, and this branch's
    /// own adversarial audit found the one case where it did not hold: a file result run while
    /// Settings was open focused `code_focus_handle` while `render` was still drawing Settings
    /// instead of the workspace. The fix is at the source -
    /// [`crate::code_surface::tabs::AdeApp::open_and_focus_file`] now closes Settings before
    /// focusing the code surface, so what it focuses really is rendered - rather than a special
    /// case here, which would only have restored focus while leaving the user staring at
    /// Settings after asking for a file.
    pub(crate) fn close_palette_keeping_result_focus(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
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
        self.plus_menu_open = false;
        self.title_menu_open = None;
        self.new_file_input = None;
        // Same reason as the three above: Settings *replaces* the workspace body, so a file-tree
        // context menu or a half-typed inline name left open would either float over the
        // Settings page or - worse for the editor - keep the tree's `"tree-editing"` key context
        // alive on a node that is no longer rendered (GitHub issue #19). The armed *delete
        // confirmation* is deliberately left alone: it is a real, window-level modal the user is
        // mid-way through answering, and `crate::root::AdeApp::render` keeps it hidden while
        // Settings is up rather than silently disarming it.
        self.tree_context_menu = None;
        self.tree_inline_edit = None;
        // Same reason, for the git graph tab's own two window-positioned overlays (GitHub issue
        // #1's row `⋯`/right-click menu and Push `▾` menu): unlike opening a *different* tab,
        // opening Settings does not clear `graph_tab_active` (the graph tab, if it was showing,
        // is still "active" underneath Settings - `crate::graph_view::render::AdeApp::
        // leave_graph_tab` is not called here), so without this an open row or Push menu kept
        // painting its full-window scrim over the Settings surface, swallowing the first click a
        // user aimed at it (an adversarial audit's own finding).
        self.graph_state.row_menu_open = None;
        self.graph_state.push_menu_open = false;
        self.settings_open = true;
        self.settings_focus.capture(window, &self.agents, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.custom_theme_remove_armed = None;
        window.focus(&self.settings_focus_handle, cx);
        self.load_agent_rows(cx);
        self.load_lsp_rows(cx);
        cx.notify();
    }

    /// Closes the Settings surface (the nav header's Esc keycap, or a real Esc keystroke via
    /// [`Self::handle_settings_key_down`]) and restores focus via [`restore_focus`].
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        // A live keybinding-recording intercept (`Self::_keymap_intercept`) is a real, global
        // `App::intercept_keystrokes` subscription - it must never survive leaving the Settings
        // surface, or every keystroke in the whole app would keep being silently swallowed.
        self.cancel_keybinding_recording(cx);
        restore_focus(&self.agents, &mut self.settings_focus, window, cx);
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
    use gpui::{Entity, TestAppContext};

    /// Opens an `AdeApp` in a test GPUI window against a throwaway temp directory (not a real
    /// git repo, so `worktrees`/`diff_state` end up empty/errored - irrelevant to what these
    /// tests check). `pub(crate)` since `settings_focus_tests` and others reuse this
    /// same setup. Uses `AdeApp::new_with_settings` (in-memory `Settings::default()`, `None`
    /// path), not `AdeApp::new`, so tests never read or write a real `settings.toml`.
    pub(crate) fn open_test_app(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
    ) -> (Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings_store::Settings::default(),
                None,
                window,
                cx,
            )
        })
    }

    /// Without a focus restore in `close_palette`, `Window::focus` stayed on the untracked
    /// `palette_focus_handle` and every action dispatch after that - including the next ⌘P -
    /// fell back to the dispatch root, so the palette could never be reopened without a manual
    /// click first.
    #[gpui::test]
    fn toggle_palette_reopens_after_being_closed(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
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

    /// A fresh window starts with `Window::focus == None` - without `AdeApp::new` giving the
    /// initial agent's pane focus up front, the very first ⌘P would silently do nothing.
    #[gpui::test]
    fn toggle_palette_works_on_a_fresh_window_with_nothing_clicked_yet(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "secondary-p on a completely fresh window (nothing clicked yet) should still open the \
             palette"
        );
    }

    /// Spawning an agent from the palette swaps the active agent; verifies `close_palette`
    /// focuses the *new* agent's pane instead of the stale captured handle (see
    /// [`OverlayFocus`]'s `opened_agent` field).
    #[gpui::test]
    fn toggle_palette_still_works_after_a_palette_spawned_new_shell(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
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

    /// Scope-prefix coverage requested alongside the focus fix: `>`/`@` should only switch the
    /// palette's scope when typed as the very first character of an empty query - typed
    /// mid-query, it's an ordinary character appended to the query like any other.
    #[gpui::test]
    fn scope_prefix_only_fires_on_an_empty_query(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
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
        let repo = tempfile::tempdir().expect("tempdir");
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

    /// The `+` menu popover is rendered as an unconditional sibling of both the palette and
    /// Settings (`AdeApp::plus_menu_open`'s own docs) - opening either while it happened to
    /// still be open must close it, or it would paint on top of a surface it no longer makes
    /// sense over.
    #[gpui::test]
    fn opening_the_palette_closes_an_already_open_plus_menu(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.dispatch_action(TogglePalette);

        assert!(app.read_with(cx, |app, _| app.palette_open));
        assert!(
            !app.read_with(cx, |app, _| app.plus_menu_open),
            "opening the palette should have closed the still-open plus menu"
        );
    }

    #[gpui::test]
    fn opening_settings_closes_an_already_open_plus_menu(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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
    }

    /// `ctrl-shift-T` is a literal Ctrl combo on every OS, not `secondary`-aliased (see
    /// `crate::default_key_bindings`), so it's simulated literally rather than branching on
    /// `cfg!(target_os = "macos")`.
    #[gpui::test]
    fn ctrl_shift_t_spawns_a_real_shell_agent_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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
            Some(AgentKind::Shell),
            "New terminal always spawns a real Shell agent"
        );
    }

    #[gpui::test]
    fn secondary_shift_n_spawns_a_real_agent_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// `secondary-p` is now `TogglePalette`'s own real global keybinding (see
    /// `crate::default_key_bindings`'s docs for the full tradeoff), deliberately unscoped rather
    /// than `!terminal`-scoped - GPUI dispatches a matched `KeyBinding` before a focused
    /// element's own `on_key_down`, so this global binding really does swallow readline's
    /// `previous-history` Ctrl+P out of every focused terminal, a known, explicit, accepted
    /// tradeoff rather than an oversight. This proves the palette *does* now open even with a
    /// terminal focused; `crate::terminal::pane`'s `keystroke_tests` covers the (now-unreachable
    /// in practice) pty-forwarding half of `Ctrl+P`.
    #[gpui::test]
    fn ctrl_p_opens_the_palette_even_while_a_terminal_is_focused(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// With a file tab active (so `render_center_pane` shows the file, not any agent's pane),
    /// spawning a new agent via `ctrl-shift-t` must not leave `Window::focus` on a pane that
    /// isn't rendered anywhere that frame - `Agents::spawn` used to move focus there
    /// unconditionally, silently killing every bound shortcut until the next click.
    #[gpui::test]
    fn ctrl_p_still_works_after_ctrl_shift_t_with_a_file_tab_active(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("a.txt"), "hello\n").expect("write a.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The identical gap in `Agents::close`: closing the active agent's tab picks a new
    /// active agent but must also move focus onto it, or every bound shortcut dies until the
    /// next click.
    #[gpui::test]
    fn ctrl_p_still_works_after_closing_the_active_agent_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let first_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        });
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
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

    /// The other path to the same `Agents::close` gap: archiving the active agent from the
    /// rail (`AdeApp::archive_agent`, which delegates to `Self::close_agent`).
    #[gpui::test]
    fn ctrl_p_still_works_after_archiving_the_active_agent(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let first_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        });
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
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

    #[gpui::test]
    fn bracket_advances_to_the_next_changed_file_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        // `]` is scoped to `Some("diff")` (`crate::default_key_bindings`), not global - a global
        // bare `]` would swallow a literal `]` typed into any focused terminal instead of
        // forwarding it to the pty. This opens a file first (`AdeApp::open_change_diff`) to
        // establish "diff"-context focus before simulating the keystroke.
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\n").expect("write b.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\nchanged\n").expect("rewrite b.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The other half of the scoping above: with no file tab focused, `]` must not reach
    /// [`NextChangedFile`] at all (`crate::terminal::pane`'s `keystroke_tests` covers the pty-forwarding
    /// half). Asserts a diff with at least one file actually loaded first, so this can't go
    /// vacuous if the fixture ever stops producing a diff - `open_change` would stay `None`
    /// before and after `]` for an uninteresting reason instead of proving the scoping works.
    #[gpui::test]
    fn bracket_does_not_fire_globally_while_a_terminal_is_focused(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// Spawns four extra real shell agents (five total, including the one `AdeApp::new` starts)
    /// and confirms `secondary-3` really jumps to the third one in real agent order - not just
    /// that `AdeApp::jump_to_agent_at(3, ..)` does when called directly.
    #[gpui::test]
    fn secondary_3_jumps_to_the_third_real_agent_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        for _ in 0..4 {
            app.update_in(cx, |app, window, cx| {
                app.new_agent(AgentKind::Shell, window, cx);
            });
        }
        let third_id = app.read_with(cx, |app, _| {
            app.agents
                .iter()
                .nth(2)
                .map(|agent| agent.id)
                .expect("five real agents should exist by now")
        });
        assert_ne!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(third_id),
            "sanity check: the most recently spawned agent (the fifth), not the third, should \
             be active before the jump"
        );

        let secondary_3 = if cfg!(target_os = "macos") {
            "cmd-3"
        } else {
            "ctrl-3"
        };
        cx.simulate_keystrokes(secondary_3);

        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(third_id),
            "a real, simulated {secondary_3} keystroke must activate the agent at position 3 \
             through crate::default_key_bindings' real secondary-3 -> JumpToAgent3 binding"
        );
    }

    /// [`AdeApp::jump_to_agent_at`]'s own direct-call coverage (as opposed to the keystroke
    /// simulation above) for every position 1..=8, plus the real "fewer agents than the
    /// position" no-op.
    #[gpui::test]
    fn jump_to_agent_at_activates_the_right_agent_by_position(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let mut ids = vec![app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the initial shell agent")
        })];
        for _ in 0..3 {
            let id = app.update_in(cx, |app, window, cx| {
                app.agents.spawn(
                    AgentKind::Shell,
                    repo.path().to_path_buf(),
                    app.settings.appearance.terminal_font_size,
                    window,
                    cx,
                )
            });
            ids.push(id);
        }
        // Four real agents now exist, in spawn order `ids[0..4]`.

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

        // A real no-op: there is no fifth agent.
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

    /// `secondary-,` opens Settings, a real Esc keystroke (`VisualTestContext::
    /// simulate_keystrokes("escape")`, the same lowercase-string precedent
    /// `vendor/zed/crates/editor/src/edit_prediction_tests.rs` uses) closes it, and a
    /// subsequent `secondary-p` still reaches [`AdeApp::handle_toggle_palette_action`].
    #[gpui::test]
    fn toggle_settings_action_opens_then_real_escape_closes_it_and_focus_stays_live(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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

    /// `secondary-,` works from a fresh window with nothing clicked yet - same case as
    /// `palette_focus_tests::toggle_palette_works_on_a_fresh_window_with_nothing_clicked_yet`,
    /// here for Settings.
    #[gpui::test]
    fn toggle_settings_works_on_a_fresh_window_with_nothing_clicked_yet(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);

        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "secondary-, on a completely fresh window (nothing clicked yet) should still open Settings"
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
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// An agent tab opened before Settings was shown is still there, and still active, after an
    /// open/close round-trip - Settings swaps which body `Render::render` draws
    /// ([`AdeApp::render_workspace_body`]) without touching `AdeApp::agents` itself.
    #[gpui::test]
    fn closing_settings_leaves_open_agent_tabs_intact(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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

    /// A page switch survives a Settings close/reopen, matching `AdeApp::open_settings`'s "does
    /// not reset settings_page" contract.
    #[gpui::test]
    fn settings_page_selection_persists_across_a_close_and_reopen(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
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

    /// The palette's `Open Settings` command opens Settings and leaves focus on it, not a stale
    /// palette handle - covers `AdeApp::close_palette`'s Settings-aware branch.
    #[gpui::test]
    fn open_settings_palette_command_leaves_real_focus_on_settings(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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

    /// Opening Settings must populate [`AdeApp::agent_rows`] from the background `$PATH` search
    /// (see [`AdeApp::load_agent_rows`]) rather than leave it empty. `run_until_parked` drives
    /// the spawned task to completion; without it this would race the still-in-flight search.
    #[gpui::test]
    fn opening_settings_populates_real_agent_rows_from_a_background_path_search(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert!(
            app.read_with(cx, |app, _| app.agent_rows.is_empty()),
            "agent_rows should still be empty before Settings has ever been opened - nothing \
             should eagerly run a $PATH search that's only ever shown on the Agents page"
        );

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
    }

    /// Mirrors the Agents-page test above, for [`AdeApp::lsp_rows`] (the Language servers page).
    #[gpui::test]
    fn opening_settings_populates_real_lsp_rows_from_a_background_path_search(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert!(app.read_with(cx, |app, _| app.lsp_rows.is_empty()));

        cx.dispatch_action(ToggleSettings);
        cx.run_until_parked();

        let rows = app.read_with(cx, |app, _| app.lsp_rows.clone());
        let languages = settings::lsp_languages();
        assert_eq!(rows.len(), languages.len());
        for def in languages {
            assert!(rows.iter().any(|row| row.language == def.language));
        }
    }

    #[gpui::test]
    fn settings_opens_to_general_by_default_on_a_fresh_window(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);

        assert_eq!(
            app.read_with(cx, |app, _| app.settings_page),
            SettingsPage::General
        );
    }

    /// Every page in `SettingsPage::ALL` must render without panicking. Running the test
    /// executor to a parked state (`cx.run_until_parked`) is what actually exercises
    /// `AdeApp::render_settings_content`'s per-page render, not just the state transition
    /// `select_settings_page` performs.
    #[gpui::test]
    fn every_settings_page_renders_without_panicking(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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

    /// The General page's `Window controls` row is wired live, not decorative:
    /// `AdeApp::set_window_controls_style` is the method its click handler
    /// (`crate::settings::render::render_settings_general_page`) calls - the same accessor
    /// `title_bar::render::caption_button_tests` proves actually changes which title-bar variant
    /// renders.
    #[gpui::test]
    fn window_controls_style_change_updates_the_real_persisted_settings_field(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        (app, cx, file_path)
    }

    #[gpui::test]
    fn toggle_settings_action_reaches_the_real_handler_with_a_file_view_open(
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
        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "secondary-, must still reach handle_toggle_settings_action once a plain .txt File view is \
             mounted - without AdeApp::code_focus_handle tracking real focus there, this dispatch \
             would silently fall back to the window's internal dispatch root (GPUI's own \
             `Window::focus_node_id_in_rendered_frame` falls back to `dispatch_tree.root_node_id\
             ()` whenever the focused FocusId isn't found in the last rendered frame) instead of \
             reaching this handler"
        );
    }

    #[gpui::test]
    fn toggle_palette_action_reaches_the_real_handler_with_a_file_view_open(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, file_path) = open_test_app_with_a_plain_text_file(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "secondary-p must still reach handle_toggle_palette_action once a File view is mounted, \
             for exactly the same real reason secondary-, must"
        );
    }

    #[gpui::test]
    fn goto_definition_action_reaches_the_real_handler_with_a_file_view_open(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, file_path) = open_test_app_with_a_plain_text_file(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
        cx.dispatch_action(GotoDefinition);
        cx.run_until_parked();
        // `handle_goto_definition_action` only has a harmless early return with `hover == None`
        // (a .txt file has no hover entry) - what's under test is that dispatch reached the
        // handler at all with a File view open, not what it did once there.
        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
    }

    /// Closing the File view ([`AdeApp::close_change_diff`]) must restore focus onto the active
    /// agent's pane, not leave it dangling on [`AdeApp::code_focus_handle`].
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

    /// The hardest real case for §3's terminal rule: a real edit buffer with a real undo history
    /// genuinely exists, and focus is genuinely back on a terminal. `secondary-z` must not reach
    /// text undo - no `"text-input"` anywhere in a terminal's dispatch path - leaving the
    /// keystroke free to reach the pty as the real `SIGTSTP` control byte, which
    /// `crate::terminal::pane::keystroke_tests::ctrl_z_maps_to_the_real_sigtstp_control_byte`
    /// covers the other half of.
    #[gpui::test]
    fn secondary_z_with_a_terminal_focused_does_not_reach_text_undo(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The palette-over-an-open-editor case `crate::default_key_bindings`' own docs single out:
    /// `secondary-z` must undo the *palette query*, because the palette is what has focus - not
    /// the file editor still open behind it.
    #[gpui::test]
    fn secondary_z_with_the_palette_open_undoes_the_query_not_the_file_editor_behind_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// A reopened palette is a genuinely new widget instance: its predecessor's history must not
    /// be reachable, or Ctrl+Z would resurrect a query the user already dismissed.
    #[gpui::test]
    fn reopening_the_palette_starts_a_genuinely_fresh_history(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// Settings › Keybindings' own filter field. Settings being *open* is not enough to claim
    /// `"text-input"` - only the filter row itself is tagged.
    #[gpui::test]
    fn secondary_z_in_the_settings_keybindings_filter_undoes_the_filter(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The rail's agent filter - one of this app's several hand-rolled single-line inputs (this
    /// comment used to claim it was "the fourth and last", which had already gone stale by the
    /// time GitHub issue #45's audit found two more real ones - the git graph tab's own branches
    /// filter and the "New file" prompt - that undo/redo already covered independently but whose
    /// *carets* had never been wired up at all; see `crate::root::caret_blink`'s own docs).
    /// Also covers that `Esc`-clearing a filter is a real, undoable step rather than a silent
    /// loss.
    #[gpui::test]
    fn secondary_z_in_the_rail_filter_undoes_it_including_a_real_escape_clear(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The fourth single-line input: the inline "New file" name prompt. Its history is created and
    /// destroyed with the prompt, which is the per-widget lifetime GitHub issue #17 asks for.
    #[gpui::test]
    fn secondary_z_in_the_new_file_prompt_undoes_the_name_and_a_fresh_prompt_has_no_history(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // Cancel and reopen: a genuinely new prompt, so nothing to undo back to.
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
    use tempfile::TempDir;

    /// `src/main.rs` (the file every test opens) plus a sibling, so the palette's own filtering
    /// has something to actually discriminate between.
    fn seed(repo: &TempDir) {
        fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        fs::write(repo.path().join("src/other.rs"), "pub fn o() {}\n").expect("write");
    }

    fn open_seeded(
        cx: &mut TestAppContext,
    ) -> (TempDir, Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The *open and reveal* half of the issue: a palette file result really opens the file's
    /// tab, with a real edit buffer behind it, and really highlights it in the tree. The keystroke
    /// half is the two tests below - deliberately separate, so a failure names one thing.
    ///
    /// Before the fix the diff-less branch of `open_palette_file_result` opened no tab at all: it
    /// expanded the file's ancestors, highlighted its row, and stopped. That is both this issue's
    /// report and the separately-reported "reveal in tree selects the file but does not open it".
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

    /// The issue's own acceptance criterion, verbatim: "Palette -> type a name -> Enter -> type:
    /// the characters land in the file. No mouse involved at any point."
    ///
    /// This is the assertion `close_palette`'s unconditional `restore_focus` used to fail: the
    /// file opened, and the keystroke went to whatever had focus before the palette.
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

    /// The arrow-key half of the same criterion: `EditorRight` is a real, `\"file-editor\"`-scoped
    /// action, so it only fires if the code surface really is the focused dispatch node.
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

    /// "Works identically when the file is already open: switch to its tab and focus it, never
    /// open a duplicate." Focus is deliberately parked somewhere else first, so this genuinely
    /// tests the re-open path rather than passing because focus never moved.
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

    /// "Dismissing the palette with Esc (no selection) restores focus to exactly where it was
    /// before the palette opened." Asserted through a real keystroke against a real editor rather
    /// than by reading `window.focused`, so it covers the dispatch path and not just the handle.
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

    /// Adversarial-audit regression, CRITICAL. Running a palette file result while the Settings
    /// surface is open used to focus `code_focus_handle` while `AdeApp::render` was still drawing
    /// Settings *instead of* the workspace body - so `Window::focus` pointed at a `FocusId` that
    /// `focus_node_id_in_rendered_frame` could not find, GPUI fell back to the dispatch root with
    /// an empty context stack, and every context-scoped binding died. Esc included: the user was
    /// left on a Settings page they could not leave, with no file in sight, until they clicked.
    ///
    /// The fix is that opening a file closes Settings, so what gets focused really is rendered.
    /// Asserted end to end: the file is showing, a keystroke reaches its buffer, and Esc still
    /// does something real.
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

    /// Adversarial-audit regression, MAJOR. `focus_code_surface` captures the pre-open focus
    /// target on the first file opened - and with no tab open yet, the thing holding focus at
    /// that moment is the palette's own handle. Capturing it moved the dangling-focus bug to
    /// `close_file_tab`, which restores through the very same `OverlayFocus`.
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

    /// Adversarial-audit regression, MAJOR. With the tree focused, opening the palette captures
    /// `tree_focus_handle`; running the palette's own "Toggle Files / Changes" then unrenders the
    /// whole tree. `set_right_sidebar_view`'s own `is_focused` guard cannot see this, because the
    /// *palette* holds focus at that moment - so closing the palette restored focus straight onto
    /// a handle that is no longer in the frame.
    #[gpui::test]
    fn toggling_to_changes_from_the_palette_does_not_restore_focus_onto_the_unrendered_tree(
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
                    == palette::EntryTarget::Command(palette::PaletteCommand::ToggleFilesChanges)
            });
            match index {
                Some(index) => {
                    app.palette_selected = index;
                    true
                }
                None => false,
            }
        });
        assert!(ran, "the palette must offer the real Files/Changes toggle");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.right_sidebar_view,
                RightSidebarView::Changes,
                "premise: the toggle really ran and the tree really is unrendered"
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

    /// The other direction of the same rule, and the reason it is *observed* rather than
    /// declared: an entry that opens nothing must leave focus exactly where it was.
    /// `WindowControlsSystem` is the smallest real such entry - it flips one setting and
    /// touches no focus handle at all.
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
}
