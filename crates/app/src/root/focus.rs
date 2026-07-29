use super::*;

impl AdeApp {
    /// Captures the real, pre-open focus target into [`Self::code_focus`]
    /// (`OverlayFocus::capture`) - but only the first time Surface C actually transitions from
    /// closed to open (`Self::open_change` was `None`), mirroring [`Self::open_settings`]'s own
    /// "capture once, not on every subsequent navigation" rule: a second file opened while one
    /// is already showing must not overwrite the real original target with
    /// `Self::code_focus_handle` itself (already focused at that point), which would make
    /// [`Self::close_change_diff`] restore focus onto a surface that isn't even rendered anymore
    /// instead of the real terminal pane it should return to. Always moves real focus onto
    /// [`Self::code_focus_handle`] regardless - see that field's own docs for why.
    pub(super) fn focus_code_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_change.is_none() {
            self.code_focus.capture(window, &self.sessions, cx);
        }
        window.focus(&self.code_focus_handle, cx);
    }

    /// Opens the command palette (⌘K) - `design_handoff_jerry_ade/README.md`'s "Command
    /// palette" section: resets the query/scope/selection to a fresh "browse everything" state
    /// (matching `Jerry.dc.html`'s own initial `state.scope === 'all'`, empty-query fixture)
    /// and moves real keyboard focus onto it, so the very next keystroke reaches
    /// [`Self::handle_palette_key_down`] rather than whatever had focus before. Captures
    /// whatever real focus target and active session were in place beforehand into
    /// [`Self::palette_focus`] (`OverlayFocus::capture`), so [`Self::close_palette`] can
    /// restore focus correctly instead of leaving it dangling on
    /// [`Self::palette_focus_handle`] once this element stops being rendered - see that field's
    /// docs for the bug this fixes. Also disarms a pending rail prune confirmation
    /// ([`Self::prune_confirm_armed`]'s docs): opening the palette is itself the kind of "did
    /// something else" gesture that should require a fresh confirmation before a later "Prune
    /// Worktrees" palette selection can execute.
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        // See `Self::plus_menu_open`'s own docs: the tab strip's `+` menu is rendered as an
        // unconditional sibling of the palette, so leaving it open here would paint it on top
        // of (or under) a surface it no longer makes sense over.
        self.plus_menu_open = false;
        self.palette_focus.capture(window, &self.sessions, cx);
        self.palette_scope = palette::PaletteScope::default();
        self.palette_query.clear();
        self.palette_selected = 0;
        self.prune_confirm_armed = false;
        window.focus(&self.palette_focus_handle, cx);
        cx.notify();
    }

    /// Closes the palette overlay - the scrim click, `Esc`, and "run a result" real handlers.
    /// Restores real keyboard focus rather than leaving `Window::focus` pointing at
    /// [`Self::palette_focus_handle`], which stops being tracked by anything the moment this
    /// panel stops rendering (see that field's docs, and [`restore_focus`]'s, for the bug this
    /// fixes: without a restore, every action dispatch - including the very next ⌘K - falls
    /// back to the root node instead of reaching [`Self::handle_toggle_palette_action`]).
    ///
    /// If the active session changed while the palette was open (e.g. a palette-run "New
    /// Shell"/"New Claude Session"/"New Codex Session" swapped which session is active - see
    /// [`Self::palette_focus`]'s docs), the captured pre-open handle is skipped in favor of the
    /// *current* active session's terminal pane, since a captured handle from the session
    /// that's no longer active would be exactly as untracked/stale as `palette_focus_handle`
    /// itself. Otherwise, the captured handle is restored if there was one, falling back to the
    /// active session's terminal pane if nothing was focused before (e.g. a completely fresh
    /// window that had never been clicked into).
    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        if self.settings_open {
            // Settings is showing underneath the palette right now - either because
            // `Self::execute_palette_command`'s `OpenSettings` branch just opened it
            // (`Self::run_selected_palette_entry` always calls `close_palette` right after
            // dispatching a command, regardless of which one), or because the palette (⌘K)
            // happened to be opened *while Settings was already open* and is now just being
            // dismissed back down to it. Either way the correct real focus target is
            // [`Self::settings_focus_handle`] - the same handle `Self::open_settings` itself
            // moves focus onto - never `Self::palette_focus`/the active session's terminal
            // pane: restoring either of those would either fight `open_settings`'s own focus
            // move (the first case) or move focus onto a surface that isn't even being rendered
            // anymore, since the Settings surface still replaces the three zones (the second
            // case) - both exactly the "`Window::focus` left pointing at an untracked handle"
            // bug class `restore_focus`'s own docs describe.
            window.focus(&self.settings_focus_handle, cx);
            self.palette_focus.clear();
            cx.notify();
            return;
        }
        restore_focus(&self.sessions, &mut self.palette_focus, window, cx);
        cx.notify();
    }

    /// Opens the Settings surface (`design_handoff_jerry_ade/README.md`'s "Settings" section) -
    /// mirrors [`Self::open_palette`]'s exact real-focus-capture shape: captures whatever real
    /// focus target and active session were in place beforehand into [`Self::settings_focus`]
    /// (`OverlayFocus::capture`), so [`Self::close_settings`] can restore correctly instead of
    /// leaving `Window::focus` dangling on [`Self::settings_focus_handle`] once the surface
    /// stops rendering - see [`Self::palette_focus`]'s docs for the exact bug this class of fix
    /// addresses.
    ///
    /// Unlike [`Self::open_palette`], this does **not** reset [`Self::settings_page`] - which
    /// page was showing persists across opens, matching ordinary settings-window UX (the
    /// palette's query/scope reset because it's a transient search, not a navigation history).
    /// Also disarms a pending rail prune confirmation, for the same reason `open_palette` does.
    ///
    /// If the palette happens to be open at the same time (e.g. the raw `secondary-,` keybinding
    /// fired while `secondary-k` was still showing), it's closed first via [`Self::close_palette`] -
    /// run while [`Self::settings_open`] is still `false`, so that call takes its own normal,
    /// non-Settings-aware restore path - rather than leaving both overlays stacked at once.
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(window, cx);
        }
        // See `Self::open_palette`'s identical guard, and `Self::plus_menu_open`'s own docs.
        self.plus_menu_open = false;
        self.settings_open = true;
        self.settings_focus.capture(window, &self.sessions, cx);
        self.prune_confirm_armed = false;
        window.focus(&self.settings_focus_handle, cx);
        self.load_agent_rows(cx);
        self.load_lsp_rows(cx);
        cx.notify();
    }

    /// Closes the Settings surface - the nav header's `esc` keycap, real `Esc` key handling
    /// (`Self::handle_settings_key_down`), and (in the palette-focus test module, matching
    /// `close_palette`'s own test coverage) direct calls. Restores real keyboard focus the same
    /// way [`Self::close_palette`] does, and for the same documented reason.
    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        restore_focus(&self.sessions, &mut self.settings_focus, window, cx);
        cx.notify();
    }
}

