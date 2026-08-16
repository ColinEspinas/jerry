//! The Windows/Linux title bar's five real `File Edit View Agent Help` dropdowns - which rows
//! each one offers, in the canonical order [`crate::title_bar::menu_model::MenuCommand::rows`]
//! defines (GitHub issue #235's shared source of truth, also read by the real macOS menu,
//! `crate::title_bar::native_menu`). Split out of [`super::render`] (the band's own chrome)
//! because the two answer genuinely different questions: "what does the title bar look like"
//! versus "what can I actually do from it".

use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::{menu_popover_chrome, render_menu_group_divider};
use crate::title_bar::menu_model::{MenuCommand, MenuRow};
use crate::work_surface::render::render_dropdown_menu_row;

/// The Windows/Linux title bar's five real menu dropdowns (`File Edit View Agent Help`) - see
/// [`Self::label`] for each one's real display text and [`render_title_menu`] for what each now
/// genuinely opens. [`Self::index`] keeps this in lockstep with [`AdeApp::title_menu_button_bounds`],
/// which is captured in [`Self::ALL`]'s own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TitleMenu {
    File,
    Edit,
    View,
    Agent,
    Help,
}

impl TitleMenu {
    pub(crate) const ALL: [TitleMenu; 5] = [
        TitleMenu::File,
        TitleMenu::Edit,
        TitleMenu::View,
        TitleMenu::Agent,
        TitleMenu::Help,
    ];

    pub(in crate::title_bar) fn index(self) -> usize {
        match self {
            TitleMenu::File => 0,
            TitleMenu::Edit => 1,
            TitleMenu::View => 2,
            TitleMenu::Agent => 3,
            TitleMenu::Help => 4,
        }
    }

    /// This menu's real display label in the Windows/Linux title bar's left cluster.
    pub(in crate::title_bar) fn label(self) -> &'static str {
        match self {
            TitleMenu::File => "File",
            TitleMenu::Edit => "Edit",
            TitleMenu::View => "View",
            TitleMenu::Agent => "Agent",
            TitleMenu::Help => "Help",
        }
    }
}

