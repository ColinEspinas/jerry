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
    pub(crate) fn focus_code_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_change.is_none() {
            self.code_focus.capture(window, &self.agents, cx);
        }
        window.focus(&self.code_focus_handle, cx);
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

    /// Closes the palette overlay (scrim click, Esc, or running a result) and restores focus via
    /// [`restore_focus`].
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
        self.settings_open = true;
        self.settings_focus.capture(window, &self.agents, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
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
                repo_path,
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

    /// Revision R10's own version of the same real conflict class as the two tests above, for
    /// `Undo`: `secondary-z` resolves to plain `Ctrl+Z` on Linux/Windows, which a focused
    /// terminal needs unclaimed to receive the real `SIGTSTP` suspend control byte
    /// (`crate::terminal::pane::keystroke_tests::ctrl_z_maps_to_the_real_sigtstp_control_byte`
    /// covers
    /// that half). `AdeApp::new_with_settings` always starts a window with one real shell
    /// agent already focused (see that function's own docs) - no extra spawn/focus needed
    /// here, unlike the merge/palette tests elsewhere in this file.
    #[gpui::test]
    fn secondary_z_does_not_undo_while_the_default_terminal_agent_is_focused(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        bind_real_keys(cx);

        assert_eq!(
            app.read_with(cx, |app, _| app.agents.iter().count()),
            1,
            "sanity check: a fresh window always starts with one real, focused shell agent"
        );
        assert!(app.read_with(cx, |app, _| app.worktree_history_status.is_none()));

        let secondary_z = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        cx.simulate_keystrokes(secondary_z);

        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "a real, simulated secondary-z keystroke must NOT reach Undo while the default, \
             real terminal agent has focus - crate::default_key_bindings scopes Undo/Redo to \
             Some(\"!terminal\") specifically so this stays free to reach the pty as literal \
             input instead"
        );
    }

    /// The positive contrast to the test above: once real focus has genuinely moved off the
    /// terminal (Settings open, its own real `track_focus`'d surface), `secondary-z` must reach
    /// the real `Undo` action - proving `Some("!terminal")` isn't just silently swallowing the
    /// keystroke everywhere.
    #[gpui::test]
    fn secondary_z_reaches_undo_once_real_focus_moves_off_the_terminal(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| app.open_settings(window, cx));
        // A real repaint is required before GPUI's key-dispatch tree reflects the new focus
        // target - `simulate_keystrokes` dispatches against the *last painted* frame's tree,
        // which (without this) would still be the terminal-showing frame from window creation,
        // still tagged `"terminal"`.
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "sanity check: Settings should now be open and focused"
        );

        let secondary_z = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        cx.simulate_keystrokes(secondary_z);

        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_history_status.clone()),
            Some("nothing to undo".to_string()),
            "once real focus has moved off the terminal, a real, simulated secondary-z \
             keystroke must reach the real Undo action (Self::perform_undo's own honest \
             \"nothing to undo\" status, since the stack is genuinely empty)"
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

    /// A agent tab opened before Settings was shown is still there, and still active, after an
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
///
/// `crate::worktree_history::flow::AdeApp::perform_undo`'s own honest "nothing to undo" status is
/// used as the real, observable tripwire for "the worktree-level Undo ran": it is set
/// synchronously, on an empty stack, whenever that handler is genuinely reached.
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
    /// genuinely exists, and focus is genuinely back on a terminal. `secondary-z` must reach
    /// *neither* undo system - not the worktree-level one (`!terminal`), and not text undo (no
    /// `"text-input"` anywhere in a terminal's dispatch path) - leaving the keystroke free to
    /// reach the pty as the real `SIGTSTP` control byte, which
    /// `crate::terminal::pane::keystroke_tests::ctrl_z_maps_to_the_real_sigtstp_control_byte`
    /// covers the other half of.
    #[gpui::test]
    fn secondary_z_with_a_terminal_focused_reaches_neither_undo_system(cx: &mut TestAppContext) {
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
        assert!(app.read_with(cx, |app, _| app.worktree_history_status.is_none()));

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
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "and it must not reach the worktree-level Undo either - crate::default_key_bindings \
             scopes it Some(\"!terminal && !text-input\") precisely so the keystroke stays free \
             to reach the pty as literal input"
        );
    }

    /// The palette-over-an-open-editor case `crate::default_key_bindings`' own docs single out:
    /// `secondary-z` must undo the *palette query*, because the palette is what has focus - not
    /// the file editor still open behind it, and not the worktree-level history.
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
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "and the worktree-level Undo must not have run"
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

    /// Settings › Keybindings' own filter field. Also the real contrast case for the pre-existing
    /// `secondary_z_reaches_undo_once_real_focus_moves_off_the_terminal` test above: Settings
    /// being *open* is not enough to claim `"text-input"` - only the filter row itself is tagged,
    /// so the worktree-level Undo still owns the keystroke everywhere else on that surface.
    #[gpui::test]
    fn secondary_z_in_the_settings_keybindings_filter_undoes_the_filter_not_the_worktree_history(
        cx: &mut TestAppContext,
    ) {
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
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "and the worktree-level Undo must not have run - it would have set its honest \
             \"nothing to undo\" status"
        );

        cx.simulate_keystrokes("ctrl-y");
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_keymap_filter.as_str().to_string()),
            "palette",
            "ctrl-y must redo here too"
        );
    }

    /// The rail's agent filter - the fourth and last of this app's hand-rolled single-line
    /// inputs. Also covers that `Esc`-clearing a filter is a real, undoable step rather than a
    /// silent loss.
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
        assert!(app.read_with(cx, |app, _| app.worktree_history_status.is_none()));

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
        assert!(app.read_with(cx, |app, _| app.worktree_history_status.is_none()));

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

    /// The exact sequence self-review turned up: with nothing left to focus, the app falls
    /// back to the rail, and `secondary-z` must still reach the worktree-level `Undo` rather than
    /// being silently swallowed by a text input the user never asked to type in.
    ///
    /// This is the regression for a real, reachable bug this issue's own first draft introduced:
    /// the fallback used to target `filter_focus_handle`, which now carries a `"text-input"` key
    /// context, so `Undo`'s `!terminal && !text-input` predicate became unsatisfiable and
    /// `TextUndo` won against an empty field - a keystroke consumed with no effect and no
    /// feedback, verbatim the bug class `crate::default_key_bindings`' own docs catalogue
    /// seven-plus instances of. Fixed by giving the rail's context-less root its own focus handle
    /// (`AdeApp::rail_focus_handle`) and pointing all three fallback sites at that instead.
    #[gpui::test]
    fn secondary_z_still_reaches_the_worktree_undo_after_focus_falls_back_to_the_rail(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        bind_real_keys(cx);

        let agent_id = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("a fresh window always starts with one real, focused shell agent");
        app.update_in(cx, |app, window, cx| {
            app.close_agent(agent_id, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.agents.active_id().is_none()
                && app.open_change.is_none()
                && !app.settings_open),
            "sanity check: this must really be the \"nothing left to focus\" fallback state, or \
             the assertion below would pass for the wrong reason"
        );
        assert!(app.read_with(cx, |app, _| app.worktree_history_status.is_none()));

        cx.simulate_keystrokes(SECONDARY_Z);

        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_history_status.clone()),
            Some("nothing to undo".to_string()),
            "with focus on the app's fallback target - not a text widget anyone chose - \
             secondary-z must still reach the worktree-level Undo and produce its real, honest \
             status, never be swallowed by an empty text field's own history"
        );
    }

    /// The fourth dangling-focus site of this shape, found by an independent adversarial audit
    /// after the three already fixed on this branch: switching Settings pages away from
    /// Keybindings left focus on that page's own now-unrendered filter field. GPUI then falls back
    /// to the dispatch root with an **empty** context stack, against which every scoped predicate
    /// is dead - so `secondary-z` reached neither undo system and vanished with no feedback at all.
    ///
    /// Asserts real feedback, not merely "the filter didn't change": silence is exactly the
    /// symptom, so a test that only checked the text would have passed against the bug.
    #[gpui::test]
    fn secondary_z_still_reaches_a_real_handler_after_switching_away_from_the_keybindings_page(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        app.update_in(cx, |app, window, cx| {
            app.open_settings(window, cx);
            app.select_settings_page(settings::SettingsPage::Keymap, window, cx);
            window.focus(&app.settings_keymap_filter_focus_handle, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("abc");
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_keymap_filter.as_str().to_string()),
            "abc",
            "sanity check: the filter must really be focused and receiving real keystrokes"
        );

        // Leave the page. The filter row stops being rendered entirely.
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(settings::SettingsPage::Appearance, window, cx);
        });
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.worktree_history_status.is_none()));
        cx.simulate_keystrokes(SECONDARY_Z);

        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_history_status.clone()),
            Some("nothing to undo".to_string()),
            "with the filter no longer rendered, secondary-z must reach a real handler and \
             produce real feedback - not fall into an empty dispatch context and vanish"
        );
    }
}
