//! GitHub issue #235: the state-aware half of `crate::title_bar::menu_model::MenuCommand` - is a
//! given command enabled right now ([`AdeApp::menu_command_enabled`]), and what does picking it
//! actually do ([`AdeApp::perform_menu_command`]). `menu_model` itself is deliberately
//! window/state-free (see that module's own docs for why); the two functions here are what layer
//! real `AdeApp` state on top of its pure data, and they are the **only** place either question
//! is answered - both the Windows/Linux in-window popover (`crate::title_bar::menu`) and the real
//! macOS menu (`crate::title_bar::native_menu`) go through them rather than each keeping its own
//! copy, so enablement and effect can never drift between the two surfaces.
use super::*;
use crate::title_bar::menu_model::MenuCommand;

impl AdeApp {
    /// Whether `cmd` genuinely has something to do right now, using the exact same real
    /// predicates the pre-issue-#235 popover rows already dimmed on (see each arm's own
    /// commentary for which `crate::title_bar::menu` row it replaces). Commands with no real
    /// precondition - opening a picker, opening Settings, closing this window, the application
    /// menu's Hide/Show/Quit quartet - are always enabled, matching those rows' own `true` literal
    /// today.
    pub(crate) fn menu_command_enabled(&self, cmd: MenuCommand) -> bool {
        match cmd {
            MenuCommand::Save => self.active_edit_buffer().is_some(),
            MenuCommand::Undo => self
                .active_edit_buffer()
                .is_some_and(|buffer| buffer.can_undo()),
            MenuCommand::Redo => self
                .active_edit_buffer()
                .is_some_and(|buffer| buffer.can_redo()),
            MenuCommand::Cut | MenuCommand::Copy | MenuCommand::Paste | MenuCommand::SelectAll => {
                self.active_edit_buffer().is_some()
            }
            // The same `code_surface_showing` predicate `crate::title_bar::menu::view_menu_rows`
            // computes today: Surface C (the File/Diff code view) must genuinely be on screen for
            // a zoom level to mean anything.
            MenuCommand::ZoomIn | MenuCommand::ZoomOut | MenuCommand::ResetZoom => {
                self.open_change.is_some()
                    && (self.open_diff_file_cache.is_some()
                        || self.code_view == code_view::CodeView::File)
            }
            // GitHub issue #90: a genuinely empty window has no real repo root to spawn an agent
            // into - see `crate::work_surface::render::AdeApp::new_agent`'s own docs.
            MenuCommand::NewTerminal | MenuCommand::NewAgentPane => self.focused_repo().is_some(),
            // Exactly what `select_relative_agent` can actually do, so the row is never enabled
            // over a no-op: with the active tab already *being* the worktree's only agent
            // session there is nowhere to cycle to, but from a shell tab (or any other non-agent
            // pane) a single agent session is a real destination - see that method's own docs
            // for the entry-from-outside-the-cycle case, and GitHub issue #381 for why a shell
            // stopped counting as a stop on it.
            MenuCommand::NextAgent | MenuCommand::PreviousAgent => {
                let ids: Vec<_> = self
                    .current_worktree_agent_sessions()
                    .map(|agent| agent.id)
                    .collect();
                match self.agents.active_id() {
                    None => false,
                    // Already sitting on the cycle: it has to have somewhere else to go.
                    Some(active) if ids.contains(&active) => ids.len() > 1,
                    // Entering it from outside (a shell tab is the common one): any single
                    // agent session is a real destination.
                    Some(_) => !ids.is_empty(),
                }
            }
            MenuCommand::ArchiveAgent => self.agents.active_id().is_some(),
            // GitHub issue #295 moved the agent review door here from the pane footer, which §4r
            // emptied ("a finished transcript is a record; its actions live where their object
            // lives"). It keeps issue #225's own gate exactly: a review needs a real captured
            // baseline, and it is only meaningful when this agent is the sole agent in its
            // worktree - see `crate::review::flow::AdeApp::review_available_for`.
            MenuCommand::ReviewAgent => self
                .agents
                .active_id()
                .is_some_and(|id| self.review_available_for(id)),
            MenuCommand::KeepAllChanges => {
                self.agents.active_id().is_some()
                    && self.worktree_history_op_in_flight
                        != Some(worktree_history::WorktreeHistoryOpKind::Keep)
            }
            MenuCommand::DiscardWorktree => {
                self.agents.active_id().is_some()
                    && self.worktree_history_op_in_flight
                        != Some(worktree_history::WorktreeHistoryOpKind::Discard)
            }
            MenuCommand::OpenFile
            | MenuCommand::OpenFolder
            | MenuCommand::NewWindow
            | MenuCommand::Settings
            | MenuCommand::CloseWindow
            | MenuCommand::CommandPalette
            | MenuCommand::Documentation
            | MenuCommand::ReportIssue
            | MenuCommand::About
            | MenuCommand::Hide
            | MenuCommand::HideOthers
            | MenuCommand::ShowAll
            | MenuCommand::Quit => true,
        }
    }

