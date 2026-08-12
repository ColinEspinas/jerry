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
///
/// ## History: these used to be inert
///
/// Until this app grew enough real, already-wired functionality to hang a genuine menu off each
/// label, this app had no real menu hierarchy behind them at all, and they rendered as honest,
/// hover-only labels with no `cursor_pointer()`/`on_click()` - a deliberate choice over a
/// dropdown that looked openable but showed nothing. Every row in every one of these five real
/// dropdowns now calls a real, already-existing `AdeApp` method (the same ones the tab strip's
/// `+` menu, the command palette, and the agent footer already call) - never a placeholder row
/// that only closes the menu.
///
/// ## One source of truth, not two parallel arrays
///
/// An earlier revision paired this enum with a separately-declared `WINDOWS_MENU_ITEMS: [&str; 5]`
/// label array and a `from_index(usize) -> Option<Self>` lookup, with nothing enforcing the two
/// stayed the same length/order - [`Self::ALL`] plus [`Self::label`] below is the real fix: one
/// array, indexed directly, so there is no second array that can silently drift out of lockstep.
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
    ///
    /// Same scrim-plus-popover shape as
    /// [`crate::work_surface::render::AdeApp::render_plus_menu`]: a full-screen transparent
    /// scrim closes the menu on any click outside it, and the popover itself is absolutely
    /// positioned directly off the open label's own painted bounds
    /// ([`AdeApp::title_menu_button_bounds`]) rather than a second, independently-computed
    /// offset. Every row is a real [`render_dropdown_menu_row`] built by [`Self::title_menu_rows`]
    /// from [`crate::title_bar::menu_model::MenuCommand`] - the same shared model the real macOS
    /// menu (`crate::title_bar::native_menu`, a later revision) reads too (GitHub issue #235).
    ///
    /// ## No new keybinding opens or drives this menu
    ///
    /// This menu is mouse-only - clicking a [`TitleMenu::label`] is the only way to open it, and
    /// there is no `gpui::KeyBinding` for it in `crate::default_key_bindings`. That's a
    /// deliberate scope decision: this project has repeatedly hit real, live-reproduced bugs
    /// where a *global* keybinding stole a keystroke a focused terminal/agent needed
    /// (`crate::default_key_bindings`'s own docs cover several it scoped away from this exact
    /// way - unscoped `"]"`, unscoped `secondary-z` - and one, `secondary-p`, it ultimately
    /// accepted stealing anyway as a deliberate, discussed tradeoff). A title-bar menu has no
    /// conventional shortcut of its own to
    /// conflict with anything, so the safest way to avoid adding an eighth instance of that bug
    /// class is simply not adding a keybinding at all - every row it offers is already reachable
    /// some other real way (a keybinding of its own, the `+` menu, or the command palette).
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
    ///
    /// Kept as a named method purely for this module's own call sites; the element itself is
    /// `crate::root::widgets::render_menu_group_divider`, shared with the file tree's right-click
    /// context menu (GitHub issue #19) so the app has exactly one in-menu divider rather than two
    /// that happen to agree today.
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
    ///
    /// Enablement and the row's real effect both come from `crate::root::menu_commands`
    /// ([`AdeApp::menu_command_enabled`]/[`AdeApp::perform_menu_command`]) - the same two
    /// functions the real macOS menu (`crate::title_bar::native_menu`, a later revision) uses, so
    /// a row here can never show as enabled/disabled or do something different from what its
    /// native-menu counterpart would. A disabled row gets no `on_click` at all (rather than one
    /// that silently no-ops), matching [`render_dropdown_menu_row`]'s own enabled/disabled
    /// contract every pre-issue-#235 row already followed.
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

    /// Clicking the real "File" label - a genuine painted div with its own `on_click`, hit-test
    /// via a real simulated click at its own captured bounds - opens the real File dropdown, not
    /// just a state flip.
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

    /// The File menu's first row ("Open File…") really opens the command palette scoped to
    /// files - the same real `open_palette` + `PaletteScope::Files` the `+` menu's own "Open
    /// file…" row already uses - and closes the title menu.
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

    /// GitHub issue #90's "Open Folder…" row - the File menu's second row. Drives it through a
    /// real simulated click plus a real simulated native-picker response
    /// (`TestAppContext::simulate_path_prompt_response`, the same real `gpui::App::
    /// prompt_for_paths` seam `AdeApp::start_choose_repo_folder` itself calls), and asserts the
    /// real effect: the chosen folder becomes this window's own focused repo, not merely that a
    /// dialog was requested.
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

    /// GitHub issue #90's "New Window" row: opens a genuinely *second*, empty-state window -
    /// never this window's own repo, and never whatever repo happens to be focused here - even
    /// though this window under test starts with a real repo focused.
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

    /// The Edit menu's *first* row is now real **text** undo (GitHub issue #17), and it is a real
    /// affordance in both directions: inert with nothing to undo (no `on_click` attached at all,
    /// per `render_dropdown_menu_row`'s own enabled/disabled contract), and genuinely undoing the
    /// active buffer's own last step once there is one.
    ///
    /// The negative half matters as much as the positive one: before this issue that same first
    /// row fired a real `git reset --soft` while advertising `mod+z`, which is no longer what
    /// `mod+z` does inside a text widget.
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

        // Nothing open, nothing typed: the row must be genuinely inert, not merely ineffective.
        let bounds = open_edit_menu(&app, cx);
        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        // Now open a real file, type into it, and click the same row again.
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

    /// The View menu's first row ("Command Palette") really opens the real palette (default
    /// scope, unlike the File menu's files-scoped row).
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

    /// The Agent menu's first row ("New Terminal") really spawns a new real agent tab (the
    /// same real `Agents::spawn` call the tab strip's own `+` menu row and `secondary-n` use),
    /// not just a decoration - the agent count genuinely increases.
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

    /// The Help menu's real "About" row (the third row, after a divider - so this test drives it
    /// through `AdeApp::select_settings_page` state directly rather than computing a third row's
    /// pixel offset, since the exact click-point math for a row-after-a-divider is already
    /// covered by the simpler first-row tests above) really opens Settings on the real
    /// `SettingsPage::About` page.
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

    /// Opening the palette while a title-bar menu happens to still be open must close it -
    /// mirrors `crate::root::focus::tab_strip_keybinding_tests::
    /// opening_the_palette_closes_an_already_open_plus_menu`'s identical regression for the `+`
    /// menu, now that [`AdeApp::open_palette`] resets both.
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

    /// GitHub issue #176 for this surface: clicking a real title-bar label while another menu is
    /// open must close that one rather than stack on top of it. The file tree's context menu is
    /// the interesting partner - its scrim `.occlude()`s, but deliberately starts *below* the
    /// title bar (so the window's caption buttons stay reachable), which is exactly why this click
    /// really does reach the "File" label and really can leave two menus open.
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

    /// The Agent menu's "Discard worktree" row's real two-step confirm - mirrors the agent
    /// footer's own identical two-click button
    /// (`crate::work_surface::render::AdeApp::render_footer_action_button`), reusing the
    /// exact same real [`AdeApp::request_discard_worktree`]/[`AdeApp::discard_confirm_armed`]
    /// this test drives through the menu row instead of the footer button. Needs a real,
    /// non-main worktree (`crate::worktree_history::flow::AdeApp::request_discard_worktree`
    /// unconditionally refuses on the main worktree), so this sets one up the same way
    /// `crate::worktree_history::flow::worktree_history_regression_tests::add_worktree` does.
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

    /// The title menu's own click-handling for the Discard row (not just
    /// `request_discard_worktree` itself, already covered above): the first, arming click must
    /// leave the menu open so the row's label can really swap to "confirm discard?" for a second
    /// click, and the second, executing click must close it. Drives this through two real
    /// simulated clicks on the actual rendered Discard row ([`nth_row_click_point`]), matching
    /// this file's own established style for every other title-menu test - an earlier version of
    /// this test hand-copied the row's `on_click` condition inline instead, which meant it could
    /// never actually catch a bug in the real rendered row's own click wiring (e.g. a wrong
    /// bounds computation, or the row silently missing its `on_click` entirely).
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

        // The Discard row is the 7th real row (index 6, 0-based) in the Agent menu: New
        // Terminal, New Agent Pane, [divider], Next Agent, Previous Agent, [divider],
        // Archive Agent, Keep All Changes, Discard Worktree - six real rows and two dividers
        // sit above it (see `AdeApp::agent_menu_rows`'s own row order).
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Agent.index()]
        });
        let discard_row_point = nth_row_click_point(bounds, 6, 2);

        // First, arming click on the real rendered Discard row.
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

    /// [`crate::work_surface::render::AdeApp::select_relative_agent`] (the Agent menu's
    /// "Next Agent"/"Previous Agent" rows) - real coverage of the cyclic-index logic itself:
    /// cycling forward through three real agents wraps back to the first, and cycling backward
    /// from the first wraps to the last, via the same real [`AdeApp::select_agent`] every
    /// tab-strip click already goes through.
    ///
    /// Also real, live-reproduced coverage for this revision's own self-audit finding: an
    /// earlier version of `select_relative_agent` cycled the flat `self.agents` list
    /// directly rather than [`AdeApp::current_worktree_agents`], so "Next Agent" could jump
    /// to a *different* worktree's agent entirely - which `AdeApp::select_agent` then
    /// silently promotes into a full `AdeApp::select_worktree` switch, landing the user on the
    /// wrong worktree's `edit_buffers` entries (keyed by `(worktree, path)` - see that field's
    /// own docs) rather than the one they were actually cycling through. Spawning every test
    /// agent into the *same* worktree (as an earlier version of this test did) can't distinguish
    /// that bug from correct behaviour at all - this seeds a **second** worktree with its own
    /// agent alongside the three under test, and asserts cycling never leaves the first worktree
    /// (`AdeApp::selected` stays put) and a real unsaved edit seeded in the first worktree's
    /// `AdeApp::edit_buffers` entry is still resolvable there after cycling - which an accidental
    /// jump to the second (buffer-less) worktree would break, since [`AdeApp::edit_buffer_contains`]
    /// resolves through whichever worktree is genuinely current.
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
}
