use super::*;

/// A themed, single-line message used for every Zone 3 empty/loading/error state (the file
/// tree's and the Changes list's alike) - one real, consistent look instead of each call site
/// improvising its own.
pub(super) fn render_sidebar_message(text: String, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .p(px(10.0))
        .font(font(theme::font::MONO))
        .text_size(px(10.5))
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// The Changes row / diff toolbar's optional `new`/`del` tag pill.
pub(super) fn render_tag_pill(tag: ChangeTag) -> impl IntoElement {
    let style = changes::tag_style(tag);
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(theme::radius::CHIP)
        .bg(style.bg)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(style.fg)
        .child(style.label)
}

/// One keyboard-shortcut keycap, per `design_handoff_jerry_ade/README.md`'s "Keyboard
/// affordances" spec: 15 high, min-width 15, padding 0 4, radius 3, bg `#181c1f`, border 1px
/// `#272c31`, 9.5px/450 mono `#7d848b`.
pub(super) fn render_keycap(label: &'static str) -> impl IntoElement {
    div()
        .h(theme::band::KEYCAP)
        .min_w(theme::band::KEYCAP)
        .px(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius::CHIP)
        .bg(theme::surface::KEYCAP)
        .border_1()
        .border_color(theme::border::KEYCAP)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(theme::text::DIMMER)
        .child(label)
}

/// A `⌘` + letter keycap pair, e.g. the rail header's `⌘N`.
pub(super) fn render_keycap_pair(modifier: &'static str, key: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(3.0))
        .child(render_keycap(modifier))
        .child(render_keycap(key))
}

/// One footer-action keycap with the *button's own* tint (`design_handoff_jerry_ade/
/// README.md`'s "Keyboard affordances": "Inside a coloured button the cap goes transparent and
/// borrows the button's tint") - unlike [`render_keycap`] (the rail/tab-strip's always-neutral
/// keycaps), this one's colours vary per `crate::work_surface::ActionStyle` (see
/// `crate::work_surface::action_button_colors`).
pub(super) fn render_action_keycap(
    label: &'static str,
    fg: gpui::Rgba,
    border: gpui::Rgba,
) -> impl IntoElement {
    div()
        .flex_none()
        .h(theme::band::KEYCAP)
        .min_w(theme::band::KEYCAP)
        .px(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius::CHIP)
        .border_1()
        .border_color(border)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(fg)
        .child(label)
}