    /// Runs `cmd`'s real effect - the same call, in the same order, the pre-issue-#235 popover's
    /// own `on_click` closure for that row already made. Never called for a `cmd` this instant's
    /// [`Self::menu_command_enabled`] says is `false`: every real caller (the popover's row
    /// `on_click`, and `handle_*_menu_command` below, itself only ever attached to the dispatch
    /// tree while enabled) already guards on that first.
    pub(crate) fn perform_menu_command(
        &mut self,
        cmd: MenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match cmd {
            MenuCommand::OpenFile => {
                self.title_menu_open = None;
                self.open_palette(window, cx);
                // `open_palette` always resets `palette_scope` to `PaletteScope::default()`, so
                // this must be set after it returns, not before - see
                // `crate::title_bar::menu::AdeApp::file_menu_rows`'s identical historical row for
                // the same ordering requirement.
                self.palette_scope = palette::PaletteScope::Files;
                cx.notify();
            }
            MenuCommand::OpenFolder => {
                self.title_menu_open = None;
                self.start_choose_repo_folder(cx);
                cx.notify();
            }
            MenuCommand::NewWindow => {
                self.title_menu_open = None;
                let options = crate::default_window_options(cx);
                let settings = self.settings.clone();
                let settings_path = self.settings_path.clone();
                let opened = cx.open_window(options, move |window, cx| {
                    cx.new(|cx| {
                        AdeApp::new_with_settings(None, false, settings, settings_path, window, cx)
                    })
                });
                if let Err(err) = opened {
                    log::error!("failed to open a new ADE window: {err}");
                }
                cx.notify();
            }
            MenuCommand::Save => {
                self.title_menu_open = None;
                self.handle_editor_save_action(&EditorSave, window, cx);
            }
            MenuCommand::Settings => {
                self.title_menu_open = None;
                self.open_settings(window, cx);
            }
            MenuCommand::CloseWindow => {
                // The real `Window::remove_window` the title bar's own close control (both the
                // macOS dot and the Windows/Linux caption button) already calls - see
                // `crate::title_bar::render::AdeApp::render_window_controls`'s own docs. This row
                // was labelled "Quit" before issue #235; it never actually quit the app (see
                // `MenuCommand::label`'s own docs for why it was renamed).
                window.remove_window();
            }
            MenuCommand::Undo => {
                self.title_menu_open = None;
                self.perform_text_undo(cx);
            }
            MenuCommand::Redo => {
                self.title_menu_open = None;
                self.perform_text_redo(cx);
            }
            MenuCommand::Cut => {
                self.title_menu_open = None;
                self.handle_editor_cut_action(&EditorCut, window, cx);
            }
            MenuCommand::Copy => {
                self.title_menu_open = None;
                self.handle_editor_copy_action(&EditorCopy, window, cx);
            }
            MenuCommand::Paste => {
                self.title_menu_open = None;
                self.handle_editor_paste_action(&EditorPaste, window, cx);
            }
            MenuCommand::SelectAll => {
                self.title_menu_open = None;
                self.handle_editor_select_all_action(&EditorSelectAll, window, cx);
            }
            MenuCommand::CommandPalette => {
                self.title_menu_open = None;
                self.open_palette(window, cx);
            }
            MenuCommand::ZoomIn => self.zoom_in(cx),
            MenuCommand::ZoomOut => self.zoom_out(cx),
            MenuCommand::ResetZoom => self.reset_zoom(cx),
            MenuCommand::NewTerminal => {
                self.title_menu_open = None;
                self.handle_new_terminal_action(&NewTerminal, window, cx);
            }
            MenuCommand::NewAgentPane => {
                self.title_menu_open = None;
                self.handle_new_agent_pane_action(&NewAgentPane, window, cx);
            }
            MenuCommand::NextAgent => {
                self.title_menu_open = None;
                self.select_relative_agent(1isize, window, cx);
            }
            MenuCommand::PreviousAgent => {
                self.title_menu_open = None;
                self.select_relative_agent(-1isize, window, cx);
            }
            MenuCommand::ArchiveAgent => {
                if let Some(id) = self.agents.active_id() {
                    self.title_menu_open = None;
                    self.archive_agent(id, window, cx);
                }
            }
            MenuCommand::ReviewAgent => {
                if let Some(id) = self.agents.active_id() {
                    self.title_menu_open = None;
                    self.open_review_tab(id, window, cx);
                }
            }
            MenuCommand::KeepAllChanges => {
                if let Some(id) = self.agents.active_id() {
                    self.title_menu_open = None;
                    self.keep_all_changes(id, cx);
                }
            }
            MenuCommand::DiscardWorktree => {
                if let Some(id) = self.agents.active_id() {
                    self.request_discard_worktree(id, window, cx);
                    // The first click only arms confirmation
                    // (`AdeApp::discard_confirm_armed`) - keep the menu open so its own row can
                    // swap to "confirm discard?" for a real second click, mirroring the agent
                    // footer's identical two-step button and the pre-issue-#235 popover's own
                    // `agent_menu_rows` handler for this exact row.
                    if self.discard_confirm_armed != Some(id) {
                        self.title_menu_open = None;
                    }
                    cx.notify();
                }
            }
            MenuCommand::Documentation => {
                self.title_menu_open = None;
                cx.open_url("https://github.com/ColinEspinas/jerry#readme");
                cx.notify();
            }
            MenuCommand::ReportIssue => {
                self.title_menu_open = None;
                cx.open_url("https://github.com/ColinEspinas/jerry/issues");
                cx.notify();
            }
            MenuCommand::About => {
                self.title_menu_open = None;
                self.open_settings(window, cx);
                self.select_settings_page(settings::SettingsPage::About, window, cx);
            }
            // The macOS application menu's own quartet - kept here purely so this `match` stays
            // exhaustive and every command's real effect is documented in the one place this
            // module's own docs promise, even though this arm is never actually reached in
            // practice: `MenuCommand::app_menu_rows` (the only place these four appear in any
            // menu) is only ever consumed by the macOS-only native menu
            // (`crate::title_bar::native_menu`), never by the Windows/Linux popover, which is
            // this function's only real caller. The real live path for these four is `crate::run`'s
            // global `cx.on_action` listeners - registered at the `App` level (no `AdeApp`/`Window`
            // in scope for a menu click with no window focused, e.g. Quit from the Dock menu),
            // calling the exact same `gpui::App`/`Context` methods as here.
            MenuCommand::Hide => cx.hide(),
            MenuCommand::HideOthers => cx.hide_other_apps(),
            MenuCommand::ShowAll => cx.unhide_other_apps(),
            MenuCommand::Quit => cx.quit(),
        }
    }

