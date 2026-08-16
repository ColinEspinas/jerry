//! The app's one menu popover, drawn once for every surface that opens one - see this folder's
//! own module docs for why there is exactly one of these.

use super::*;
use crate::menu::model::{self, MenuEntry, MenuRow};

/// Everything [`AdeApp::render_menu_overlay`] needs to paint one open menu: where it goes, what
/// is in it, and what running a row (or clicking away) really does.
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