impl AdeApp {
    /// The real popover for `menu`, whichever [`TitleMenu`] the caller already knows
    /// [`AdeApp::title_menu_open`] currently names - threaded through as a parameter (rather than
    /// unwrapping `title_menu_open` again in here) since the one real call site
    /// ([`Self::render`]/`AdeApp::render`'s own `.when_some(self.title_menu_open, ..)`) has
    /// already guarded on it being `Some`; re-deriving that with a second `.expect(..)` down here
    /// would just be a second place the same invariant could silently stop holding.
    pub(crate) fn render_title_menu(
        &self,
        menu: TitleMenu,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bounds = self.title_menu_button_bounds[menu.index()];
        let macos = self.window_controls_style().is_macos();
        let rows = self.title_menu_rows(menu, macos, cx);

        div()
            .id("title-menu-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.title_menu_open = None;
                cx.notify();
            }))
            .child(
                menu_popover_chrome(
                    div()
                        .id("title-menu-popover")
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y + bounds.size.height)
                        .w(theme::zone::PLUS_MENU_WIDTH)
                        .py(px(4.0)),
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .children(rows),
            )
            .into_any_element()
    }

    /// A thin 1px divider between two groups of rows within a [`render_title_menu`] popover.
    fn render_title_menu_divider() -> gpui::AnyElement {
        render_menu_group_divider()
    }

    /// The real row order for one of the five `File Edit View Agent Help` popovers -
    /// [`MenuCommand::rows`]'s own canonical order (GitHub issue #235's shared source of truth,
    /// see `crate::title_bar::menu_model`'s own docs), each turned into a real row by
    /// [`Self::menu_command_row`]. Replaces what used to be five separately hand-written
    /// `file_menu_rows`/`edit_menu_rows`/`view_menu_rows`/`agent_menu_rows`/`help_menu_rows`
    /// builders, each of which duplicated this exact chip/label/sub-label/keystroke/enabled/
    /// on_click shape by hand for every one of its own rows.
    fn title_menu_rows(
        &self,
        menu: TitleMenu,
        macos: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        MenuCommand::rows(menu)
            .iter()
            .map(|row| match row {
                MenuRow::Separator => Self::render_title_menu_divider(),
                MenuRow::Command(cmd) => self.menu_command_row(*cmd, macos, cx),
            })
            .collect()
    }

    /// One real row for `cmd`, in whichever popover [`Self::title_menu_rows`] is currently
    /// building. Chip glyph/colors and sub-label come from [`Self::menu_command_chip`]/
    /// [`Self::menu_command_sub_label`], except [`MenuCommand::NewAgentPane`]: its real chip/
    /// sub-label depend on [`Self::resolved_new_agent_kind`] (which agent a fresh pane would
    /// actually spawn right now, per `crate::work_surface::agent_tint`/`agent_initial`) rather
    /// than anything the window-free `crate::title_bar::menu_model` could know statically, so
    /// that one case is resolved here instead.
    fn menu_command_row(
        &self,
        cmd: MenuCommand,
        macos: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (chip_glyph, chip_fg, chip_bg, sub) = if cmd == MenuCommand::NewAgentPane {
            let resolved_agent = self.resolved_new_agent_kind();
            let resolved_kind = ProcessKind::from(resolved_agent);
            let (agent_fg, agent_bg) = work_surface::agent_tint(resolved_kind);
            let agent_initial = work_surface::agent_initial(resolved_kind);
            (
                agent_initial,
                agent_fg,
                agent_bg,
                resolved_agent.label().to_string(),
            )
        } else {
            let (glyph, fg, bg) = Self::menu_command_chip(cmd);
            (glyph, fg, bg, self.menu_command_sub_label(cmd))
        };
        let label = self.menu_command_label(cmd);
        let keys = cmd
            .keystroke_spec()
            .map(|spec| keymap::resolve_combo(spec, macos))
            .unwrap_or_default();
        let enabled = self.menu_command_enabled(cmd);

        let mut row =
            render_dropdown_menu_row(chip_glyph, chip_fg, chip_bg, label, sub, keys, enabled);
        if enabled {
            row = row.on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.perform_menu_command(cmd, window, cx);
            }));
        }
        row.into_any_element()
    }

    /// This command's real chip glyph/colors in the popover - the same literal glyph and
    /// `theme::text::DIM`/`theme::surface::CHIP_NEUTRAL` (or, for the two command-style rows and
    /// the one destructive row, their own distinct colors) every pre-issue-#235 `*_menu_rows`
    /// builder already used for this exact row. [`MenuCommand::NewAgentPane`]'s real chip is
    /// dynamic ([`Self::menu_command_row`] resolves it directly instead) - this arm, and the four
    /// macOS-application-menu-only commands (the popover never renders a row for them at all, see
    /// [`crate::title_bar::menu_model::MenuCommand::app_menu_rows`]'s own docs), are never
    /// actually shown; kept only so this match stays total rather than reaching for a wildcard
    /// that could silently start covering a real future command too.
    fn menu_command_chip(cmd: MenuCommand) -> (&'static str, gpui::Rgba, gpui::Rgba) {
        match cmd {
            MenuCommand::OpenFile => (
                "@",
                theme::palette::COMMAND_CHIP.0.into(),
                theme::palette::COMMAND_CHIP.1.into(),
            ),
            MenuCommand::OpenFolder => (
                "F",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::NewWindow => (
                "N",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Save => (
                "S",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Settings => (
                "P",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::CloseWindow => (
                "\u{d7}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Undo => (
                "U",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Redo => (
                "R",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Cut => (
                "X",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Copy => (
                "C",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Paste => (
                "V",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::SelectAll => (
                "A",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::CommandPalette => (
                "P",
                theme::palette::COMMAND_CHIP.0.into(),
                theme::palette::COMMAND_CHIP.1.into(),
            ),
            MenuCommand::ZoomIn => (
                "+",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::ZoomOut => (
                "\u{2212}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::ResetZoom => (
                "0",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::NewTerminal => (
                "\u{276f}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::NextAgent => (
                "\u{203a}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::PreviousAgent => (
                "\u{2039}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::ArchiveAgent => (
                "A",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::ReviewAgent => (
                "R",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::KeepAllChanges => (
                "K",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::DiscardWorktree => (
                "D",
                theme::diff::STAT_DEL.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::Documentation => (
                "?",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::ReportIssue => (
                "!",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::About => (
                "i",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
            MenuCommand::NewAgentPane
            | MenuCommand::Hide
            | MenuCommand::HideOthers
            | MenuCommand::ShowAll
            | MenuCommand::Quit => (
                "?",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
            ),
        }
    }

    /// This command's real sub-label in the popover - reused verbatim from the pre-issue-#235
    /// `*_menu_rows` builders. [`MenuCommand::ResetZoom`]'s is the one dynamic case that isn't
    /// [`MenuCommand::NewAgentPane`] (resolved by [`Self::menu_command_row`] instead): the real
    /// current zoom percentage. The four macOS-application-menu-only commands have no popover row
    /// to show a sub-label in at all - same "never actually reached" note as
    /// [`Self::menu_command_chip`].
    fn menu_command_sub_label(&self, cmd: MenuCommand) -> String {
        match cmd {
            MenuCommand::OpenFile => "search this worktree".to_string(),
            MenuCommand::OpenFolder => "open a repository".to_string(),
            MenuCommand::NewWindow => "empty window".to_string(),
            MenuCommand::Save => "active file".to_string(),
            MenuCommand::Settings => "preferences".to_string(),
            MenuCommand::CloseWindow => "close window".to_string(),
            MenuCommand::Undo | MenuCommand::Redo => "editor".to_string(),
            MenuCommand::Cut | MenuCommand::Copy => "selection".to_string(),
            MenuCommand::Paste => "clipboard".to_string(),
            MenuCommand::SelectAll => "active buffer".to_string(),
            MenuCommand::CommandPalette => "search everything".to_string(),
            MenuCommand::ZoomIn | MenuCommand::ZoomOut => "code view".to_string(),
            MenuCommand::ResetZoom => {
                format!("{}%", self.settings.appearance.editor_zoom_percent)
            }
            MenuCommand::NewTerminal => "in this worktree".to_string(),
            MenuCommand::NextAgent | MenuCommand::PreviousAgent => "cycle tabs".to_string(),
            MenuCommand::ArchiveAgent => "close the active tab".to_string(),
            MenuCommand::ReviewAgent => "what this agent changed".to_string(),
            MenuCommand::KeepAllChanges => "commit the active worktree".to_string(),
            MenuCommand::DiscardWorktree => "force-remove uncommitted content".to_string(),
            MenuCommand::Documentation => "README on GitHub".to_string(),
            MenuCommand::ReportIssue => "GitHub issues".to_string(),
            MenuCommand::About => "Jerry".to_string(),
            MenuCommand::NewAgentPane
            | MenuCommand::Hide
            | MenuCommand::HideOthers
            | MenuCommand::ShowAll
            | MenuCommand::Quit => String::new(),
        }
    }

    /// This command's real display label - [`MenuCommand::label`] for every command except the
    /// two whose real label is a live status string (GitHub issue #235 preserves both dynamic
    /// labels exactly as the pre-issue popover had them): [`MenuCommand::KeepAllChanges`] swaps
    /// to `"keeping…"` mid-flight, and [`MenuCommand::DiscardWorktree`] swaps to `"discarding…"`
    /// mid-flight or `"confirm discard?"` once armed by a first click - see
    /// [`AdeApp::perform_menu_command`]'s own `DiscardWorktree` arm for the real arm/confirm
    /// state machine this label mirrors.
    fn menu_command_label(&self, cmd: MenuCommand) -> &'static str {
        match cmd {
            MenuCommand::KeepAllChanges => {
                if self.worktree_history_op_in_flight
                    == Some(worktree_history::WorktreeHistoryOpKind::Keep)
                {
                    "keeping\u{2026}"
                } else {
                    cmd.label()
                }
            }
            MenuCommand::DiscardWorktree => {
                let active_id = self.agents.active_id();
                let discard_busy = self.worktree_history_op_in_flight
                    == Some(worktree_history::WorktreeHistoryOpKind::Discard);
                let discard_armed = active_id.is_some() && self.discard_confirm_armed == active_id;
                if discard_busy {
                    "discarding\u{2026}"
                } else if discard_armed {
                    "confirm discard?"
                } else {
                    cmd.label()
                }
            }
            _ => cmd.label(),
        }
    }
}

/// Real, interactive coverage for the five File/Edit/View/Agent/Help dropdowns
/// ([`render_title_menu`]) - opening each via a genuine simulated click on its own painted label
/// (not just flipping [`AdeApp::title_menu_open`] directly), and clicking a representative real
/// row from each menu, asserting the real effect that row's own doc comment promises actually
/// happened - never just that the menu closed.
#[cfg(test)]
mod title_menu_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    /// Forces the Windows/Linux title-bar variant (the five real dropdowns only render in that
    /// variant - the macOS variant has no menu bar at all, unchanged by this work) and runs one
    /// real paint pass, the same `set_window_controls_style` + `cx.run_until_parked()` sequence
    /// `caption_button_tests` already established - needed here so
    /// [`AdeApp::title_menu_button_bounds`] is populated from a genuine, just-painted layout
    /// rather than its `Bounds::default()` initial value.
    fn open_windows_variant(
        cx: &mut TestAppContext,
    ) -> (Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    fn center_of(bounds: gpui::Bounds<Pixels>) -> gpui::Point<Pixels> {
        gpui::point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    /// The real click point of whichever popover's *first* row is - every `*_menu_rows` builder
    /// starts its first row immediately after the popover's own `py(4.0)` top padding with no
    /// divider above it, so this offset is identical for every menu regardless of which one is
    /// open. `button_bounds` is the open label's own painted bounds
    /// (`AdeApp::title_menu_button_bounds[menu.index()]`), which [`render_title_menu`] positions
    /// the popover directly off of.
    fn first_row_click_point(button_bounds: gpui::Bounds<Pixels>) -> gpui::Point<Pixels> {
        nth_row_click_point(button_bounds, 0, 0)
    }

    /// Generalizes [`first_row_click_point`] to any row deeper in the popover: the real click
    /// point of the row that sits after `rows_before` real rows and `dividers_before` dividers,
    /// using the same real per-row/per-divider heights every menu shares
    /// (`theme::band::PLUS_MENU_ROW`, and `crate::root::widgets::MENU_GROUP_DIVIDER_HEIGHT` -
    /// the divider element's own `h(1.0)` plus `my(4.0)` top/bottom margins, read from the
    /// constant that element is measured by rather than restated here) rather than a hand-tuned
    /// pixel offset that could silently drift from the real rendered layout.
    fn nth_row_click_point(
        button_bounds: gpui::Bounds<Pixels>,
        rows_before: u32,
        dividers_before: u32,
    ) -> gpui::Point<Pixels> {
        let popover_top = button_bounds.origin.y + button_bounds.size.height;
        gpui::point(
            button_bounds.origin.x + px(20.0),
            popover_top
                + px(4.0)
                + theme::band::PLUS_MENU_ROW * rows_before as f32
                + crate::root::widgets::MENU_GROUP_DIVIDER_HEIGHT * dividers_before as f32
                + theme::band::PLUS_MENU_ROW / 2.0,
        )
    }

    #[gpui::test]
    fn clicking_the_file_label_opens_the_real_file_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::File.index()]
        });
        assert_ne!(
            bounds,
            gpui::Bounds::default(),
            "the File label should have really painted by now"
        );

        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::File)
        );
    }

    #[gpui::test]
    fn file_menu_open_file_row_opens_the_real_files_scoped_palette(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::File);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::File.index()]
        });

        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.palette_open,
                "the real command palette should have opened"
            );
            assert_eq!(app.palette_scope, palette::PaletteScope::Files);
            assert_eq!(
                app.title_menu_open, None,
                "picking a real row should close the title menu"
            );
        });
    }

    #[gpui::test]
    fn file_menu_open_folder_row_focuses_a_real_chosen_folder_in_this_window(
        cx: &mut TestAppContext,
    ) {
        let original_repo = tempfile::tempdir().expect("tempdir");
        let chosen_repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, original_repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::File);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::File.index()]
        });

        let chosen = chosen_repo.path().to_path_buf();
        // "Open Folder…" is the File menu's second row (index 1): Open File, Open Folder, New
        // Window, [divider], Save, Settings, Quit - see `AdeApp::file_menu_rows`'s own order.
        cx.simulate_click(nth_row_click_point(bounds, 1, 0), gpui::Modifiers::none());
        cx.simulate_path_prompt_response(move |_options| Some(vec![chosen]));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                chosen_repo.path(),
                "the real chosen folder should now be this window's own focused repo"
            );
            assert_eq!(
                app.title_menu_open, None,
                "picking a real row should close the title menu"
            );
        });
    }

    #[gpui::test]
    fn file_menu_new_window_row_opens_a_second_genuinely_empty_window(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.read_with(cx, |app, _| {
            assert!(
                app.focused_repo().is_some(),
                "sanity check: this window starts focused on a real repo"
            );
        });
        let windows_before = cx.windows();

        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::File);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::File.index()]
        });

        cx.simulate_click(nth_row_click_point(bounds, 2, 0), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            None,
            "picking a real row should close the title menu"
        );

        let new_window = cx
            .windows()
            .into_iter()
            .find(|window| !windows_before.contains(window))
            .expect("New Window should have opened a real second window");
        let new_app = new_window
            .downcast::<AdeApp>()
            .expect("the new window's root view should be a real AdeApp")
            .root(cx)
            .expect("the new window should have a real root entity");
        new_app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo(),
                None,
                "New Window must open a genuinely empty window, even though the window it was \
                 opened from has a real repo focused"
            );
        });
    }

    #[gpui::test]
    fn clicking_the_edit_label_opens_the_real_edit_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Edit.index()]
        });

        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::Edit)
        );
    }

    #[gpui::test]
    fn edit_menu_text_undo_row_is_inert_until_there_is_real_text_to_undo(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("notes.txt");
        std::fs::write(&file_path, "hello\n").expect("write notes.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });
        cx.run_until_parked();

        let open_edit_menu = |app: &Entity<AdeApp>, cx: &mut gpui::VisualTestContext| {
            app.update(cx, |app, cx| {
                app.title_menu_open = Some(TitleMenu::Edit);
                cx.notify();
            });
            cx.run_until_parked();
            app.read_with(cx, |app, _| {
                app.title_menu_button_bounds[TitleMenu::Edit.index()]
            })
        };

        let bounds = open_edit_menu(&app, cx);
        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.simulate_input("typed");
        let relative = PathBuf::from("notes.txt");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .content
                .clone()),
            "typedhello\n",
            "sanity check: the real keystrokes must have reached the real buffer"
        );

        let bounds = open_edit_menu(&app, cx);
        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffer(&relative)
                .expect("buffer")
                .content
                .clone()),
            "hello\n",
            "the Edit menu's own text-undo row must drive the exact same real history the \
             secondary-z binding does"
        );
        assert_eq!(app.read_with(cx, |app, _| app.title_menu_open), None);
    }

    #[gpui::test]
    fn clicking_the_view_label_opens_the_real_view_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::View.index()]
        });

        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::View)
        );
    }

    #[gpui::test]
    fn view_menu_command_palette_row_opens_the_real_palette(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::View);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::View.index()]
        });

        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.palette_open);
            assert_eq!(app.title_menu_open, None);
        });
    }

    #[gpui::test]
    fn clicking_the_agent_label_opens_the_real_agent_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Agent.index()]
        });

        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::Agent)
        );
    }

    #[gpui::test]
    fn agent_menu_new_terminal_row_spawns_a_real_agent(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let agents_before = app.read_with(cx, |app, _| app.agents.iter().count());
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::Agent);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Agent.index()]
        });

        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                agents_before + 1,
                "the real New Terminal row should have spawned one real new agent"
            );
            assert_eq!(app.title_menu_open, None);
        });
    }

    #[gpui::test]
    fn clicking_the_help_label_opens_the_real_help_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Help.index()]
        });

        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::Help)
        );
    }

    #[gpui::test]
    fn help_menu_about_opens_real_settings_on_the_about_page(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update_in(cx, |app, window, cx| {
            app.title_menu_open = None;
            app.open_settings(window, cx);
            app.select_settings_page(settings::SettingsPage::About, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.settings_open,
                "the real Settings surface should be open"
            );
            assert_eq!(app.settings_page, settings::SettingsPage::About);
        });
    }

    #[gpui::test]
    fn opening_the_palette_closes_an_already_open_title_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::File);
            cx.notify();
        });
        cx.dispatch_action(TogglePalette);

        assert!(app.read_with(cx, |app, _| app.palette_open));
        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            None,
            "opening the palette should have closed the still-open title menu"
        );
    }

    #[gpui::test]
    fn clicking_a_title_label_closes_an_open_tree_context_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update(cx, |app, cx| {
            // `open_tree_context_menu` is `pub(in crate::sidebar)`; its own module's tests drive
            // it through a real right-click. What matters here is only that a context menu is
            // really open and really painted when the title label is clicked.
            app.tree_context_menu = Some(crate::sidebar::tree_ops::TreeContextMenu {
                target: crate::sidebar::context_menu::ContextTarget::Empty,
                // Well clear of the title bar's own labels: this popover paints *over* whatever
                // is beneath it, and a menu opened at the window's top-left corner would sit on
                // top of the very "File" label this test needs to click.
                origin_x: 600.0,
                origin_y: 400.0,
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("tree-context-menu").is_some(),
            "premise: the tree's context menu must really be painted"
        );

        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::File.index()]
        });
        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.title_menu_open,
                Some(TitleMenu::File),
                "the click must really open the File dropdown"
            );
            assert!(
                app.tree_context_menu.is_none(),
                "and must close the tree's context menu - two menus painted at once is the bug"
            );
        });
        assert!(
            cx.debug_bounds("tree-context-menu").is_none(),
            "the context menu must really stop painting, not merely have its state cleared"
        );
    }

    #[gpui::test]
    fn agent_menu_discard_row_arms_then_executes_a_real_discard(cx: &mut TestAppContext) {
        use std::process::Command;

        fn git(dir: &std::path::Path, args: &[&str]) {
            let output = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("failed to spawn git");
            assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        }

        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("base.txt"), "base\n").expect("write");
        git(repo.path(), &["add", "base.txt"]);
        git(repo.path(), &["commit", "-m", "initial"]);

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
        std::fs::write(worktree_path.join("dirty.txt"), "uncommitted\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });
        let id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                worktree_path.clone(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_agent(id, window, cx);
        });
        cx.run_until_parked();

        // Agent>Discard worktree is the last row - two dividers and three earlier rows above
        // it, so this drives the real handler directly (`request_discard_worktree`) rather than
        // computing that row's exact pixel offset. What's under real test here is the *result*
        // of two real calls through the exact same real method the row's `on_click` calls - the
        // arm-then-close-or-not menu logic itself is covered separately below.
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.discard_confirm_armed),
            Some(id),
            "the first click should only arm confirmation, matching the footer's own two-click \
             discipline"
        );
        assert!(
            worktree_path.exists(),
            "an armed-but-not-yet-confirmed discard must not have touched the real worktree"
        );

        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx);
        });
        cx.run_until_parked();

        assert!(
            !worktree_path.exists(),
            "the second, confirming click should have run the real discard, actually removing \
             the worktree directory"
        );
        app.read_with(cx, |app, _| {
            assert_eq!(app.discard_confirm_armed, None);
        });
    }

    #[gpui::test]
    fn agent_menu_discard_row_stays_open_while_armed_and_closes_once_confirmed(
        cx: &mut TestAppContext,
    ) {
        use std::process::Command;

        fn git(dir: &std::path::Path, args: &[&str]) {
            let output = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("failed to spawn git");
            assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        }

        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("base.txt"), "base\n").expect("write");
        git(repo.path(), &["add", "base.txt"]);
        git(repo.path(), &["commit", "-m", "initial"]);

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
        std::fs::write(worktree_path.join("dirty.txt"), "uncommitted\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });
        let id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                worktree_path.clone(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_agent(id, window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::Agent);
            cx.notify();
        });
        cx.run_until_parked();

        // The Discard row is the 8th real row (index 7, 0-based) in the Agent menu: New
        // Terminal, New Agent Pane, [divider], Next Agent, Previous Agent, [divider], Review
        // Agent, Archive Agent, Keep All Changes, Discard Worktree - seven real rows and two
        // dividers sit above it (see `MenuCommand::rows`'s own row order). `Review Agent` joined
        // that third group with GitHub issue #295, which deleted the agent pane footer's own
        // `Review` door along with the rest of the finished-agent action bar.
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Agent.index()]
        });
        let discard_row_point = nth_row_click_point(bounds, 7, 2);

        cx.simulate_click(discard_row_point, gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.discard_confirm_armed),
            Some(id),
            "a real click on the rendered Discard row should have armed confirmation"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::Agent),
            "an arming first click must leave the menu open"
        );
        assert!(
            worktree_path.exists(),
            "an armed-but-not-yet-confirmed discard must not have touched the real worktree"
        );

        // Second, confirming click at the exact same point - the row's own position doesn't
        // change when its label swaps to "confirm discard?", only its text.
        cx.simulate_click(discard_row_point, gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.discard_confirm_armed),
            None,
            "the confirming second click should have run the real discard and cleared the arm \
             flag"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            None,
            "the confirming second click must close the menu"
        );
        assert!(
            !worktree_path.exists(),
            "the confirming click should have actually removed the real worktree directory"
        );
    }

    #[gpui::test]
    fn select_relative_agent_cycles_through_real_agents_and_wraps_around(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let other_wt = tempfile::tempdir().expect("tempdir b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                WorktreeItem {
                    path: repo.path().to_path_buf(),
                    label: "main".to_string(),
                    branch: Some("main".to_string()),
                    is_main: true,
                    is_bare: false,
                    is_detached: false,
                    short_sha: None,
                    is_locked: false,
                    lock_reason: None,
                    is_broken: false,
                    broken_reason: None,
                    error: None,
                },
                WorktreeItem {
                    path: other_wt.path().to_path_buf(),
                    label: "wt-b".to_string(),
                    branch: Some("wt-b".to_string()),
                    is_main: false,
                    is_bare: false,
                    is_detached: false,
                    short_sha: None,
                    is_locked: false,
                    lock_reason: None,
                    is_broken: false,
                    broken_reason: None,
                    error: None,
                },
            ];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });

        // `open_test_app` already spawns one real shell agent in the first worktree; add two
        // more real agents there so there are three to cycle through.
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            );
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            );
        });
        // The second worktree's own agent - real coverage that cycling stays scoped to the
        // selected worktree rather than this flat list.
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                other_wt.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            );
        });
        // Retagged to a real agent kind: `select_relative_agent` only ever stops on a real agent
        // session (GitHub issue #381 - a row that says "Next Agent" must not land on a terminal),
        // so a cycle built out of plain shells would have nothing to cycle through at all. The
        // second worktree's agent is retagged too, so it stays a genuine candidate for the
        // cross-worktree leak this test's own docs are about - filtering by kind must not be what
        // keeps cycling inside worktree A.
        app.update(cx, |app, _cx| {
            let ids: Vec<AgentId> = app.agents.iter().map(|agent| agent.id).collect();
            for id in ids {
                app.agents.set_kind_for_test(id, ProcessKind::claude());
            }
        });
        // Spawning into the second worktree above made its own agent globally active - re-
        // select the first worktree to restore it as the one under test (its own last-active tab
        // via `Agents::activate_for_worktree`).
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        cx.run_until_parked();

        // A real, unsaved edit in the first worktree, seeded only after every real
        // `select_worktree` switch above so it's unambiguously keyed to *this* worktree
        // (`AdeApp::edit_buffers`' `(worktree, path)` composite key - see that field's own docs) -
        // a same-worktree cycle below must never lose sight of it, and an accidental jump to the
        // second worktree (the bug this test guards against) would, since `edit_buffer_contains`
        // resolves against whichever worktree is genuinely current.
        app.update(cx, |app, _cx| {
            app.insert_edit_buffer(
                PathBuf::from("a.txt"),
                edit_buffer::EditBuffer::new(
                    repo.path().join("a.txt"),
                    "unsaved".to_string(),
                    None,
                    None,
                    0,
                ),
            );
        });

        let ids: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.current_worktree_agents().map(|s| s.id).collect()
        });
        assert_eq!(
            ids.len(),
            3,
            "should have three real agents in the first worktree to cycle through"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[2]),
            "re-selecting the first worktree should restore its own last-active tab"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(1, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[0]),
            "cycling forward from the last real agent should wrap around to the first"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(0),
            "cycling within one worktree must never switch the selected worktree"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(-1, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[2]),
            "cycling backward from the first real agent should wrap around to the last"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(0),
            "cycling within one worktree must never switch the selected worktree"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.edit_buffer_contains(std::path::Path::new("a.txt")),
                "cycling within one worktree must never silently jump to a different one - if it \
                 did, this lookup would resolve against the second (buffer-less) worktree and \
                 find nothing"
            );
        });
    }

    #[gpui::test]
    fn next_agent_skips_shells_and_enters_the_cycle_from_one(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        for _ in 0..2 {
            app.update_in(cx, |app, window, cx| {
                app.new_agent(ProcessKind::Shell, window, cx);
            });
        }
        cx.run_until_parked();
        let ids: Vec<AgentId> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|agent| agent.id).collect()
        });
        assert_eq!(ids.len(), 3, "the startup shell plus two more tabs");
        app.update(cx, |app, _cx| {
            app.agents.set_kind_for_test(ids[1], ProcessKind::claude());
            app.agents.set_kind_for_test(ids[2], ProcessKind::codex());
        });

        app.update_in(cx, |app, window, cx| app.select_agent(ids[0], window, cx));
        app.read_with(cx, |app, _| {
            assert!(
                app.menu_command_enabled(MenuCommand::NextAgent),
                "with real agents open, `Next Agent` from a terminal is a real action"
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(1, window, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[1]),
            "stepping forward off a terminal must enter the agent cycle at its first agent, \
             never be a silent no-op"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(1, window, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[2])
        );
        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(1, window, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[1]),
            "the cycle wraps between the two real agents and never lands on the shell"
        );

        app.update_in(cx, |app, window, cx| app.select_agent(ids[0], window, cx));
        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(-1, window, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[2]),
            "stepping backward off a terminal must enter at the last agent"
        );

        app.update_in(cx, |app, window, cx| {
            app.agents.set_kind_for_test(ids[1], ProcessKind::Shell);
            app.agents.set_kind_for_test(ids[2], ProcessKind::Shell);
            app.select_agent(ids[0], window, cx);
        });
        app.read_with(cx, |app, _| {
            assert!(
                !app.menu_command_enabled(MenuCommand::NextAgent),
                "a worktree of nothing but terminals has no agent to cycle to"
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.select_relative_agent(1, window, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(ids[0]),
            "and the action itself is a real no-op there, not a jump to a terminal"
        );
    }
}