    // The `handle_*_menu_command` methods below are the real macOS-menu-reachable half of every
    // `MenuCommand` that has no existing action handler anywhere else in the tree - see this
    // module's own "Not every command gets a `handle_*_menu_command` here" docs for the commands
    // deliberately excluded (they keep their own existing, correctly-scoped handler untouched).
    // Each one is a one-line delegation to `Self::perform_menu_command`, registered on
    // `impl Render for AdeApp`'s root element in `crate::root::mod` - unconditionally for a
    // command `Self::menu_command_enabled` always reports `true` for, or behind
    // `.when(self.menu_command_enabled(..), ..)` for one that can genuinely be disabled.

    pub(crate) fn handle_open_file_menu_command(
        &mut self,
        _: &OpenFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::OpenFile, window, cx);
    }

    pub(crate) fn handle_open_folder_menu_command(
        &mut self,
        _: &OpenFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::OpenFolder, window, cx);
    }

    pub(crate) fn handle_new_window_menu_command(
        &mut self,
        _: &NewWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::NewWindow, window, cx);
    }

    pub(crate) fn handle_close_window_menu_command(
        &mut self,
        _: &CloseWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::CloseWindow, window, cx);
    }

    pub(crate) fn handle_zoom_in_menu_command(
        &mut self,
        _: &ZoomIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::ZoomIn, window, cx);
    }

    pub(crate) fn handle_zoom_out_menu_command(
        &mut self,
        _: &ZoomOut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::ZoomOut, window, cx);
    }

    pub(crate) fn handle_reset_zoom_menu_command(
        &mut self,
        _: &ResetZoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::ResetZoom, window, cx);
    }

    pub(crate) fn handle_next_agent_menu_command(
        &mut self,
        _: &NextAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::NextAgent, window, cx);
    }

    pub(crate) fn handle_previous_agent_menu_command(
        &mut self,
        _: &PreviousAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::PreviousAgent, window, cx);
    }

    pub(crate) fn handle_archive_agent_menu_command(
        &mut self,
        _: &ArchiveAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::ArchiveAgent, window, cx);
    }

    pub(crate) fn handle_review_agent_menu_command(
        &mut self,
        _: &ReviewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::ReviewAgent, window, cx);
    }

    pub(crate) fn handle_keep_all_changes_menu_command(
        &mut self,
        _: &KeepAllChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::KeepAllChanges, window, cx);
    }

    pub(crate) fn handle_discard_worktree_menu_command(
        &mut self,
        _: &DiscardWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::DiscardWorktree, window, cx);
    }

    pub(crate) fn handle_open_documentation_menu_command(
        &mut self,
        _: &OpenDocumentation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::Documentation, window, cx);
    }

    pub(crate) fn handle_report_issue_menu_command(
        &mut self,
        _: &ReportIssue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::ReportIssue, window, cx);
    }

    pub(crate) fn handle_about_menu_command(
        &mut self,
        _: &About,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_menu_command(MenuCommand::About, window, cx);
    }
}