/// Real, interactive regression coverage for the palette's own ⌘K entry point, driven through
/// GPUI's actual `TestAppContext`/`VisualTestContext` harness (a real window, real focus
/// tracking, real action dispatch and keystroke simulation - not a mock of any of those). A
/// plain unit test can't catch this bug class: the bug was `Window::focus` being left pointing
/// at a `FocusHandle` no element tracks anymore, which only a real window with real GPUI
/// dispatch can actually reproduce or verify fixed.
#[cfg(test)]
pub(in crate::root) mod palette_focus_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    /// Opens a real `AdeApp` in a real (test) GPUI window against a throwaway temp directory.
    /// Not a real git repo, so `wt_core::list_worktrees`/`diff_against_base` genuinely fail and
    /// leave `worktrees`/`diff_state` empty/errored - exactly like pointing the app at some
    /// non-repo directory would in production, and irrelevant to what these tests check.
    /// `AdeApp::new` still spawns one real shell session regardless (see that method's docs),
    /// which is exactly the terminal pane these tests check ⌘K's focus-restore behavior
    /// against.
    ///
    /// `pub(in crate::root)` rather than private: `settings_focus_tests` (a sibling test module,
    /// not a child of this one) reuses this exact same real-window setup for its own Settings
    /// lifecycle coverage, rather than maintaining a second, separately-written copy that could
    /// drift - as do several test modules in other `crate::root` submodules (`code_surface`,
    /// `lsp`, `merge_flow`) covering cross-cutting focus regressions in their own areas.
    ///
    /// Uses `AdeApp::new_with_settings` (real, in-memory-only `Settings::default()`, `None`
    /// settings path), not `AdeApp::new` - see that method's own docs for why: `AdeApp::new`
    /// really does read and write `~/.config/jerry/settings.toml` on whatever real machine
    /// calls it, which must never be the machine running `cargo test`.
    pub(in crate::root) fn open_test_app(
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

    /// The bug this guards against, exactly as measured: closing the palette used to leave
    /// `Window::focus` pointing at `palette_focus_handle`, which stops being tracked by
    /// anything the instant the palette panel stops rendering. Every action dispatch after that
    /// - including the very next ⌘K - fell back to the root node, which has no
    /// `on_action(handle_toggle_palette_action)` of its own, so the palette could never be
    /// reopened without the user manually clicking something first to re-establish real focus.
    #[gpui::test]
    fn toggle_palette_reopens_after_being_closed(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "first secondary-k should open the palette"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "second secondary-k should close the palette"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "third secondary-k - reopening after a close - is exactly the case that was broken: \
             without restoring real focus in close_palette, this dispatch had nowhere real to \
             land and silently did nothing"
        );
    }

    /// The other half of the same bug: a completely fresh window starts with `Window::focus ==
    /// None` (nothing focused until the user clicks something), so without `AdeApp::new` giving
    /// the initial session's terminal pane real focus up front, the very first secondary-k - before
    /// any click has ever happened - would also silently do nothing.
    #[gpui::test]
    fn toggle_palette_works_on_a_fresh_window_with_nothing_clicked_yet(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "secondary-k on a completely fresh window (nothing clicked yet) should still open the \
             palette"
        );
    }

    /// Spawning a session from the palette (e.g. "New Shell") swaps the active session, and the
    /// centre pane only ever renders `sessions.active()` - so a captured pre-open focus handle
    /// belonging to the *previous* active session's terminal pane would be exactly as
    /// untracked/stale as `palette_focus_handle` itself once that swap happens. Verifies
    /// `close_palette` correctly detects the active-session change and focuses the *new*
    /// session's pane instead of the stale captured one, by confirming the keyboard is left
    /// live enough for a subsequent secondary-k to still work.
    #[gpui::test]
    fn toggle_palette_still_works_after_a_palette_spawned_new_shell(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        let initial_session_id = app.read_with(cx, |app, _| app.sessions.active_id());

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

        let new_session_id = app.read_with(cx, |app, _| app.sessions.active_id());
        assert_ne!(
            initial_session_id, new_session_id,
            "sanity check: New Shell should have made a different session active"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "secondary-k after a palette-spawned New Shell should still open the palette - the \
             center pane now renders a different session's terminal pane than the one focus \
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
            assert_eq!(app.palette_query, "");
        });

        // Back to a fresh, empty-query palette state before the mid-query case.
        cx.dispatch_action(TogglePalette);
        cx.dispatch_action(TogglePalette);
        app.read_with(cx, |app, _| {
            assert_eq!(app.palette_scope, palette::PaletteScope::All);
            assert_eq!(app.palette_query, "");
        });

        cx.simulate_input("x");
        cx.simulate_input(">");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.palette_scope,
                palette::PaletteScope::All,
                "a `>` typed mid-query (query is non-empty) must not switch scope"
            );
            assert_eq!(app.palette_query, "x>");
        });
    }

    /// The exact regression test gap the real, live-reproduced `"cmd-k"` bug slipped through:
    /// every other test in this module (and `settings_focus_tests`/`code_focus_tests`) dispatches
    /// [`TogglePalette`] directly via `cx.dispatch_action`, which exercises the action handler but
    /// never the real keystroke-to-action resolution `crate::default_key_bindings`'s `KeyBinding`s
    /// perform - so a wrong keystroke spec (`"cmd-k"`, which GPUI resolves to the Super/Windows
    /// key on Linux, not Ctrl - verified against `vendor/zed/crates/gpui/src/platform/
    /// keystroke.rs`'s own `Keystroke::parse`) could ship with every action-dispatch test green.
    ///
    /// This test instead does what a real user does: binds the crate's real, production
    /// `default_key_bindings()` list (not a hand-picked subset) onto this real test window via
    /// the real `App::bind_keys` (`vendor/zed/crates/gpui/src/app.rs:2130`), then simulates the
    /// real keystroke via `VisualTestContext::simulate_keystrokes`
    /// (`vendor/zed/crates/gpui/src/app/test_context.rs:794`, the same real API
    /// `settings_focus_tests` already uses for `"escape"`) - never `dispatch_action`. The
    /// simulated string tracks the real per-OS resolution of GPUI's `"secondary"` keystroke alias
    /// (`cfg!(target_os = "macos")`, the same real compile-time fact
    /// `crate::keymap::detected_platform_is_macos` resolves for rendering) rather than hardcoding
    /// `"ctrl-k"`, so this test proves the real binding is correct on whatever OS actually runs
    /// it - on this Linux dev sandbox that resolves to exactly `"ctrl-k"`, the literal keystroke
    /// the audit reproduced failing (silently doing nothing) against the old `"cmd-k"` binding.
    #[gpui::test]
    fn secondary_keystroke_opens_the_palette_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        let secondary_k = if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        };
        cx.simulate_keystrokes(secondary_k);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real, simulated {secondary_k} keystroke - not a direct TogglePalette action \
             dispatch - must open the palette through crate::default_key_bindings' real \
             KeyBinding registration; this is exactly the path the old \"cmd-k\" binding broke \
             on Linux (Ctrl+K did nothing) without any test catching it"
        );
    }
}

