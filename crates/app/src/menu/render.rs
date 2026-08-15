//! The app's one menu popover, drawn once for every surface that opens one - see this folder's
//! own module docs for why there is exactly one of these.

use super::*;
use crate::menu::model::{self, MenuEntry, MenuRow};

/// Everything [`AdeApp::render_menu_overlay`] needs to paint one open menu: where it goes, what
/// is in it, and what running a row (or clicking away) really does.
///
/// `A` is the opening surface's own action payload - `crate::sidebar::context_menu::MenuAction`,
/// `crate::rail::menu::RailMenuAction`. The two callbacks are plain `fn` pointers rather than
/// boxed closures on purpose: a menu row's handler is always "run this surface's dispatcher with
/// this row's action", never a capture of per-row state, and a `fn` keeps every row's handler
/// identical instead of one allocation per row per frame.
pub(crate) struct MenuOverlay<A: Copy + 'static> {
    /// This menu's element-id prefix - the panel is `<id>`, its scrim `<id>-scrim`, and each row
    /// `<id>-<label>`. Also the `debug_selector` every real test resolves the menu through.
    pub(crate) id: &'static str,
    /// The already-clamped, window-space top-left corner (`crate::menu::model`'s
    /// [`model::clamp_menu_origin`] for a right-click, [`model::anchor_menu_below_button`] for a
    /// button-anchored one). Resolved once, at open time, off the real click and the real
    /// `Window::bounds()` - so a menu near a window edge is repositioned once, not re-solved (and
    /// possibly moved) on every frame it is open.
    pub(crate) origin_x: f32,
    pub(crate) origin_y: f32,
    /// Exactly the rows to paint, dividers included.
    pub(crate) rows: Vec<MenuRow<A>>,
    /// Runs one row's action. The row's own click handler closes nothing itself - dispatchers
    /// decide whether the menu stays open (a two-click confirmation row does; everything else
    /// closes).
    pub(crate) on_pick: fn(&mut AdeApp, A, &mut Window, &mut Context<AdeApp>),
    /// Closes this menu - the scrim's click-away, and a right-click landing on the scrim.
    pub(crate) on_dismiss: fn(&mut AdeApp, &mut Context<AdeApp>),
}

impl AdeApp {
    /// One open menu: a full-window occluding scrim whose `on_click` dismisses, plus an
    /// absolutely-positioned panel that stops that click from bubbling.
    ///
    /// Zed's own `ui::ContextMenu` (`vendor/zed/crates/ui/src/components/context_menu.rs`) is not
    /// reachable from here: it lives in Zed's `ui` crate, and this workspace deliberately depends
    /// on `gpui`/`gpui_platform` only. This is the same real scrim + panel shape
    /// `crate::work_surface::render::AdeApp::render_plus_menu` established for this app's first
    /// popover, with the two differences the file tree's menu had to add, both forced by a real
    /// reported bug:
    ///
    /// ## The scrim genuinely blocks what is behind it
    ///
    /// `.occlude()` (`gpui::InteractiveElement::occlude`, which sets
    /// `HitboxBehavior::BlockMouse` - `vendor/zed/crates/gpui/src/window.rs`'s `hit_test` stops
    /// walking hitboxes at the first one carrying it) is what makes this a real modal layer
    /// rather than a decorative one. Without it the scrim's `on_click` fired *and* the row
    /// underneath it took the same click - so dismissing the menu also opened whatever was under
    /// the cursor - and every row under the pointer still painted its `:hover` fill and its
    /// tooltip, because those read `Hitbox::is_hovered` directly and never consulted the click
    /// handlers at all. A `cx.stop_propagation()` in the scrim's own handler could only ever have
    /// fixed the click half; hover styling is not an event.
    ///
    /// ## It starts below the title bar
    ///
    /// A full-window occluding scrim swallows the window's own close/minimise/maximise caption
    /// buttons and the title bar's drag region, so the window could not be closed or moved while
    /// a menu was up. Reproduced against the real caption button by the file-tree menu's own
    /// adversarial audit. [`MenuOverlay::origin_y`] is window-space, so the panel subtracts
    /// `theme::band::TITLE_BAR` to place itself inside a scrim that starts there.
    pub(crate) fn render_menu_overlay<A: Copy + 'static>(
        &self,
        menu: MenuOverlay<A>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let MenuOverlay {
            id,
            origin_x,
            origin_y,
            rows,
            on_pick,
            on_dismiss,
        } = menu;
        let macos = self.window_controls_style().is_macos();

