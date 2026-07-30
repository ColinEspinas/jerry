use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::work_surface_render::render_dropdown_menu_row;

/// Half-diagonal of an 11×1px rect rotated ±45° about its own center - `5.5 * cos(45°)`. Used to
/// place the close glyph's two crossing strokes (see [`render_close_glyph`]).
const CLOSE_GLYPH_HALF_DIAGONAL: f32 = 3.889_87; // 5.5 * cos(45°)

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
pub(super) enum TitleMenu {
    File,
    Edit,
    View,
    Session,
    Help,
}

impl TitleMenu {
    pub(super) const ALL: [TitleMenu; 5] = [
        TitleMenu::File,
        TitleMenu::Edit,
        TitleMenu::View,
        TitleMenu::Session,
        TitleMenu::Help,
    ];

    pub(super) fn index(self) -> usize {
        match self {
            TitleMenu::File => 0,
            TitleMenu::Edit => 1,
            TitleMenu::View => 2,
            TitleMenu::Session => 3,
            TitleMenu::Help => 4,
        }
    }

    /// This menu's real display label in the Windows/Linux title bar's left cluster.
    pub(super) fn label(self) -> &'static str {
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
    /// The three flat-circle window controls in the title bar's left cluster, wired to real GPUI
    /// window-control methods (`Window::remove_window`/`minimize_window`/`zoom_window`, verified
    /// against `vendor/zed/crates/gpui/src/window.rs:2016,5520,2489`) - the same calls
    /// `vendor/zed/crates/platform_title_bar/src/platforms/platform_linux.rs`'s own
    /// `WindowControl::on_click` makes. Left-to-right order (close, minimize, maximize) follows
    /// the macOS traffic-light convention; the design doesn't colour-code these dots, so there's
    /// no ordering hint from the mockup itself.
    ///
    /// The wrapping row stops left-click propagation on mouse-down so pressing a dot can never
    /// also arm [`Self::render_title_bar`]'s window-move drag.
    pub(super) fn render_window_controls(&self) -> impl IntoElement {
        div()
            .id("window-controls")
            .flex()
            .gap(px(8.0))
            .pl(px(2.0))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(window_control_dot("title-bar-close", |window, _cx| {
                window.remove_window();
            }))
            .child(window_control_dot("title-bar-minimize", |window, _cx| {
                window.minimize_window();
            }))
            .child(window_control_dot("title-bar-maximize", |window, _cx| {
                window.zoom_window();
            }))
    }

