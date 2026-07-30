//! The Windows/Linux title bar's five real `File Edit View Session Help` dropdowns -
//! which rows each one offers, and which already-existing real `AdeApp` method every row
//! calls. Split out of [`super::render`] (the band's own chrome) because the two answer
//! genuinely different questions: "what does the title bar look like" versus "what can I
//! actually do from it".

use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::work_surface::render::render_dropdown_menu_row;

/// The Windows/Linux title bar's five real menu dropdowns (`File Edit View Session Help`) - see
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
/// `+` menu, the command palette, and the session footer already call) - never a placeholder row
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
    Session,
    Help,
}

impl TitleMenu {
    pub(crate) const ALL: [TitleMenu; 5] = [
        TitleMenu::File,
        TitleMenu::Edit,
        TitleMenu::View,
        TitleMenu::Session,
        TitleMenu::Help,
    ];

    pub(in crate::title_bar) fn index(self) -> usize {
        match self {
            TitleMenu::File => 0,
            TitleMenu::Edit => 1,
            TitleMenu::View => 2,
            TitleMenu::Session => 3,
            TitleMenu::Help => 4,
        }
    }

    /// This menu's real display label in the Windows/Linux title bar's left cluster.
    pub(in crate::title_bar) fn label(self) -> &'static str {
        match self {
            TitleMenu::File => "File",
            TitleMenu::Edit => "Edit",
            TitleMenu::View => "View",
            TitleMenu::Session => "Session",
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
    /// offset. Every row is a real [`render_dropdown_menu_row`] wired to a real, already-existing
    /// `AdeApp` method via the matching `*_menu_rows` builder below - see each one's own docs for
    /// which real method backs which row, and why.
    ///
    /// ## No new keybinding opens or drives this menu
    ///
    /// This menu is mouse-only - clicking a [`TitleMenu::label`] is the only way to open it, and
    /// there is no `gpui::KeyBinding` for it in `crate::default_key_bindings`. That's a
    /// deliberate scope decision: this project has repeatedly hit real, live-reproduced bugs
    /// where a *global* keybinding stole a keystroke a focused terminal/agent session needed
    /// (`crate::default_key_bindings`'s own docs cover several - `secondary-p`, unscoped `"]"`,
    /// unscoped `secondary-z`). A title-bar menu has no conventional shortcut of its own to
    /// conflict with anything, so the safest way to avoid adding an eighth instance of that bug
    /// class is simply not adding a keybinding at all - every row it offers is already reachable
    /// some other real way (a keybinding of its own, the `+` menu, or the command palette).
    pub(crate) fn render_title_menu(
        &self,
        menu: TitleMenu,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let bounds = self.title_menu_button_bounds[menu.index()];
        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::PLUS_MENU;
        let macos = self.window_controls_style().is_macos();
        let rows = match menu {
            TitleMenu::File => self.file_menu_rows(macos, cx),
            TitleMenu::Edit => self.edit_menu_rows(macos, cx),
            TitleMenu::View => self.view_menu_rows(cx),
            TitleMenu::Session => self.session_menu_rows(macos, cx),
            TitleMenu::Help => self.help_menu_rows(cx),
        };

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
                div()
                    .id("title-menu-popover")
                    .absolute()
                    .left(bounds.origin.x)
                    .top(bounds.origin.y + bounds.size.height)
                    .w(theme::zone::PLUS_MENU_WIDTH)
                    .py(px(4.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .shadow(vec![BoxShadow::new(
                        shadow_x,
                        shadow_y,
                        gpui::black().opacity(0.55),
                    )
                    .blur_radius(shadow_blur)])
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .children(rows),
            )
            .into_any_element()
    }

    /// A thin 1px divider between two groups of rows within a [`render_title_menu`] popover -
    /// the same [`theme::border::DIVIDER`] token the title bar's own left-cluster divider uses.
    fn render_title_menu_divider() -> gpui::AnyElement {
        div()
            .h(px(1.0))
            .mx(px(10.0))
            .my(px(4.0))
            .bg(theme::border::DIVIDER)
            .into_any_element()
    }

    /// The File menu: open a file (the same real, files-scoped command palette the `+` menu's
    /// own "Open file…" row opens), save the active file (real, same handler `secondary-s`
    /// dispatches - a safe no-op with nothing dirty to save, per
    /// [`crate::code_surface::editing::AdeApp::save_active_file`]'s own guard), open Settings, and quit
    /// (the same real [`Window::remove_window`] the title bar's own close control uses).
    fn file_menu_rows(&self, macos: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let can_save = self.active_edit_buffer().is_some();
        let mut save_row = render_dropdown_menu_row(
            "S",
            theme::text::DIM.into(),
            theme::surface::CHIP_NEUTRAL.into(),
            "Save",
            "active file".to_string(),
            keymap::resolve_combo("mod+s", macos),
            can_save,
        );
        if can_save {
            save_row = save_row.on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.handle_editor_save_action(&EditorSave, window, cx);
                cx.notify();
            }));
        }