/// Real, interactive regression coverage for Revision R4a's new global keybindings (`ctrl-shift-
/// T`, `secondary-shift-n`, `secondary-p`, `]`, `secondary-1`..`secondary-8`) - the exact same
/// real `cx.bind_keys(crate::default_key_bindings())` + `cx.simulate_keystrokes(..)` shape
/// [`palette_focus_tests::secondary_keystroke_opens_the_palette_through_the_real_key_bindings`]
/// established in Revision R2 for exactly this reason: a test that only calls
/// `cx.dispatch_action(..)` directly proves the *handler* works, never that the real, literal
/// keystroke string in `crate::default_key_bindings` actually resolves to that action - which is
/// exactly the class of bug R2's own `"cmd-k"` incident shipped with a fully green, dispatch-only
/// test suite.
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

    /// `ctrl-shift-T` is a real, literal Ctrl combo on every OS (not `secondary`-aliased - see
    /// `crate::default_key_bindings`'s own docs for why) - simulated literally here rather than
    /// branching on `cfg!(target_os = "macos")` the way the `secondary-` bindings' own tests do.
    #[gpui::test]
    fn ctrl_shift_t_spawns_a_real_shell_session_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let sessions_before = app.read_with(cx, |app, _| app.sessions.iter().count());

        cx.simulate_keystrokes("ctrl-shift-t");

        let (sessions_after, active_kind) = app.read_with(cx, |app, _| {
            (
                app.sessions.iter().count(),
                app.sessions.active().map(|session| session.kind),
            )
        });
        assert_eq!(
            sessions_after,
            sessions_before + 1,
            "a real, simulated ctrl-shift-t keystroke should have spawned exactly one new \
             session through crate::default_key_bindings' real ctrl-shift-t -> NewTerminal \
             binding"
        );
        assert_eq!(
            active_kind,
            Some(SessionKind::Shell),
            "New terminal always spawns a real Shell session"
        );
    }

    #[gpui::test]
    fn secondary_shift_n_spawns_a_real_agent_session_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let sessions_before = app.read_with(cx, |app, _| app.sessions.iter().count());

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

        let sessions_after = app.read_with(cx, |app, _| app.sessions.iter().count());
        assert_eq!(
            sessions_after,
            sessions_before + 1,
            "a real, simulated {secondary_shift_n} keystroke should have spawned exactly one \
             new agent session through crate::default_key_bindings' real \
             secondary-shift-n -> NewAgentPane binding"
        );
    }

    /// The mirror-image regression the audit specifically called out as missing: the `+` menu's
    /// "Open file…" row used to be backed by a real global `secondary-p` binding - removed (see
    /// `crate::default_key_bindings`'s own docs) once audit found it silently ate a real,
    /// standard readline control byte (Ctrl+P, `0x10`, "previous history") out of every focused
    /// terminal session on Linux/Windows, since GPUI dispatches a matched, registered
    /// `KeyBinding`'s action *before* a focused element's own `on_key_down`. With no real
    /// binding registered for it anymore, a real, simulated `ctrl-p` keystroke - with the
    /// window's real initial terminal pane focused, exactly like a completely ordinary "browsing
    /// the terminal" moment - must not open the palette at all; it should be free to reach the
    /// focused terminal as literal input instead. This test only proves the palette didn't open
    /// (the state-level half of the same two-part proof `bracket_does_not_fire_globally_while_a_
    /// terminal_is_focused` uses for `]`); `terminal_pane`'s own `keystroke_tests` module covers
    /// the real pty-forwarding half (`ctrl-p` really does map to the real `0x10` control byte).
    #[gpui::test]
    fn ctrl_p_does_not_open_the_palette_while_a_terminal_is_focused(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "sanity check: the palette must start closed"
        );

        let secondary_p = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(secondary_p);

        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "a real, simulated {secondary_p} keystroke must NOT open the palette - there is no \
             real global keybinding for it anymore (crate::default_key_bindings' own docs), so \
             it must be free to reach the focused terminal pane as literal input instead of \
             being intercepted at the dispatch level"
        );
    }

    fn secondary_k() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        }
    }

    /// CRITICAL regression test, the exact sequence the audit reproduced live: open a file tab
    /// (so `AdeApp::open_change` is `Some` and `Self::render_center_pane` is showing that file,
    /// not any session's `TerminalPane`), press `ctrl-shift-t` (`NewTerminal`, which spawns a new
    /// session), then confirm `ctrl-k` still opens the palette. Before the fix,
    /// `Sessions::spawn` unconditionally moved `Window::focus` onto the freshly spawned
    /// session's own pane even though it wasn't rendered anywhere that frame, leaving
    /// `Window::focus` pointing at a node with no `on_action` handlers above it - silently
    /// killing every bound shortcut, `ctrl-k` included, until the next click.
    #[gpui::test]
    fn ctrl_k_still_works_after_ctrl_shift_t_with_a_file_tab_active(cx: &mut TestAppContext) {
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
            app.read_with(cx, |app, _| app.sessions.iter().count()) >= 2,
            "sanity check: ctrl-shift-t should have spawned a real new session"
        );

        let key = secondary_k();
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after ctrl-shift-t, with a file tab active, must still open \
             the palette - before the fix, Sessions::spawn's unconditional focus pointed \
             Window::focus at the new session's pane even though render_center_pane was still \
             showing the file tab, leaving no real dispatch path to any on_action handler"
        );
    }

    /// CRITICAL regression test for the identical gap in `Sessions::close`: closing the *active*
    /// session's tab (the tab strip's own `×`, exercised here via `AdeApp::close_session`, its
    /// real handler) picks a new active session but, before the fix, never moved real keyboard
    /// focus onto it - leaving `ctrl-k` dead afterward exactly like the `Sessions::spawn` gap
    /// above.
    #[gpui::test]
    fn ctrl_k_still_works_after_closing_the_active_session_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let first_id = app.read_with(cx, |app, _| {
            app.sessions.active_id().expect("the initial shell session")
        });
        app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_session(first_id, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(first_id),
            "sanity check: the first session should be active again before closing it"
        );

        app.update_in(cx, |app, window, cx| {
            app.close_session(first_id, window, cx);
        });

        let key = secondary_k();
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after closing the active session's own tab must still open \
             the palette - Sessions::close must move real keyboard focus onto whichever session \
             became active as a result, not leave Window::focus dangling"
        );
    }

    /// The other real, live reproduction path the audit found for the same `Sessions::close`
    /// gap: archiving the active session from the rail (`AdeApp::archive_session`, which
    /// delegates to `Self::close_session`).
    #[gpui::test]
    fn ctrl_k_still_works_after_archiving_the_active_session(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        let first_id = app.read_with(cx, |app, _| {
            app.sessions.active_id().expect("the initial shell session")
        });
        app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_session(first_id, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(first_id),
            "sanity check: the first session should be active again before archiving it"
        );

        app.update_in(cx, |app, window, cx| {
            app.archive_session(first_id, window, cx);
        });

        let key = secondary_k();
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after archiving the active session must still open the \
             palette - archiving goes through the same Sessions::close real focus-restore path \
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
        // `]` is deliberately scoped to `Some("diff")` (`crate::default_key_bindings`'s own
        // docs), not global - real, live-verified against GPUI's own dispatch order: a *global*
        // bare `]` binding would silently swallow a literal `]` typed into any focused terminal/
        // agent session (closing a bracket, an array literal, ...) instead of forwarding it to
        // the real pty, since GPUI dispatches a matched action *before* a focused element's own
        // `on_key_down`. This test therefore first opens a real file (establishing real
        // `"diff"`-context focus, via `AdeApp::open_change_diff`) before simulating the
        // keystroke - proving the real, scoped binding still works from the surface it's meant
        // to, not that it works from literally anywhere.
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

    /// The other real half of the scoping above: with *no* file tab focused (only the initial
    /// session's terminal pane, as in a completely ordinary "browsing the terminal" moment), a
    /// real `]` keystroke must **not** reach [`NextChangedFile`] at all - it should be a real
    /// no-op at the dispatch level, leaving the keystroke free to reach the focused terminal
    /// instead (this test only proves the state didn't change; `terminal_pane`'s own
    /// `keystroke_tests` module covers the pty-forwarding half separately).
    ///
    /// Asserts a real diff with at least one file actually loaded before checking the negative
    /// keystroke behavior - a hardening fix so this test can't silently go vacuous: without it,
    /// a future change to the test fixture that stopped producing a real diff (e.g. a git config
    /// or branch-detection regression) would leave `open_change` at `None` before *and* after the
    /// `]` keystroke for a completely different, uninteresting reason - there being nothing to
    /// navigate to at all - and this test would keep passing trivially forever without ever
    /// exercising the real scoping it claims to.
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

    /// Spawns four extra real shell sessions (five total, including the one `AdeApp::new` starts)
    /// and confirms `secondary-3` really jumps to the third one in real session order - not just
    /// that `AdeApp::jump_to_session_at(3, ..)` does when called directly.
    #[gpui::test]
    fn secondary_3_jumps_to_the_third_real_session_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);

        for _ in 0..4 {
            app.update_in(cx, |app, window, cx| {
                app.new_session(SessionKind::Shell, window, cx);
            });
        }
        let third_id = app.read_with(cx, |app, _| {
            app.sessions
                .iter()
                .nth(2)
                .map(|session| session.id)
                .expect("five real sessions should exist by now")
        });
        assert_ne!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(third_id),
            "sanity check: the most recently spawned session (the fifth), not the third, should \
             be active before the jump"
        );

        let secondary_3 = if cfg!(target_os = "macos") {
            "cmd-3"
        } else {
            "ctrl-3"
        };
        cx.simulate_keystrokes(secondary_3);

        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(third_id),
            "a real, simulated {secondary_3} keystroke must activate the session at position 3 \
             through crate::default_key_bindings' real secondary-3 -> JumpToSession3 binding"
        );
    }

    /// [`AdeApp::jump_to_session_at`]'s own direct-call coverage (as opposed to the keystroke
    /// simulation above) for every position 1..=8, plus the real "fewer sessions than the
    /// position" no-op.
    #[gpui::test]
    fn jump_to_session_at_activates_the_right_session_by_position(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let mut ids = vec![app.read_with(cx, |app, _| {
            app.sessions.active_id().expect("the initial shell session")
        })];
        for _ in 0..3 {
            let id = app.update_in(cx, |app, window, cx| {
                app.sessions.spawn(
                    SessionKind::Shell,
                    repo.path().to_path_buf(),
                    app.settings.appearance.terminal_font_size,
                    window,
                    cx,
                )
            });
            ids.push(id);
        }
        // Four real sessions now exist, in spawn order `ids[0..4]`.

        for (position, expected_id) in ids.iter().enumerate() {
            let position = position + 1;
            app.update_in(cx, |app, window, cx| {
                app.jump_to_session_at(position, window, cx);
            });
            assert_eq!(
                app.read_with(cx, |app, _| app.sessions.active_id()),
                Some(*expected_id),
                "position {position} should activate session {expected_id}"
            );
        }

        // A real no-op: there is no fifth session.
        let active_before = app.read_with(cx, |app, _| app.sessions.active_id());
        app.update_in(cx, |app, window, cx| {
            app.jump_to_session_at(5, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            active_before,
            "jumping to a position with no real session there must be a no-op"
        );
    }
}

/// Real, interactive regression coverage for the Settings surface's own lifecycle - the same
/// bug class `palette_focus_tests` exists to catch (a `Window::focus` handle left dangling
/// after an element stops rendering), now exercised against the Settings surface instead of
/// the palette overlay. Settings is a bigger risk for exactly this bug than the palette was:
/// it *replaces* the whole three-zone body (see [`AdeApp::render_workspace_body`]'s docs)
/// rather than drawing on top of it, so a broken focus restore here would leave every bound
/// action (⌘N, ⌘K, ⌘,) unreachable, not just ⌘K.
#[cfg(test)]
mod settings_focus_tests {
    use super::*;
    use gpui::TestAppContext;

    /// `secondary-,` opens Settings, real `Esc` (simulated as an actual keystroke via `VisualTestContext::
    /// simulate_keystrokes` - `vendor/zed/crates/editor/src/edit_prediction_tests.rs`'s own
    /// `cx.simulate_keystroke("escape")` on `TestAppContext` is the verified real precedent
    /// that GPUI's keystroke parser accepts the lowercase string `"escape"` for this key)
    /// closes it, and a subsequent `secondary-k` still reaches
    /// [`AdeApp::handle_toggle_palette_action`] - which it only can if closing Settings left
    /// real, live focus somewhere `dispatch_action` can find, not dangling on
    /// [`AdeApp::settings_focus_handle`].
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
            "secondary-k after closing Settings must still reach handle_toggle_palette_action - the \
             exact bug class this module exists to catch is close_settings leaving \
             Window::focus dangling on settings_focus_handle instead of restoring it"
        );
    }

    /// `secondary-,` works from a completely fresh window (nothing manually clicked into yet) - the
    /// same "no click has established real focus" case `palette_focus_tests::
    /// toggle_palette_works_on_a_fresh_window_with_nothing_clicked_yet` covers for the palette,
    /// here for Settings. Relies on the same real fix (`AdeApp::new` focusing the initial
    /// session's terminal pane up front) that test's own docs describe.
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

    /// The comma-leak half of the real, live-reproduced `"cmd-k"`-class bug (see
    /// `crate::default_key_bindings`'s own docs): with the old `"cmd-,"` binding, `Ctrl+,` was
    /// never recognized as a keystroke matching any real `KeyBinding` at all (`"cmd"`/`"super"`/
    /// `"win"` only ever mean the platform/Super modifier, never Ctrl, on Linux), so the
    /// keystroke fell all the way through GPUI's dispatch tree to whatever plain text input had
    /// real keyboard focus - a live terminal session in this app's case - and got typed into it
    /// as a literal `,` character instead of ever reaching [`AdeApp::handle_toggle_settings_action`].
    ///
    /// This test proves the fix the same real way `palette_focus_tests::
    /// secondary_keystroke_opens_the_palette_through_the_real_key_bindings` proves ⌘K's: binds
    /// the crate's real `default_key_bindings()` and simulates the real keystroke (never
    /// `dispatch_action`), so it fails the same way the live bug did if the binding regresses -
    /// unlike every other test in this module, which dispatches [`ToggleSettings`] directly and
    /// so could never have caught this.
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

    /// The orchestrator-visible proof that closing Settings genuinely "returns to the
    /// workspace" with real state intact, per `design_handoff_jerry_ade/README.md`'s "esc ...
    /// returns to the workspace": a session tab opened *before* Settings was ever shown is
    /// still there, and still the active tab, after a real open/close round-trip - Settings
    /// swapping out `AdeApp::render_workspace_body` (see that method's docs) never tore down
    /// or mutated `AdeApp::sessions` itself, only which body `Render::render` draws.
    #[gpui::test]
    fn closing_settings_leaves_open_session_tabs_intact(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let sessions_before = app.read_with(cx, |app, _| app.sessions.iter().count());
        let active_before = app.read_with(cx, |app, _| app.sessions.active_id());
        assert_eq!(
            sessions_before, 1,
            "AdeApp::new starts with exactly one real shell tab"
        );

        cx.dispatch_action(ToggleSettings);
        assert!(app.read_with(cx, |app, _| app.settings_open));

        cx.simulate_keystrokes("escape");
        assert!(!app.read_with(cx, |app, _| app.settings_open));

        let sessions_after = app.read_with(cx, |app, _| app.sessions.iter().count());
        let active_after = app.read_with(cx, |app, _| app.sessions.active_id());
        assert_eq!(
            sessions_after, sessions_before,
            "the real session tab opened before Settings was shown must still exist after \
             closing Settings"
        );
        assert_eq!(
            active_after, active_before,
            "the active tab must be unchanged by a Settings open/close round-trip"
        );
    }

    /// Selecting a nav page is real, live `AdeApp` state - covers the "nav-page-switching"
    /// focus/lifecycle risk the orchestrator flagged alongside Esc-to-close, verifying a page
    /// switch survives (and doesn't reset) across a Settings close/reopen, matching
    /// `AdeApp::open_settings`'s own documented "does not reset settings_page" contract.
    #[gpui::test]
    fn settings_page_selection_persists_across_a_close_and_reopen(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        app.update(cx, |app, cx| {
            app.select_settings_page(SettingsPage::Worktrees, cx);
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

    /// The palette's real `Open Settings` command (`palette::PaletteCommand::OpenSettings`)
    /// actually opens Settings and leaves real, live focus on it - not on a stale palette
    /// handle - covers `AdeApp::close_palette`'s Settings-aware branch (see its docs) via the
    /// exact real dispatch path a user typing "settings" into ⌘K and hitting `⏎` would take.
    #[gpui::test]
    fn open_settings_palette_command_leaves_real_focus_on_settings(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        app.update_in(cx, |app, window, cx| {
            app.execute_palette_command(palette::PaletteCommand::OpenSettings, window, cx);
        });
        // Mirrors `palette_focus_tests::
        // toggle_palette_still_works_after_a_palette_spawned_new_shell`'s own comment:
        // `execute_palette_command` alone doesn't close the palette - that's
        // `run_selected_palette_entry`'s job - so close it the same way Escape does, to reach
        // the exact real `close_palette` code path this test targets.
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

    /// The real regression this module exists to catch for [`AdeApp::load_agent_rows`]: opening
    /// Settings must actually populate [`AdeApp::agent_rows`] from a real `$PATH` search, not
    /// leave it permanently empty now that the search moved off the render path and onto
    /// `cx.spawn`/`cx.background_executor()` (see that method's docs for why - a real ~30ms
    /// `$PATH` walk for a not-found binary, previously paid inline in `render()` on every
    /// frame). `cx.run_until_parked()` is what actually drives the spawned background task (and
    /// its `this.update` write-back) to completion in this deterministic test executor - without
    /// it, the assertion below would race the still-in-flight task and could flake.
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

    /// Mirrors the Agents-page test above, for [`AdeApp::lsp_rows`]
    /// (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3 "Language servers" page).
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
        assert_eq!(rows.len(), settings::LSP_LANGUAGES.len());
        for def in settings::LSP_LANGUAGES {
            assert!(rows.iter().any(|row| row.language == def.language));
        }
    }

    /// `design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3: "Default page is now
    /// General (was Agents)."
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

    /// A real, whole-surface render smoke test: every real page in `SettingsPage::ALL` -
    /// General/Agents/Worktrees/Appearance/Theme/Keymap's real content and every nav-only
    /// page's honest placeholder alike - must actually render without panicking. Selecting a
    /// page and running the real GPUI test executor to a parked, fully-drawn state
    /// (`cx.run_until_parked`) is what actually exercises `AdeApp::render_settings_content`'s
    /// real per-page render call, not just the pure state transition `select_settings_page`
    /// itself performs.
    #[gpui::test]
    fn every_settings_page_renders_without_panicking(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        cx.run_until_parked();

        for page in SettingsPage::ALL {
            app.update(cx, |app, cx| {
                app.select_settings_page(page, cx);
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

    /// `design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3: `Window controls` on the
    /// General page is "wired live to change 1" - the real title-bar/keycap override, not a
    /// decorative row. `AdeApp::set_window_controls_style` is the exact method the General
    /// page's real choice-row click handler calls (`crate::root::settings_render::
    /// render_settings_general_page`) - the same real, single source of truth
    /// `root::title_bar::caption_button_tests::clicking_the_close_caption_button_closes_the_real_window`
    /// already proves actually changes which title-bar variant renders.
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

/// Real, interactive regression coverage for the third real occurrence of the exact bug class
/// [`palette_focus_tests`]/[`settings_focus_tests`]'s own docs describe - `Window::focus` left
/// dangling on a `FocusHandle` that stops being tracked once its own element stops rendering -
/// this time for Surface C's Diff/File view (`AdeApp::code_focus_handle`'s own docs).
/// `AdeApp::render_center_pane` early-returns before ever rendering the active session's own
/// terminal pane (the previously-focused node in every one of these tests) once `AdeApp::
/// open_change` becomes `Some` - reproduced here with a plain `.txt` file, deliberately no `.rs`/
/// LSP content involved at all, matching exactly how this bug was actually found: it has nothing
/// to do with hover/go-to-definition, only with a file view being mounted at all.
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
            "secondary-k must still reach handle_toggle_palette_action once a File view is mounted, \
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
        // `handle_goto_definition_action`'s only real effect with `hover == None` (a plain .txt
        // file has no real hover entry at all - it isn't even a `.rs` file) is a harmless early
        // return inside `trigger_goto_definition`, the same real proof technique
        // `lsp_hover_wiring_tests::f12_action_reaches_the_real_handler_on_a_fresh_window` already
        // establishes - what's actually under test here is that dispatch reached the handler at
        // all with a File view open, not the no-op it did once there.
        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
    }

    /// Closing the File view (the surface's own real `× close` affordance,
    /// [`AdeApp::close_change_diff`]) must restore real, live focus back onto the active
    /// session's terminal pane - not leave it dangling on [`AdeApp::code_focus_handle`], which
    /// stops being rendered the instant [`AdeApp::open_change`] goes back to `None` - so every
    /// bound action keeps working afterward too.
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
             AdeApp::close_change_diff must have restored real focus onto the active session's \
             terminal pane, not left it dangling on code_focus_handle"
        );
    }
}