    /// The macOS-style left cluster: [`Self::render_window_controls`]'s three dots, plus a
    /// trailing 1×16 divider.
    fn render_macos_title_bar_left(&self) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(2.0))
            .child(self.render_window_controls())
            .child(
                div()
                    .flex_none()
                    .ml(px(6.0))
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(theme::border::DIVIDER),
            )
            .into_any_element()
    }

    /// The Windows/Linux-style left cluster: [`TitleMenu::ALL`]'s five real menu triggers,
    /// plus the same trailing divider. Each label toggles [`AdeApp::title_menu_open`] (clicking
    /// the already-open one closes it; clicking a different one switches directly, matching
    /// ordinary desktop menu-bar behaviour) and captures its own painted bounds into
    /// [`AdeApp::title_menu_button_bounds`] via a `gpui::canvas` child, the same pattern
    /// [`crate::root::work_surface_render::AdeApp::render_tab_strip_plus`] uses for the `+`
    /// menu's single button.
    ///
    /// The wrapping row stops left-click propagation on mouse-down, the same guard
    /// [`Self::render_window_controls`] uses - now that these labels are real click targets
    /// (unlike their earlier inert form, which deliberately let a press-and-drag starting on one
    /// fall through into [`Self::render_title_bar`]'s window-move arming), a click here must
    /// never also start a window move.
    fn render_windows_title_bar_left(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("windows-title-bar-menu")
            .flex()
            .items_center()
            .gap(px(2.0))
            .ml(px(-4.0))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .children(TitleMenu::ALL.iter().enumerate().map(|(index, &menu)| {
                let is_open = self.title_menu_open == Some(menu);
                let this = cx.entity();
                div()
                    .id(("title-bar-menu", index))
                    .flex_none()
                    .h(theme::band::TITLE_BAR_MENU_ITEM)
                    .px(px(8.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .when(is_open, |el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(11.0))
                            .text_color(theme::text::DIM)
                            .child(menu.label()),
                    )
                    .child(
                        gpui::canvas(
                            move |bounds, _window, cx| {
                                this.update(cx, |this, _cx| {
                                    this.title_menu_button_bounds[index] = bounds;
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.title_menu_open = if this.title_menu_open == Some(menu) {
                            None
                        } else {
                            this.plus_menu_open = false;
                            Some(menu)
                        };
                        cx.notify();
                    }))
            }))
            .child(
                div()
                    .flex_none()
                    .ml(px(6.0))
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(theme::border::DIVIDER),
            )
            .into_any_element()
    }

    /// The real popover for `menu`, whichever [`TitleMenu`] the caller already knows
    /// [`AdeApp::title_menu_open`] currently names - threaded through as a parameter (rather than
    /// unwrapping `title_menu_open` again in here) since the one real call site
    /// ([`Self::render`]/`AdeApp::render`'s own `.when_some(self.title_menu_open, ..)`) has
    /// already guarded on it being `Some`; re-deriving that with a second `.expect(..)` down here
    /// would just be a second place the same invariant could silently stop holding.
    ///
    /// Same scrim-plus-popover shape as
    /// [`crate::root::work_surface_render::AdeApp::render_plus_menu`]: a full-screen transparent
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
    pub(super) fn render_title_menu(
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
    /// [`crate::root::editing::AdeApp::save_active_file`]'s own guard), open Settings, and quit
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
                // `crate::root::work_surface_render::AdeApp::render_plus_menu`'s identical "Open
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

    /// The Edit menu: `Undo`/`Redo` (Revision R10's real, worktree-level "keep all changes"/
    /// "discard worktree" undo stack - labelled with a "worktree history" sub-line, not text
    /// editing, since this app has no separate per-buffer text undo/redo to offer), then
    /// Cut/Copy/Paste/Select All against the real active edit buffer (File view or merge
    /// hand-edit - [`crate::root::editing::AdeApp::active_edit_buffer`]), dimmed when there's no
    /// real edit target right now.
    fn edit_menu_rows(&self, macos: bool, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let editing = self.active_edit_buffer().is_some();

        let mut rows = vec![
            render_dropdown_menu_row(
                "U",
                theme::text::DIM.into(),
                theme::surface::CHIP_NEUTRAL.into(),
                "Undo",
                "worktree history".to_string(),
                keymap::resolve_combo("mod+z", macos),
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
                "Redo",
                "worktree history".to_string(),
                keymap::resolve_combo("mod+shift+z", macos),
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
    /// ([`crate::root::code_surface::AdeApp::zoom_in`]/`zoom_out`/`reset_zoom` - the same ones
    /// the Diff/File toolbar's own zoom group calls), dimmed while no file/diff view is actually
    /// showing to zoom (the exact predicate
    /// [`crate::root::editing::AdeApp::active_edit_target`]'s own docs give for "Surface C is
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
    /// ([`crate::root::work_surface_render::AdeApp::select_relative_session`]), and the real,
    /// per-active-session worktree-history actions - archive, "Keep all changes", and "Discard
    /// worktree". The last two reuse the exact same real methods
    /// ([`crate::root::worktree_history::AdeApp::keep_all_changes`]/`request_discard_worktree`)
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
                        // (`crate::root::work_surface_render::AdeApp::render_footer_action_button`).
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
    /// already-shipped Settings page - `crate::settings::SettingsPage::About` - the palette and
    /// Settings' own nav already reach; still an honest nav-only placeholder page, per
    /// `crate::settings`'s own module docs, but navigating there is exactly what the Settings
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
                this.select_settings_page(settings::SettingsPage::About, cx);
            }))
            .into_any_element(),
        ]
    }

    /// The Windows/Linux title bar's three caption buttons (minimise/maximise/close), pinned to
    /// the band's right edge, 44px wide × full band height, bleeding past the band's own 12px
    /// right padding (`.mr(px(-12.0))`). Wired to the same [`Window::minimize_window`]/
    /// [`Window::zoom_window`]/[`Window::remove_window`] calls [`Self::render_window_controls`]
    /// uses - the macOS dot cluster and these caption buttons are two skins over identical real
    /// window-control behaviour, not two independently implemented ones. Only the close button's
    /// glyph uses [`theme::text::SECONDARY`]; minimize/maximize use the dimmer
    /// [`render_minimize_glyph`]/[`render_maximize_glyph`] default.
    fn render_windows_caption_buttons(&self) -> impl IntoElement {
        div()
            .id("title-bar-caption-buttons")
            .flex()
            .self_stretch()
            .ml(px(2.0))
            .mr(px(-12.0))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(render_caption_button(
                "title-bar-caption-minimize",
                theme::surface::ROW_HOVER_ALT.into(),
                render_minimize_glyph(),
                |window, _cx| window.minimize_window(),
            ))
            .child(render_caption_button(
                "title-bar-caption-maximize",
                theme::surface::ROW_HOVER_ALT.into(),
                render_maximize_glyph(),
                |window, _cx| window.zoom_window(),
            ))
            .child(render_caption_button(
                "title-bar-caption-close",
                theme::surface::TITLE_BAR_CLOSE_HOVER.into(),
                render_close_glyph(theme::text::SECONDARY.into()),
                |window, _cx| window.remove_window(),
            ))
    }

    /// The 38px title-bar band: real window content (not OS chrome - this band draws itself
    /// regardless of the outer window frame), carrying a platform-dependent left cluster
    /// ([`Self::render_macos_title_bar_left`] or [`Self::render_windows_title_bar_left`], per
    /// [`Self::window_controls_style`]), the real project name/branch, and, on the
    /// Windows/Linux variant only, [`Self::render_windows_caption_buttons`] pinned to the right
    /// edge.
    ///
    /// ## Dragging the window
    ///
    /// GPUI has no single "make this element drag the window" method. The real pattern (matching
    /// `vendor/zed/crates/platform_title_bar/src/platform_title_bar.rs`'s own title bar, which
    /// faces the same "no native draggable titlebar for a client-side-decorated window on
    /// Wayland/X11" problem): mark the area with `.window_control_area(WindowControlArea::Drag)`
    /// (a hit-test hint the compositor consults, `vendor/zed/crates/gpui/src/elements/
    /// div.rs:1166`), then drive the actual move from ordinary mouse events - arm
    /// [`Self::title_bar_move_armed`] on left mouse-down, and on the next mouse-move (still
    /// armed) call `Window::start_window_move` (`window.rs:2502`) and disarm.
    /// `on_mouse_up`/`on_mouse_down_out` also disarm, so a click that never moves (e.g. clicking
    /// to focus the window) never starts a move. [`Self::render_window_controls`],
    /// [`Self::render_windows_caption_buttons`], and (now that its five labels are real click
    /// targets, not inert text - see [`TitleMenu`]'s own docs) [`Self::
    /// render_windows_title_bar_left`] all stop propagation on their own mouse-down, so pressing
    /// any of those controls can never also arm this drag.
    pub(super) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = self
            .repo_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repo_path.display().to_string());
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.is_main)
            .and_then(|item| item.branch.clone());
        let macos = self.window_controls_style().is_macos();

        div()
            .id("title-bar")
            .window_control_area(WindowControlArea::Drag)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(14.0))
            .px(px(12.0))
            .w_full()
            .h(theme::band::TITLE_BAR)
            .bg(theme::surface::TITLE_BAR)
            .border_b_1()
            .border_color(theme::border::ZONE)
            .on_mouse_down_out(cx.listener(|this, _event, _window, _cx| {
                this.title_bar_move_armed = false;
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.title_bar_move_armed = false;
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.title_bar_move_armed = true;
                }),
            )
            .on_mouse_move(cx.listener(|this, _event, window, _cx| {
                if this.title_bar_move_armed {
                    this.title_bar_move_armed = false;
                    window.start_window_move();
                }
            }))
            .child(if macos {
                self.render_macos_title_bar_left()
            } else {
                self.render_windows_title_bar_left(cx)
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(12.0))
                            .text_color(theme::text::STRONG)
                            .child(project_name),
                    )
                    .when_some(branch, |el, branch| {
                        el.child(
                            div()
                                .font(font(theme::font::MONO))
                                .text_size(px(11.0))
                                .text_color(theme::text::FAINTER)
                                .child(branch),
                        )
                    }),
            )
            .child(div().flex_1())
            .when(!macos, |el| el.child(self.render_windows_caption_buttons()))
    }
}