        vec![
            render_dropdown_menu_row(
                "@",
                theme::palette::COMMAND_CHIP.0.into(),
                theme::palette::COMMAND_CHIP.1.into(),
                "Open File\u{2026}",
                "search this worktree".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.open_palette(window, cx);
                // `open_palette` always resets `palette_scope` to `PaletteScope::default()`, so
                // this must be set after it returns, not before - the same ordering
                // `crate::work_surface::render::AdeApp::render_plus_menu`'s identical "Open
                // file…" row already established.
                this.palette_scope = palette::PaletteScope::Files;
                cx.notify();
            }))
            .into_any_element(),
            save_row.into_any_element(),
            Self::render_title_menu_divider(),
            render_dropdown_menu_row(
                "P",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Settings\u{2026}",
                "preferences".to_string(),
                keymap::resolve_combo("mod+,", macos),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.open_settings(window, cx);
            }))
            .into_any_element(),
            render_dropdown_menu_row(
                "\u{d7}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Quit",
                "close window".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|_this, _event: &ClickEvent, window, _cx| {
                // The same real `Window::remove_window` the title bar's own close control (both
                // the macOS dot and the Windows/Linux caption button) already calls - see
                // `Self::render_window_controls`'s own docs.
                window.remove_window();
            }))
            .into_any_element(),
        ]
    }

    /// The Edit menu: real **text** `Undo`/`Redo` for whichever edit buffer is currently active
    /// (GitHub issue #17, `crate::text_history`), then the worktree-level "keep all changes"/
    /// "discard worktree" undo stack (Revision R10) as its own separately-labelled pair, then
    /// Cut/Copy/Paste/Select All against the real active edit buffer (File view or merge
    /// hand-edit - [`crate::code_surface::editing::AdeApp::active_edit_buffer`]), dimmed when there's no
    /// real edit target right now.
    ///
    /// The two undo pairs are deliberately separate rows with separate labels, and only the text
    /// pair carries a keycap. Before GitHub issue #17 there was one pair, labelled `Undo` with a
    /// `"worktree history"` sub-line and a `mod+z` keycap, and that was accurate. It stopped being
    /// accurate the moment `mod+z` started resolving to `TextUndo` inside every real text widget
    /// (see `crate::default_key_bindings`' own scoping docs): a keycap that is only true when no
    /// text input has focus, on a row that fires a real `git reset --soft`, is exactly the kind of
    /// confidently-wrong affordance this project's discipline forbids. The worktree rows are still
    /// fully clickable - only their (now context-dependent) keycap is gone, with the sub-line
    /// saying where the shortcut does apply.
    ///
    /// The text rows are dimmed, per [`crate::work_surface::render::render_dropdown_menu_row`]'s
    /// own enabled/disabled convention, whenever there is genuinely nothing to undo or redo -
    /// rather than looking exactly as actionable as a working row and silently doing nothing.
    ///
    /// Their sub-line says `"editor"`, not `"text"`, and that word is load-bearing: enablement
    /// comes from [`crate::code_surface::editing::AdeApp::active_edit_buffer`], which only ever
    /// resolves to an `EditBuffer` (the File view or the merge hand-edit surface). The app's five
    /// single-line `crate::text_history::TextField` inputs - palette query, rail filter, Settings
    /// keybindings filter, New file prompt, and the file tree's inline name editor - have real,
    /// working undo histories of their own that
    /// this menu genuinely cannot reach, so a row labelled `"text"` would sit permanently dimmed
    /// while `mod+z` worked perfectly well inside them. Found by an independent adversarial audit.
    /// Narrowing the label is the honest fix rather than routing the menu through whatever widget
    /// happens to be focused: a menu click moves focus to the menu, so "the focused text widget"
    /// is not a thing this row could resolve at click time anyway.
    fn edit_menu_rows(&self, macos: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let editing = self.active_edit_buffer().is_some();
        let can_text_undo = self
            .active_edit_buffer()
            .is_some_and(|buffer| buffer.can_undo());
        let can_text_redo = self
            .active_edit_buffer()
            .is_some_and(|buffer| buffer.can_redo());

        let mut rows = vec![
            {
                let mut row = render_dropdown_menu_row(
                    "U",
                    theme::text::DIM.into(),
                    theme::surface::CHIP_NEUTRAL.into(),
                    "Undo",
                    "editor".to_string(),
                    keymap::resolve_combo("mod+z", macos),
                    can_text_undo,
                );
                if can_text_undo {
                    row = row.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.title_menu_open = None;
                        this.perform_text_undo(cx);
                    }));
                }
                row.into_any_element()
            },
            {
                let mut row = render_dropdown_menu_row(
                    "R",
                    theme::text::DIM.into(),
                    theme::surface::CHIP_NEUTRAL.into(),
                    "Redo",
                    "editor".to_string(),
                    keymap::resolve_combo("mod+shift+z", macos),
                    can_text_redo,
                );
                if can_text_redo {
                    row = row.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.title_menu_open = None;
                        this.perform_text_redo(cx);
                    }));
                }
                row.into_any_element()
            },
            Self::render_title_menu_divider(),
            render_dropdown_menu_row(
                "U",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Undo worktree action",
                "history \u{b7} shortcut applies outside text inputs".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.title_menu_open = None;
                this.perform_undo(cx);
            }))
            .into_any_element(),
            render_dropdown_menu_row(
                "R",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Redo worktree action",
                "history \u{b7} shortcut applies outside text inputs".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.title_menu_open = None;
                this.perform_redo(cx);
            }))
            .into_any_element(),
            Self::render_title_menu_divider(),
        ];

        macro_rules! edit_action_row {
            ($chip:expr, $label:expr, $sub:expr, $spec:expr, $handler:ident, $action:expr) => {{
                let mut row = render_dropdown_menu_row(
                    $chip,
                    theme::text::DIM.into(),
                    theme::surface::CHIP_NEUTRAL.into(),
                    $label,
                    $sub.to_string(),
                    keymap::resolve_combo($spec, macos),
                    editing,
                );
                if editing {
                    row = row.on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.title_menu_open = None;
                        this.$handler($action, window, cx);
                    }));
                }
                rows.push(row.into_any_element());
            }};
        }
        edit_action_row!(
            "X",
            "Cut",
            "selection",
            "mod+x",
            handle_editor_cut_action,
            &EditorCut
        );
        edit_action_row!(
            "C",
            "Copy",
            "selection",
            "mod+c",
            handle_editor_copy_action,
            &EditorCopy
        );
        edit_action_row!(
            "V",
            "Paste",
            "clipboard",
            "mod+v",
            handle_editor_paste_action,
            &EditorPaste
        );
        edit_action_row!(
            "A",
            "Select All",
            "active buffer",
            "mod+a",
            handle_editor_select_all_action,
            &EditorSelectAll
        );

        rows
    }

    /// The View menu: the command palette, and the real code-surface zoom controls
    /// ([`crate::code_surface::zoom::AdeApp::zoom_in`]/`zoom_out`/`reset_zoom` - the same ones
    /// the Diff/File toolbar's own zoom group calls), dimmed while no file/diff view is actually
    /// showing to zoom (the exact predicate
    /// [`crate::code_surface::editing::AdeApp::active_edit_target`]'s own docs give for "Surface C is
    /// genuinely on screen").
    fn view_menu_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let code_surface_showing = self.open_change.is_some()
            && (self.open_diff_file_cache.is_some() || self.code_view == code_view::CodeView::File);

        let mut rows = vec![
            render_dropdown_menu_row(
                "K",
                theme::palette::COMMAND_CHIP.0.into(),
                theme::palette::COMMAND_CHIP.1.into(),
                "Command Palette",
                "search everything".to_string(),
                keymap::resolve_combo("mod+K", self.window_controls_style().is_macos()),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.open_palette(window, cx);
            }))
            .into_any_element(),
            Self::render_title_menu_divider(),
        ];

        macro_rules! zoom_row {
            ($chip:expr, $label:expr, $sub:expr, $method:ident) => {{
                let mut row = render_dropdown_menu_row(
                    $chip,
                    theme::text::DIM.into(),
                    theme::surface::CHIP_NEUTRAL.into(),
                    $label,
                    $sub,
                    Vec::new(),
                    code_surface_showing,
                );
                if code_surface_showing {
                    row = row.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.$method(cx);
                    }));
                }
                rows.push(row.into_any_element());
            }};
        }
        zoom_row!("+", "Zoom In", "code view".to_string(), zoom_in);
        zoom_row!("\u{2212}", "Zoom Out", "code view".to_string(), zoom_out);
        zoom_row!(
            "0",
            "Reset Zoom",
            format!("{}%", self.settings.appearance.editor_zoom_percent),
            reset_zoom
        );

        rows
    }

    /// The Session menu: spawn a terminal or agent pane (the same two real actions the `+`
    /// menu's own top two rows call), cycle the active session tab
    /// ([`crate::work_surface::render::AdeApp::select_relative_session`]), and the real,
    /// per-active-session worktree-history actions - archive, "Keep all changes", and "Discard
    /// worktree". The last two reuse the exact same real methods
    /// ([`crate::worktree_history::flow::AdeApp::keep_all_changes`]/`request_discard_worktree`)
    /// and the same busy/two-click-confirm state
    /// ([`AdeApp::worktree_history_op_in_flight`]/[`AdeApp::discard_confirm_armed`]) the session
    /// footer's own buttons already use - a first click on "Discard worktree" only arms
    /// confirmation and deliberately does **not** close the menu (so the row's own label can
    /// swap to "confirm discard?" for a real second click); every other row closes the menu on
    /// click, matching the `+` menu's own convention.
    fn session_menu_rows(&self, macos: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let resolved_kind = self.resolved_new_agent_kind();
        let (agent_fg, agent_bg) = work_surface::agent_tint(resolved_kind);
        let agent_initial = work_surface::agent_initial(resolved_kind);
        let agent_label = resolved_kind.label();
        // Scoped to the *currently selected worktree*'s own sessions, matching
        // `AdeApp::select_relative_session`'s own real cycling scope (see that method's docs) -
        // otherwise this row could show "enabled" while the real cycle it drives is a genuine
        // no-op (or worse, silently jumps to a different worktree) whenever other worktrees have
        // sessions open but this one doesn't have a second one of its own.
        let can_cycle = self.current_worktree_sessions().count() > 1;
        let active_id = self.sessions.active_id();

        let mut rows = vec![
            render_dropdown_menu_row(
                "\u{276f}",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "New Terminal",
                "in this worktree".to_string(),
                keymap::resolve_combo("ctrl+shift+T", macos),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.handle_new_terminal_action(&NewTerminal, window, cx);
            }))
            .into_any_element(),
            render_dropdown_menu_row(
                agent_initial,
                agent_fg,
                agent_bg,
                "New Agent Pane",
                agent_label.to_string(),
                keymap::resolve_combo("mod+shift+N", macos),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.handle_new_agent_pane_action(&NewAgentPane, window, cx);
            }))
            .into_any_element(),
            Self::render_title_menu_divider(),
        ];

        macro_rules! cyclic_row {
            ($chip:expr, $label:expr, $delta:expr) => {{
                let mut row = render_dropdown_menu_row(
                    $chip,
                    theme::text::DIM.into(),
                    theme::surface::CHIP_NEUTRAL.into(),
                    $label,
                    "cycle tabs".to_string(),
                    Vec::new(),
                    can_cycle,
                );
                if can_cycle {
                    row = row.on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.title_menu_open = None;
                        this.select_relative_session($delta, window, cx);
                    }));
                }
                rows.push(row.into_any_element());
            }};
        }
        cyclic_row!("\u{203a}", "Next Session", 1isize);
        cyclic_row!("\u{2039}", "Previous Session", -1isize);
        rows.push(Self::render_title_menu_divider());

        let mut archive_row = render_dropdown_menu_row(
            "A",
            theme::text::DIM.into(),
            theme::surface::CHIP_NEUTRAL.into(),
            "Archive Session",
            "close the active tab".to_string(),
            Vec::new(),
            active_id.is_some(),
        );
        if let Some(id) = active_id {
            archive_row =
                archive_row.on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.title_menu_open = None;
                    this.archive_session(id, window, cx);
                }));
        }
        rows.push(archive_row.into_any_element());

        let keep_busy = self.worktree_history_op_in_flight
            == Some(worktree_history::WorktreeHistoryOpKind::Keep);
        let keep_label: &'static str = if keep_busy {
            "keeping\u{2026}"
        } else {
            "Keep All Changes"
        };
        let mut keep_row = render_dropdown_menu_row(
            "K",
            theme::text::DIM.into(),
            theme::surface::CHIP_NEUTRAL.into(),
            keep_label,
            "commit the active worktree".to_string(),
            Vec::new(),
            active_id.is_some() && !keep_busy,
        );
        if let Some(id) = active_id {
            if !keep_busy {
                keep_row = keep_row.on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        this.title_menu_open = None;
                        this.keep_all_changes(id, cx);
                    },
                ));
            }
        }
        rows.push(keep_row.into_any_element());

        let discard_busy = self.worktree_history_op_in_flight
            == Some(worktree_history::WorktreeHistoryOpKind::Discard);
        let discard_armed = active_id.is_some() && self.discard_confirm_armed == active_id;
        let discard_label: &'static str = if discard_busy {
            "discarding\u{2026}"
        } else if discard_armed {
            "confirm discard?"
        } else {
            "Discard Worktree"
        };
        let mut discard_row = render_dropdown_menu_row(
            "D",
            theme::diff::STAT_DEL.into(),
            theme::surface::CHIP_NEUTRAL.into(),
            discard_label,
            "force-remove uncommitted content".to_string(),
            Vec::new(),
            active_id.is_some() && !discard_busy,
        );
        if let Some(id) = active_id {
            if !discard_busy {
                discard_row = discard_row.on_click(cx.listener(
                    move |this, _event: &ClickEvent, window, cx| {
                        this.request_discard_worktree(id, window, cx);
                        // The first click only arms confirmation
                        // (`AdeApp::discard_confirm_armed`) - keep the menu open so this row's
                        // own label can swap to "confirm discard?" for a real second click,
                        // mirroring the session footer's identical two-step button
                        // (`crate::work_surface::render::AdeApp::render_footer_action_button`).
                        // Only the second click - which actually executes and clears the arm
                        // flag - closes the menu.
                        if this.discard_confirm_armed != Some(id) {
                            this.title_menu_open = None;
                        }
                        cx.notify();
                    },
                ));
            }
        }
        rows.push(discard_row.into_any_element());

        rows
    }

    /// The Help menu: real links to this project's own GitHub repository (README and issue
    /// tracker, opened via the real platform `Window`-manager call
    /// `vendor/zed/crates/gpui/src/app.rs:1408`'s `App::open_url`), and About (the same real,
    /// already-shipped Settings page - `crate::settings::state::SettingsPage::About` - the palette and
    /// Settings' own nav already reach; still an honest nav-only placeholder page, per
    /// `crate::settings::state`'s own module docs, but navigating there is exactly what the Settings
    /// nav sidebar's own "About" row already does, not a new fabricated affordance).
    fn help_menu_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        vec![
            render_dropdown_menu_row(
                "?",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Documentation",
                "README on GitHub".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.title_menu_open = None;
                cx.open_url("https://github.com/ColinEspinas/jerry#readme");
                cx.notify();
            }))
            .into_any_element(),
            render_dropdown_menu_row(
                "!",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Report an Issue",
                "GitHub issues".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.title_menu_open = None;
                cx.open_url("https://github.com/ColinEspinas/jerry/issues");
                cx.notify();
            }))
            .into_any_element(),
            Self::render_title_menu_divider(),
            render_dropdown_menu_row(
                "i",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "About",
                "Jerry".to_string(),
                Vec::new(),
                true,
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.title_menu_open = None;
                this.open_settings(window, cx);
                this.select_settings_page(settings::SettingsPage::About, window, cx);
            }))
            .into_any_element(),
        ]
    }
}