#[cfg(test)]
mod menu_command_tests {
    use super::*;
    use crate::test_support::open_test_app;
    use gpui::TestAppContext;

    #[gpui::test]
    fn menu_command_enabled_save_tracks_the_real_active_edit_buffer(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        assert!(
            !app.read_with(cx, |app, _| app.menu_command_enabled(MenuCommand::Save)),
            "Save must be disabled with nothing open"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.menu_command_enabled(MenuCommand::Save)),
            "Save must be enabled once a real file is open"
        );
    }

    #[gpui::test]
    fn menu_command_enabled_zoom_requires_a_real_code_surface(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        assert!(
            !app.read_with(cx, |app, _| app.menu_command_enabled(MenuCommand::ZoomIn)),
            "Zoom must be disabled with no code surface open"
        );
    }

    #[gpui::test]
    fn menu_command_enabled_archive_agent_tracks_the_real_active_agent(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let initial_id = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("premise: open_test_app must really start with one active agent");
        assert!(
            app.read_with(cx, |app, _| app
                .menu_command_enabled(MenuCommand::ArchiveAgent)),
            "Archive Agent must be enabled with a real active agent"
        );

        app.update_in(cx, |app, window, cx| {
            app.archive_agent(initial_id, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            None,
            "premise: archiving the only agent must really leave none active"
        );
        assert!(
            !app.read_with(cx, |app, _| app
                .menu_command_enabled(MenuCommand::ArchiveAgent)),
            "Archive Agent must be disabled once every real agent has been archived"
        );
    }

    #[gpui::test]
    fn dispatching_zoom_in_action_reaches_the_real_zoom_in_handler(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let file_path = repo.path().join("notes.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write notes.rs");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.menu_command_enabled(MenuCommand::ZoomIn)),
            "premise: a real code surface must be showing for Zoom In to be enabled"
        );
        let zoom_before = app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent);

        cx.dispatch_action(ZoomIn);
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent) > zoom_before,
            "dispatching the real ZoomIn action must really increase the zoom level"
        );
    }

    #[gpui::test]
    fn zoom_in_action_is_unavailable_with_no_code_surface_showing(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let _ = app;

        assert!(
            !cx.update(|window, cx| window.is_action_available(&ZoomIn, cx)),
            "ZoomIn must be unavailable with no code surface open, mirroring \
             menu_command_enabled(ZoomIn)"
        );
    }
}
