use super::*;

impl AdeApp {
    /// Captures the real, pre-open focus target into [`Self::code_return_focus`]/
    /// [`Self::code_opened_session`] - but only the first time Surface C actually transitions
    /// from closed to open (`Self::open_change` was `None`), mirroring [`Self::open_settings`]'s
    /// own "capture once, not on every subsequent navigation" rule: a second file opened while
    /// one is already showing must not overwrite the real original target with
    /// `Self::code_focus_handle` itself (already focused at that point), which would make
    /// [`Self::close_change_diff`] restore focus onto a surface that isn't even rendered anymore
    /// instead of the real terminal pane it should return to. Always moves real focus onto
    /// [`Self::code_focus_handle`] regardless - see that field's own docs for why.
    pub(super) fn focus_code_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_change.is_none() {
            self.code_return_focus = window.focused(cx);
            self.code_opened_session = self.sessions.active_id();
        }
        window.focus(&self.code_focus_handle, cx);
    }

    /// Opens the command palette (⌘K) - `design_handoff_jerry_ade/README.md`'s "Command
    /// palette" section: resets the query/scope/selection to a fresh "browse everything" state
    /// (matching `Jerry.dc.html`'s own initial `state.scope === 'all'`, empty-query fixture)
    /// and moves real keyboard focus onto it, so the very next keystroke reaches
    /// [`Self::handle_palette_key_down`] rather than whatever had focus before. Captures
    /// whatever real focus target was in place beforehand (`window.focused(cx)`, `None` on a
    /// completely fresh window) into [`Self::palette_return_focus`], plus which session was
    /// active into [`Self::palette_opened_session`], so [`Self::close_palette`] can restore
    /// focus correctly instead of leaving it dangling on [`Self::palette_focus_handle`] once
    /// this element stops being rendered - see that field's docs for the bug this fixes.
    /// Also disarms a pending rail prune confirmation ([`Self::prune_confirm_armed`]'s docs):
    /// opening the palette is itself the kind of "did something else" gesture that should
    /// require a fresh confirmation before a later "Prune Worktrees" palette selection can
    /// execute.
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_return_focus = window.focused(cx);
        self.palette_opened_session = self.sessions.active_id();
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
    /// panel stops rendering (see that field's docs, and [`Self::palette_return_focus`]'s, for
    /// the bug this fixes: without a restore, every action dispatch - including the very next
    /// ⌘K - falls back to the root node instead of reaching
    /// [`Self::handle_toggle_palette_action`]).
    ///
    /// If the active session changed while the palette was open (e.g. a palette-run "New
    /// Shell"/"New Claude Session"/"New Codex Session" swapped which session is active - see
    /// [`Self::palette_opened_session`]'s docs), the captured pre-open handle is skipped in
    /// favor of the *current* active session's terminal pane, since a captured handle from the
    /// session that's no longer active would be exactly as untracked/stale as
    /// `palette_focus_handle` itself. Otherwise, the captured handle is restored if there was
    /// one, falling back to the active session's terminal pane if nothing was focused before
    /// (e.g. a completely fresh window that had never been clicked into).
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
            // moves focus onto - never [`Self::palette_return_focus`]/the active session's
            // terminal pane: restoring either of those would either fight `open_settings`'s own
            // focus move (the first case) or move focus onto a surface that isn't even being
            // rendered anymore, since the Settings surface still replaces the three zones (the
            // second case) - both exactly the "`Window::focus` left pointing at an untracked
            // handle" bug class [`Self::palette_return_focus`]'s own docs describe.
            window.focus(&self.settings_focus_handle, cx);
            self.palette_return_focus = None;
            self.palette_opened_session = None;
            cx.notify();
            return;
        }
        restore_focus(
            &self.sessions,
            &mut self.palette_return_focus,
            &mut self.palette_opened_session,
            window,
            cx,
        );
        cx.notify();
    }

    /// Opens the Settings surface (`design_handoff_jerry_ade/README.md`'s "Settings" section) -
    /// mirrors [`Self::open_palette`]'s exact real-focus-capture shape: captures whatever was
    /// really focused beforehand (`None` if nothing was) into [`Self::settings_return_focus`],
    /// plus which session was active into [`Self::settings_opened_session`], so
    /// [`Self::close_settings`] can restore correctly instead of leaving `Window::focus`
    /// dangling on [`Self::settings_focus_handle`] once the surface stops rendering - see
    /// [`Self::palette_return_focus`]'s docs for the exact bug this class of fix addresses.
    ///
    /// Unlike [`Self::open_palette`], this does **not** reset [`Self::settings_page`] - which
    /// page was showing persists across opens, matching ordinary settings-window UX (the
    /// palette's query/scope reset because it's a transient search, not a navigation history).
    /// Also disarms a pending rail prune confirmation, for the same reason `open_palette` does.
    ///
    /// If the palette happens to be open at the same time (e.g. the raw `cmd-,` keybinding
    /// fired while `cmd-k` was still showing), it's closed first via [`Self::close_palette`] -
    /// run while [`Self::settings_open`] is still `false`, so that call takes its own normal,
    /// non-Settings-aware restore path - rather than leaving both overlays stacked at once.
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(window, cx);
        }
        self.settings_open = true;
        self.settings_return_focus = window.focused(cx);
        self.settings_opened_session = self.sessions.active_id();
        self.prune_confirm_armed = false;
        window.focus(&self.settings_focus_handle, cx);
        self.load_agent_rows(cx);
        cx.notify();
    }

    /// Closes the Settings surface - the nav header's `esc` keycap, real `Esc` key handling
    /// (`Self::handle_settings_key_down`), and (in the palette-focus test module, matching
    /// `close_palette`'s own test coverage) direct calls. Restores real keyboard focus the same
    /// way [`Self::close_palette`] does, and for the same documented reason.
    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        restore_focus(
            &self.sessions,
            &mut self.settings_return_focus,
            &mut self.settings_opened_session,
            window,
            cx,
        );
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
    pub(in crate::root) fn open_test_app(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
    ) -> (Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| AdeApp::new(repo_path, window, cx))
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
            "first cmd-k should open the palette"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "second cmd-k should close the palette"
        );

        cx.dispatch_action(TogglePalette);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "third cmd-k - reopening after a close - is exactly the case that was broken: \
             without restoring real focus in close_palette, this dispatch had nowhere real to \
             land and silently did nothing"
        );
    }

    /// The other half of the same bug: a completely fresh window starts with `Window::focus ==
    /// None` (nothing focused until the user clicks something), so without `AdeApp::new` giving
    /// the initial session's terminal pane real focus up front, the very first cmd-k - before
    /// any click has ever happened - would also silently do nothing.
    #[gpui::test]
    fn toggle_palette_works_on_a_fresh_window_with_nothing_clicked_yet(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "cmd-k on a completely fresh window (nothing clicked yet) should still open the \
             palette"
        );
    }

    /// Spawning a session from the palette (e.g. "New Shell") swaps the active session, and the
    /// centre pane only ever renders `sessions.active()` - so a captured pre-open focus handle
    /// belonging to the *previous* active session's terminal pane would be exactly as
    /// untracked/stale as `palette_focus_handle` itself once that swap happens. Verifies
    /// `close_palette` correctly detects the active-session change and focuses the *new*
    /// session's pane instead of the stale captured one, by confirming the keyboard is left
    /// live enough for a subsequent cmd-k to still work.
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
            "cmd-k after a palette-spawned New Shell should still open the palette - the \
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

    /// `cmd-,` opens Settings, real `Esc` (simulated as an actual keystroke via `VisualTestContext::
    /// simulate_keystrokes` - `vendor/zed/crates/editor/src/edit_prediction_tests.rs`'s own
    /// `cx.simulate_keystroke("escape")` on `TestAppContext` is the verified real precedent
    /// that GPUI's keystroke parser accepts the lowercase string `"escape"` for this key)
    /// closes it, and a subsequent `cmd-k` still reaches
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
            "cmd-, should open the Settings surface"
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
            "cmd-k after closing Settings must still reach handle_toggle_palette_action - the \
             exact bug class this module exists to catch is close_settings leaving \
             Window::focus dangling on settings_focus_handle instead of restoring it"
        );
    }

    /// `cmd-,` works from a completely fresh window (nothing manually clicked into yet) - the
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
            "cmd-, on a completely fresh window (nothing clicked yet) should still open Settings"
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
            "cmd-, must still reach handle_toggle_settings_action once a plain .txt File view is \
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
            "cmd-k must still reach handle_toggle_palette_action once a File view is mounted, \
             for exactly the same real reason cmd-, must"
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
            "cmd-, must still reach handle_toggle_settings_action after closing the File view - \
             AdeApp::close_change_diff must have restored real focus onto the active session's \
             terminal pane, not left it dangling on code_focus_handle"
        );
    }
}