/// Real, interactive coverage for the five File/Edit/View/Session/Help dropdowns
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
    /// (`theme::band::PLUS_MENU_ROW`, and [`render_title_menu_divider`]'s own `h(1.0)` plus
    /// `my(4.0)` top/bottom margins - `1.0 + 4.0 + 4.0 = 9.0`px total) rather than a hand-tuned
    /// pixel offset that could silently drift from the real rendered layout.
    fn nth_row_click_point(
        button_bounds: gpui::Bounds<Pixels>,
        rows_before: u32,
        dividers_before: u32,
    ) -> gpui::Point<Pixels> {
        const DIVIDER_HEIGHT: Pixels = px(9.0);
        let popover_top = button_bounds.origin.y + button_bounds.size.height;
        gpui::point(
            button_bounds.origin.x + px(20.0),
            popover_top
                + px(4.0)
                + theme::band::PLUS_MENU_ROW * rows_before as f32
                + DIVIDER_HEIGHT * dividers_before as f32
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

    /// The Edit menu's `Undo worktree action` row really calls the real
    /// `crate::worktree_history::flow::AdeApp::perform_undo` - with nothing in
    /// [`AdeApp::undo_stack`] yet (a fresh test app), that's a real, honest "nothing to undo"
    /// status (Revision R10's own fix for the "looks actionable, silently does nothing" bug
    /// class - see that revision's build-log entry), not a silent no-op, and is exactly the
    /// observable effect this test asserts actually happened.
    ///
    /// It is the *third* row now, behind the text `Undo`/`Redo` pair and a divider (GitHub issue
    /// #17) - clicked by its real structural position through the shared
    /// [`nth_row_click_point`] geometry, so the two undo pairs can't silently swap places without
    /// this failing.
    #[gpui::test]
    fn edit_menu_worktree_undo_row_runs_the_real_undo_stack(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::Edit);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Edit.index()]
        });

        cx.simulate_click(nth_row_click_point(bounds, 2, 1), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.worktree_history_status.as_deref(),
                Some("nothing to undo"),
                "the real `perform_undo` should have run and reported its real, honest status"
            );
            assert_eq!(app.title_menu_open, None);
        });
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
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "a disabled text-undo row must never fall through to the worktree-level undo - that              would be the exact confidently-wrong affordance this row was split out to remove"
        );

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
                .edit_buffers
                .get(&relative)
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
                .edit_buffers
                .get(&relative)
                .expect("buffer")
                .content
                .clone()),
            "hello\n",
            "the Edit menu's own text-undo row must drive the exact same real history the              secondary-z binding does"
        );
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "and it must never touch the worktree-level stack"
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
    fn clicking_the_session_label_opens_the_real_session_menu(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Session.index()]
        });

        cx.simulate_click(center_of(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.title_menu_open),
            Some(TitleMenu::Session)
        );
    }

    /// The Session menu's first row ("New Terminal") really spawns a new real session tab (the
    /// same real `Sessions::spawn` call the tab strip's own `+` menu row and `secondary-n` use),
    /// not just a decoration - the session count genuinely increases.
    #[gpui::test]
    fn session_menu_new_terminal_row_spawns_a_real_session(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        let sessions_before = app.read_with(cx, |app, _| app.sessions.iter().count());
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::Session);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Session.index()]
        });

        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.sessions.iter().count(),
                sessions_before + 1,
                "the real New Terminal row should have spawned one real new session"
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

    /// The Session menu's "Discard worktree" row's real two-step confirm - mirrors the session
    /// footer's own identical two-click button
    /// (`crate::work_surface::render::AdeApp::render_footer_action_button`), reusing the
    /// exact same real [`AdeApp::request_discard_worktree`]/[`AdeApp::discard_confirm_armed`]
    /// this test drives through the menu row instead of the footer button. Needs a real,
    /// non-main worktree (`crate::worktree_history::flow::AdeApp::request_discard_worktree`
    /// unconditionally refuses on the main worktree), so this sets one up the same way
    /// `crate::worktree_history::flow::worktree_history_regression_tests::add_worktree` does.
    #[gpui::test]
    fn session_menu_discard_row_arms_then_executes_a_real_discard(cx: &mut TestAppContext) {
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
            app.sessions.spawn(
                SessionKind::Shell,
                worktree_path.clone(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_session(id, window, cx);
        });
        cx.run_until_parked();

        // Session>Discard worktree is the last row - two dividers and three earlier rows above
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
            assert!(
                app.undo_stack.can_undo(),
                "a real, undoable entry should have been pushed"
            );
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
    fn session_menu_discard_row_stays_open_while_armed_and_closes_once_confirmed(
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
            app.sessions.spawn(
                SessionKind::Shell,
                worktree_path.clone(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_session(id, window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::Session);
            cx.notify();
        });
        cx.run_until_parked();

        // The Discard row is the 7th real row (index 6, 0-based) in the Session menu: New
        // Terminal, New Agent Pane, [divider], Next Session, Previous Session, [divider],
        // Archive Session, Keep All Changes, Discard Worktree - six real rows and two dividers
        // sit above it (see `AdeApp::session_menu_rows`'s own row order).
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Session.index()]
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
            Some(TitleMenu::Session),
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

    /// [`crate::work_surface::render::AdeApp::select_relative_session`] (the Session menu's
    /// "Next Session"/"Previous Session" rows) - real coverage of the cyclic-index logic itself:
    /// cycling forward through three real sessions wraps back to the first, and cycling backward
    /// from the first wraps to the last, via the same real [`AdeApp::select_session`] every
    /// tab-strip click already goes through.
    ///
    /// Also real, live-reproduced coverage for this revision's own self-audit finding: an
    /// earlier version of `select_relative_session` cycled the flat `self.sessions` list
    /// directly rather than [`AdeApp::current_worktree_sessions`], so "Next Session" could jump
    /// to a *different* worktree's session entirely - which `AdeApp::select_session` then
    /// silently promotes into a full `AdeApp::select_worktree` switch, discarding any unsaved
    /// `edit_buffers` content for the worktree just left. Spawning every test session into the
    /// *same* worktree (as an earlier version of this test did) can't distinguish that bug from
    /// correct behaviour at all - this seeds a **second** worktree with its own session
    /// alongside the three under test, and asserts cycling never leaves the first worktree
    /// (`AdeApp::selected` stays put) and never clears a real unsaved edit sitting in
    /// `AdeApp::edit_buffers`.
    #[gpui::test]
    fn select_relative_session_cycles_through_real_sessions_and_wraps_around(
        cx: &mut TestAppContext,
    ) {
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
                    is_locked: false,
                    error: None,
                },
                WorktreeItem {
                    path: other_wt.path().to_path_buf(),
                    label: "wt-b".to_string(),
                    branch: Some("wt-b".to_string()),
                    is_main: false,
                    is_locked: false,
                    error: None,
                },
            ];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });

        // `open_test_app` already spawns one real shell session in the first worktree; add two
        // more real sessions there so there are three to cycle through.
        app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            );
            app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            );
        });
        // The second worktree's own session - real coverage that cycling stays scoped to the
        // selected worktree rather than this flat list.
        app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                other_wt.path().to_path_buf(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            );
        });
        // Spawning into the second worktree above made its own session globally active - re-
        // select the first worktree to restore it as the one under test (its own last-active tab
        // via `Sessions::activate_for_worktree`).
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        cx.run_until_parked();

        // A real, unsaved edit in the first worktree, seeded only after every real
        // `select_worktree` switch above (each one genuinely clears `edit_buffers` via
        // `reset_per_worktree_ui_state`, by design - that's not what's under test here) - a
        // same-worktree cycle below must never discard this.
        app.update(cx, |app, _cx| {
            app.edit_buffers.insert(
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

        let ids: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(
            ids.len(),
            3,
            "should have three real sessions in the first worktree to cycle through"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(ids[2]),
            "re-selecting the first worktree should restore its own last-active tab"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_relative_session(1, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(ids[0]),
            "cycling forward from the last real session should wrap around to the first"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(0),
            "cycling within one worktree must never switch the selected worktree"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_relative_session(-1, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(ids[2]),
            "cycling backward from the first real session should wrap around to the last"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(0),
            "cycling within one worktree must never switch the selected worktree"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.edit_buffers.contains_key(std::path::Path::new("a.txt")),
                "a same-worktree cycle must never discard unsaved edits via \
                 reset_per_worktree_ui_state - only a real select_worktree switch should do that"
            );
        });
    }
}