        div()
            .id(gpui::SharedString::from(format!("{id}-scrim")))
            .absolute()
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .occlude()
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                on_dismiss(this, cx);
            }))
            // A right-click on the scrim must dismiss too - otherwise the next right-click
            // anywhere would land on the scrim and do nothing at all, which reads as a frozen
            // app rather than as a menu that is still open.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, _window, cx| {
                    on_dismiss(this, cx);
                }),
            )
            .child(
                menu_popover_chrome(
                    div()
                        .id(gpui::SharedString::from(id))
                        .debug_selector(move || id.to_string())
                        .absolute()
                        .left(px(origin_x))
                        .top(px(origin_y) - theme::band::TITLE_BAR)
                        .w(px(model::MENU_WIDTH))
                        .py(px(model::MENU_VERTICAL_PADDING / 2.0)),
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .children(rows.into_iter().map(|row| match row {
                    MenuRow::Item(entry) => self.render_menu_row(id, entry, macos, on_pick, cx),
                    MenuRow::Separator => render_menu_group_divider(),
                })),
            )
            .into_any_element()
    }

    /// One menu row. A disabled row is still drawn (so the menu's shape doesn't jump between
    /// right-clicks) but carries no click handler at all - not a handler that returns early,
    /// which would be a row that looks clickable and silently isn't.
    ///
    /// Speaks this app's own established dropdown-row language rather than one invented here:
    /// every value below is `crate::work_surface::render::render_dropdown_menu_row`'s - the row
    /// shared by the tab strip's `+` menu and the title bar's File/Edit/View/Agent/Help menus -
    /// at the 24px height and 206px width `STAGE-A-CHANGELOG.md` §4t specifies for this
    /// component. That function is deliberately *not* reused: its row is a fixed chip + label +
    /// sub-label + keycap quad, and a context menu has no honest chip glyph or secondary text for
    /// two of those four slots.
    ///
    /// A **destructive** row is `theme::status::FAIL` on a
    /// `theme::surface::MENU_ROW_HOVER_DESTRUCTIVE` hover, so a destructive click is never
    /// visually identical to `Copy path` - in either its resting or its hovered state, which a
    /// shared neutral hover would have made it. §4t writes that pair as "destructive rows in
    /// `#c4726d` on a `#2a1719` hover"; the hover is exactly that literal, and the *label* keeps
    /// this app's own already-shipped destructive menu-row tint (`status::FAIL`, `#e0625c`, what
    /// the file tree's `Delete` row has always been) rather than moving every existing
    /// destructive row one shade for a two-hex difference. `#c4726d` is this app's
    /// `theme::button::DANGER_FG` - its destructive *button* pair, not its menu-row accent.
    ///
    /// The **keycap** is resolved through `crate::keymap::resolve_combo` from the entry's own
    /// binding spec, never a hard-coded glyph string, so a Ctrl/⌘ mismatch cannot be introduced
    /// here and a keycap can only ever name a real, registered binding
    /// (`crate::default_key_bindings`).
    fn render_menu_row<A: Copy + 'static>(
        &self,
        id_prefix: &'static str,
        entry: MenuEntry<A>,
        macos: bool,
        on_pick: fn(&mut AdeApp, A, &mut Window, &mut Context<AdeApp>),
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let action = entry.action;
        let keycaps = entry
            .keystroke_spec
            .map(|spec| keymap::resolve_combo(spec, macos))
            .unwrap_or_default();
        let color = if !entry.enabled {
            theme::text::GHOSTER
        } else if entry.destructive {
            theme::status::FAIL
        } else {
            theme::text::HEADING
        };
        let row_id = format!("{id_prefix}-{}", entry.label);
        let selector = row_id.clone();

        let mut row = div()
            .id(gpui::SharedString::from(row_id))
            .debug_selector(move || selector.clone())
            .flex()
            .items_center()
            .justify_between()
            .gap(px(9.0))
            .w_full()
            .h(px(model::MENU_ROW_HEIGHT))
            .px(px(10.0))
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(self.ui_text_size(11.5))
            .text_color(color)
            .children(entry.glyph.map(|icon| {
                crate::icons::IconRow::new(
                    &self.settings.icon_pack,
                    crate::icons::IconSize::MenuRow,
                )
                .draw(icon, color)
            }))
            .child(div().flex_1().min_w_0().truncate().child(entry.label))
            .child(render_keycap_row(&keycaps, KeycapSize::Hint));

        if entry.enabled {
            row = row
                .cursor_pointer()
                .hover(move |el| {
                    el.bg(if entry.destructive {
                        theme::surface::MENU_ROW_HOVER_DESTRUCTIVE
                    } else {
                        theme::surface::MENU_ROW_HOVER
                    })
                })
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    on_pick(this, action, window, cx);
                }));
            if let Some(hint) = entry.tooltip {
                row = row.tooltip(text_tooltip(hint));
            }
        } else {
            row = row.cursor_default();
            // A disabled row's *reason* outranks its hint: the hint describes what the row would
            // do, which is exactly the thing that is not going to happen.
            if let Some(reason) = entry.disabled_reason.or(entry.tooltip) {
                row = row.tooltip(text_tooltip(reason));
            }
        }

        row.into_any_element()
    }
}