/// One flat-circle window-control button - `on_activate` is called with real `&mut Window`/
/// `&mut App` access so it can invoke a real `Window` control method.
pub(super) fn window_control_dot(
    id: &'static str,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(11.0))
        .h(px(11.0))
        .rounded(px(5.5))
        .bg(theme::text::GUTTER)
        .cursor_pointer()
        .hover(|el| el.bg(theme::text::FAINT))
        .on_click(move |_event, window, cx| {
            cx.stop_propagation();
            on_activate(window, cx);
        })
}

/// One Windows/Linux caption button - 44 wide × full band height (`self_stretch`d by the caller),
/// `hover_bg` on hover, `glyph` centered inside, wired to a real `Window` control method via
/// `on_activate`.
fn render_caption_button(
    id: &'static str,
    hover_bg: gpui::Rgba,
    glyph: impl IntoElement,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .w(theme::band::TITLE_BAR_CAPTION_BUTTON)
        .self_stretch()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |el| el.bg(hover_bg))
        .child(glyph)
        .on_click(move |_event, window, cx| {
            cx.stop_propagation();
            on_activate(window, cx);
        })
}

/// The minimise caption button's glyph - a plain 10×1px rect.
fn render_minimize_glyph() -> impl IntoElement {
    div().w(px(10.0)).h(px(1.0)).bg(theme::text::DIM)
}

