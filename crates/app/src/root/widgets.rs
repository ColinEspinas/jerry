use super::*;

/// The two keycap sizes: `Standard` (primary shortcuts - the rail's `+`/⌘N, the status bar's
/// `⌘K`) and `Hint` (smaller, for hint-row contexts like footers and empty-state hint lists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeycapSize {
    Standard,
    Hint,
}

/// One keyboard-shortcut keycap, sized per [`KeycapSize`]. `label` takes `impl
/// Into<SharedString>` (not just `&'static str`) so it accepts both a literal caption (`"F12"`,
/// `"esc"`) and an owned, already-platform-resolved glyph from `crate::keymap::resolve_combo`
/// (`String`).
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

/// A themed row of already-resolved keycaps: each part gets its own keycap, with a 3px gap for
/// [`KeycapSize::Standard`] or 2px for [`KeycapSize::Hint`]. `parts` must already be
/// platform-resolved glyphs (`crate::keymap::resolve_combo`'s output) - this function only lays
/// them out, it never does glyph substitution itself, so it can never be the place a literal `⌘`
/// sneaks back into calling code.
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

/// One `[keycaps] label` hint pair. `keys` is `&[]` for a hint with no real binding behind it
/// (e.g. a plain result count) - in that case only the label renders, no empty keycap row.
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

/// A hint row: several [`render_hint_pair`]s laid out with an 11px gap.
pub(super) fn render_hint_row(
    pairs: impl IntoIterator<Item = impl IntoElement>,
) -> impl IntoElement {
    div().flex().items_center().gap(px(11.0)).children(pairs)
}

/// The environment chip: `WSL · <distro>` when this process is genuinely running inside WSL
/// (`crate::env_info::is_wsl`), else `local · <arch>` (`crate::env_info::local_arch`). A
/// parameterless, real-environment-reading widget so future call sites (status bar, Settings)
/// can reuse it rather than hand-copying a second chip.
pub(super) fn render_env_chip() -> impl IntoElement {
    let (label, fg, bg, border) = if env_info::is_wsl() {
        let distro = env_info::wsl_distro_name().unwrap_or("WSL");
        (
            format!("WSL \u{b7} {distro}"),
            theme::env::WSL_FG,
            theme::env::WSL_BG,
            theme::env::WSL_BORDER,
        )
    } else {
        (
            format!("local \u{b7} {}", env_info::local_arch()),
            theme::env::LOCAL_FG,
            work_surface::TRANSPARENT,
            theme::env::LOCAL_BORDER,
        )
    };

    div()
        .flex_none()
        .h(px(17.0))
        .px(px(6.0))
        .rounded(theme::radius::CHIP)
        .border_1()
        .border_color(border)
        .bg(bg)
        .flex()
        .items_center()
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(fg)
        .child(label)
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

/// One always-neutral, single-literal keyboard-shortcut keycap. For a platform-resolved combo
/// (anything that could contain a `mod`/`alt`/`ctrl`/`shift`/`enter`/`esc`/`tab`/`bksp` token),
/// use [`render_keycap_row`] with `crate::keymap::resolve_combo`'s output instead - this helper
/// is only for call sites that render one already-final, platform-invariant literal (`"F12"`).
pub(super) fn render_keycap(label: &'static str) -> impl IntoElement {
    render_keycap_sized(label, KeycapSize::Standard)
}

/// A themed row of already-resolved keycaps with the *button's own* tint - the colored-button
/// counterpart to [`render_keycap_row`], used by `crate::work_surface::FooterAction`'s
/// per-status action buttons (`Keep all ⌘⏎`, `Interrupt ⌃C`, ...), whose colours vary per
/// `crate::work_surface::ActionStyle`.
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

/// One footer-action keycap with the *button's own* tint - see [`render_action_keycap_row`].
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

/// A plain, real GPUI tooltip view - just the given text, styled to match this app's other
/// small popovers (`theme::surface::POPOVER`/`theme::border::POPOVER`, the same tokens
/// `root::completions`'s own completion popup uses). Backs [`text_tooltip`]; see that function's
/// own docs for why this exists.
struct TextTooltip {
    text: gpui::SharedString,
}

impl gpui::Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(420.0))
            .px(px(8.0))
            .py(px(6.0))
            .rounded(theme::radius::CARD_SM)
            .bg(theme::surface::POPOVER)
            .border_1()
            .border_color(theme::border::POPOVER)
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::PRIMARY)
            .child(self.text.clone())
    }
}

/// A real `.tooltip(...)` callback (`vendor/zed/crates/gpui/src/elements/div.rs`'s
/// `InteractiveElement::tooltip`: `impl Fn(&mut Window, &mut App) -> AnyView`) that shows
/// `text` verbatim, unstyled beyond [`TextTooltip`]'s own plain popover chrome - for the real,
/// load-bearing but potentially long status text this app now truncates on screen (an audit
/// found long undo/redo/keep/discard error text - a full stash id, two full 40-character commit
/// shas - rendered with no truncation *or* tooltip at all, in a narrow rail-footer div). Pairs
/// with `.truncate()` on the same element: the on-screen text is cut short with an ellipsis, and
/// this tooltip carries the real, untruncated text on hover.
pub(super) fn text_tooltip(
    text: impl Into<gpui::SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_window, cx| cx.new(|_| TextTooltip { text: text.clone() }).into()
}
