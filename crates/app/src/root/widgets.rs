use super::*;

/// The two real keycap sizes `design_handoff_jerry_ade/README.md`'s "Keyboard affordances"
/// section defines - `Standard` (the original, already-shipped size: primary shortcuts like
/// the rail's `+`/⌘N, the status bar's `⌘K`) and the new `Hint` (`CHANGELOG.md`'s 2026-07-29
/// entry, change 2: "hint size 14-high, padding 0 3.5, bg `#15181a`, border `#23272b`, 9px
/// `#6b7178`" - hint-row contexts like footers and empty-state hint lists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeycapSize {
    Standard,
    Hint,
}

/// One keyboard-shortcut keycap, sized per [`KeycapSize`] - `design_handoff_jerry_ade/
/// README.md`'s "Keyboard affordances" spec for both real sizes. `label` takes
/// `impl Into<SharedString>` (not just `&'static str`) so it accepts both a literal caption
/// (`"F12"`, `"esc"`) and an owned, already-platform-resolved glyph from
/// `crate::keymap::resolve_combo` (`String`) without a caller-side allocation dance.
fn render_keycap_sized(label: impl Into<gpui::SharedString>, size: KeycapSize) -> impl IntoElement {
    let (h, pad_x, bg, border, font_size, color) = match size {
        KeycapSize::Standard => (
            theme::band::KEYCAP,
            px(4.0),
            theme::surface::KEYCAP,
            theme::border::KEYCAP,
            9.5,
            theme::text::DIMMER,
        ),
        KeycapSize::Hint => (
            theme::band::KEYCAP_HINT,
            px(3.5),
            theme::surface::KEYCAP_HINT,
            theme::border::KEYCAP_HINT,
            9.0,
            theme::text::FAINT,
        ),
    };
    div()
        .h(h)
        .min_w(h)
        .px(pad_x)
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius::CHIP)
        .bg(bg)
        .border_1()
        .border_color(border)
        .font(font(theme::font::MONO))
        .text_size(px(font_size))
        .text_color(color)
        .child(label.into())
}

/// A themed row of already-resolved keycaps - `design_handoff_jerry_ade/README.md`'s "Keyboard
/// affordances": "Each resolved part gets its own keycap; a combo is a 3px-gap row of them" for
/// [`KeycapSize::Standard`]. That 3px figure is specific to the standard size, not a shared
/// constant: every real [`KeycapSize::Hint`] row in `Jerry.dc.html` (`termHints`/`diffHints`/
/// `cmpHints`/`conflictHints`/`changesHints`/`palHints` - the exact real precedents
/// [`render_hint_pair`], below, renders through) instead uses a 2px gap (e.g. its own
/// `<div style="display:flex;gap:2px">` wrapping each `height:14px` hint keycap group) - a
/// separate, deliberately smaller figure from the 3px the standalone 14px-tall keycap groups
/// elsewhere in the mockup (the `+` menu popover's hint keycaps, the merge conflict "Take
/// left/right/both" buttons) use, neither of which this function's only real caller
/// ([`render_hint_pair`]) renders. `parts` must already be real, platform-resolved glyphs
/// (`crate::keymap::resolve_combo`'s output) - this function only lays them out, it never does
/// glyph substitution itself, so it can never be the place a literal `⌘` sneaks back into
/// calling code.
pub(super) fn render_keycap_row(parts: &[String], size: KeycapSize) -> impl IntoElement {
    let gap = match size {
        KeycapSize::Standard => px(3.0),
        KeycapSize::Hint => px(2.0),
    };
    div().flex().items_center().gap(gap).children(
        parts
            .iter()
            .map(|part| render_keycap_sized(part.clone(), size)),
    )
}

/// One `[keycaps] label` hint pair - `design_handoff_jerry_ade/CHANGELOG.md`'s 2026-07-29
/// entry, change 2: "Hint rows are now `[keycaps] label` pairs, ..., label 10px Plex **Sans**
/// `#4a5057`" (`theme::text::PATH` is the same `#4a5057` token). `keys` is `&[]` for a hint
/// with no real binding behind it (e.g. a plain result count) - in that case only the label
/// renders, no empty keycap row.
pub(super) fn render_hint_pair(
    keys: &[String],
    label: impl Into<gpui::SharedString>,
) -> impl IntoElement {
    let mut row = div().flex().items_center().gap(px(5.0));
    if !keys.is_empty() {
        row = row.child(render_keycap_row(keys, KeycapSize::Hint));
    }
    row.child(
        div()
            .font(font(theme::font::SANS))
            .text_size(px(10.0))
            .text_color(theme::text::PATH)
            .child(label.into()),
    )
}

/// A hint row: several [`render_hint_pair`]s laid out with the design's own 11px gap
/// (`CHANGELOG.md`'s 2026-07-29 entry, change 2: "Hint rows are now `[keycaps] label` pairs,
/// 11px gap between pairs").
pub(super) fn render_hint_row(
    pairs: impl IntoIterator<Item = impl IntoElement>,
) -> impl IntoElement {
    div().flex().items_center().gap(px(11.0)).children(pairs)
}

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

/// One always-neutral, single-literal keyboard-shortcut keycap - `design_handoff_jerry_ade/
/// README.md`'s "Keyboard affordances" standard-size spec, via [`render_keycap_sized`]. For a
/// real, platform-resolved combo (anything that could contain a `mod`/`alt`/`ctrl`/`shift`/
/// `enter`/`esc`/`tab`/`bksp` token), use [`render_keycap_row`] with `crate::keymap::
/// resolve_combo`'s output instead - this helper is only for the handful of call sites that
/// render one already-final, platform-invariant literal (`"F12"`, the settings-close `"esc"`
/// affordance rendered as a UI label rather than a real bound keystroke hint).
pub(super) fn render_keycap(label: &'static str) -> impl IntoElement {
    render_keycap_sized(label, KeycapSize::Standard)
}

/// A themed row of already-resolved keycaps with the *button's own* tint
/// (`design_handoff_jerry_ade/README.md`'s "Keyboard affordances": "Inside a coloured button
/// the cap goes transparent and borrows the button's tint") - the colored-button counterpart to
/// [`render_keycap_row`], used by `crate::work_surface::FooterAction`'s per-status action
/// buttons (`Keep all ⌘⏎`, `Interrupt ⌃C`, ...), whose colours vary per
/// `crate::work_surface::ActionStyle` (see `crate::work_surface::action_button_colors`).
pub(super) fn render_action_keycap_row(
    parts: &[String],
    fg: gpui::Rgba,
    border: gpui::Rgba,
) -> impl IntoElement {
    div().flex().items_center().gap(px(3.0)).children(
        parts
            .iter()
            .map(move |part| render_action_keycap(part.clone(), fg, border)),
    )
}

/// One footer-action keycap with the *button's own* tint - see [`render_action_keycap_row`]'s
/// docs. `label` takes `impl Into<SharedString>` for the same reason [`render_keycap_sized`]'s
/// does (an owned, already-resolved glyph, not just a `&'static str` literal).
fn render_action_keycap(
    label: impl Into<gpui::SharedString>,
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
        .child(label.into())
}