/// The maximise caption button's glyph - a plain 9×9px 1px outline.
fn render_maximize_glyph() -> impl IntoElement {
    div()
        .w(px(9.0))
        .h(px(9.0))
        .border_1()
        .border_color(theme::text::DIM)
}

/// The close caption button's `×` glyph - two 11×1px lines crossing at ±45°. GPUI's `Style` has
/// no CSS-transform-style `rotate` (verified: no `rotate`/`transform` field anywhere in
/// `vendor/zed/crates/gpui/src/style.rs`), so this paints two strokes directly
/// (`vendor/zed/crates/gpui/examples/painting.rs`'s `PathBuilder::stroke` + `canvas`/
/// `Window::paint_path` pattern) with endpoints placed where an 11×1 rect rotated ±45° about its
/// own center would put them - see [`CLOSE_GLYPH_HALF_DIAGONAL`].
fn render_close_glyph(color: gpui::Rgba) -> impl IntoElement {
    gpui::canvas(
        move |_bounds, _window, _cx| {},
        move |bounds, _state, window, _cx| {
            let half = px(CLOSE_GLYPH_HALF_DIAGONAL);
            let center_x = bounds.origin.x + bounds.size.width / 2.0;
            let center_y = bounds.origin.y + bounds.size.height / 2.0;
            let diagonals = [
                (
                    gpui::point(center_x - half, center_y - half),
                    gpui::point(center_x + half, center_y + half),
                ),
                (
                    gpui::point(center_x - half, center_y + half),
                    gpui::point(center_x + half, center_y - half),
                ),
            ];
            for (start, end) in diagonals {
                let mut builder = gpui::PathBuilder::stroke(px(1.0));
                builder.move_to(start);
                builder.line_to(end);
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .w(px(11.0))
    .h(px(11.0))
}

/// Real, interactive coverage for the Windows/Linux caption buttons, driven through GPUI's
/// `TestAppContext`/`VisualTestContext` harness (a real window, real hit-testing, real click
/// dispatch).
///
/// ## Why only `close` gets a live-click test here
///
/// Minimise/maximise call the same real `Window::minimize_window`/`Window::zoom_window` the
/// macOS dot cluster already uses, but the test backend's `TestWindow: PlatformWindow` impl
/// (`vendor/zed/crates/gpui/src/platform/test/window.rs`) has both `fn minimize(&self) {
/// unimplemented!() }` and `fn zoom(&self) { unimplemented!() }` as deliberate panics
/// (`is_maximized` always returns `false` too, so there's no toggled state to assert against
/// even if the call didn't panic). A live click on either would crash the test process - so this
/// suite covers `close` only, the one caption button whose backing call
/// ([`Window::remove_window`], which just flips an internal `removed` flag) is implemented and
/// observable in the test harness. Minimise/maximise were instead verified manually against a
/// real running window.
///
/// Click coordinates are computed from the real, already-rendered window's own
/// `Window::viewport_size` rather than a hardcoded guess: the close button is the rightmost of
/// the three 44px-wide caption buttons pinned flush to the title bar's right edge, so its center
/// is always `(viewport_width - 22, 19)` regardless of the test display's own size.
#[cfg(test)]
mod caption_button_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Clicking the real close caption button on the Windows/Linux title bar variant actually
    /// calls the real `Window::remove_window` and closes the real window - not a mock.
    #[gpui::test]
    fn clicking_the_close_caption_button_closes_the_real_window(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        // Pin the Windows/Linux caption-button variant regardless of the real host OS this test
        // happens to run on, so the test is deterministic everywhere.
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            cx.windows().len(),
            1,
            "exactly one real window should be open before the click"
        );

        let viewport = cx.update(|window, _app| window.viewport_size());
        let close_button_center = gpui::point(viewport.width - px(22.0), px(19.0));
        cx.simulate_click(close_button_center, gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            cx.windows().len(),
            0,
            "clicking the real close caption button should have called the real \
             `Window::remove_window`, closing this window - the exact same real GPUI window- \
             control API the macOS dot cluster's own close dot already used"
        );
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

    /// The Edit menu's first row ("Undo") really calls the real
    /// `crate::root::worktree_history::AdeApp::perform_undo` - with nothing in
    /// [`AdeApp::undo_stack`] yet (a fresh test app), that's a real, honest "nothing to undo"
    /// status (Revision R10's own fix for the "looks actionable, silently does nothing" bug
    /// class - see that revision's build-log entry), not a silent no-op, and is exactly the
    /// observable effect this test asserts actually happened.
    #[gpui::test]
    fn edit_menu_undo_row_runs_the_real_undo_stack(cx: &mut TestAppContext) {
        let (app, cx) = open_windows_variant(cx);
        app.update(cx, |app, cx| {
            app.title_menu_open = Some(TitleMenu::Edit);
            cx.notify();
        });
        cx.run_until_parked();
        let bounds = app.read_with(cx, |app, _| {
            app.title_menu_button_bounds[TitleMenu::Edit.index()]
        });

        cx.simulate_click(first_row_click_point(bounds), gpui::Modifiers::none());
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
            app.select_settings_page(settings::SettingsPage::About, cx);
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
    /// (`crate::root::work_surface_render::AdeApp::render_footer_action_button`), reusing the
    /// exact same real [`AdeApp::request_discard_worktree`]/[`AdeApp::discard_confirm_armed`]
    /// this test drives through the menu row instead of the footer button. Needs a real,
    /// non-main worktree (`crate::root::worktree_history::AdeApp::request_discard_worktree`
    /// unconditionally refuses on the main worktree), so this sets one up the same way
    /// `crate::root::worktree_history::worktree_history_regression_tests::add_worktree` does.
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

    /// [`crate::root::work_surface_render::AdeApp::select_relative_session`] (the Session menu's
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
