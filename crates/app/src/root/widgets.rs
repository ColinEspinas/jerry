use super::*;

/// The two keycap sizes: `Standard` (primary shortcuts - the rail's `+`/⌘N, the status bar's
/// `⌘P`) and `Hint` (smaller, for hint-row contexts like footers and empty-state hint lists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeycapSize {
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
pub(crate) fn render_keycap_row(parts: &[String], size: KeycapSize) -> impl IntoElement {
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
pub(crate) fn render_hint_pair(
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
pub(crate) fn render_hint_row(
    pairs: impl IntoIterator<Item = impl IntoElement>,
) -> impl IntoElement {
    div().flex().items_center().gap(px(11.0)).children(pairs)
}

/// The environment chip: `WSL · <distro>` when this process is genuinely running inside WSL
/// (`crate::env_info::is_wsl`), else `local · <arch>` (`crate::env_info::local_arch`). A
/// parameterless, real-environment-reading widget so future call sites (status bar, Settings)
/// can reuse it rather than hand-copying a second chip.
pub(crate) fn render_env_chip() -> impl IntoElement {
    let (label, fg, bg, border): (String, gpui::Rgba, gpui::Rgba, gpui::Rgba) =
        if env_info::is_wsl() {
            let distro = env_info::wsl_distro_name().unwrap_or("WSL");
            (
                format!("WSL \u{b7} {distro}"),
                theme::env::WSL_FG.into(),
                theme::env::WSL_BG.into(),
                theme::env::WSL_BORDER.into(),
            )
        } else {
            (
                format!("local \u{b7} {}", env_info::local_arch()),
                theme::env::LOCAL_FG.into(),
                work_surface::TRANSPARENT,
                theme::env::LOCAL_BORDER.into(),
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
pub(crate) fn render_sidebar_message(text: String, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .p(px(10.0))
        .font(font(theme::font::MONO))
        .text_size(px(10.5))
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// The Changes row / diff toolbar's optional `new`/`del` tag pill.
pub(crate) fn render_tag_pill(tag: ChangeTag) -> impl IntoElement {
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
pub(crate) fn render_keycap(label: &'static str) -> impl IntoElement {
    render_keycap_sized(label, KeycapSize::Standard)
}

/// A themed row of already-resolved keycaps with the *button's own* tint - the colored-button
/// counterpart to [`render_keycap_row`], used by `crate::work_surface::state::FooterAction`'s
/// per-status action buttons (`Keep all ⌘⏎`, `Interrupt ⌃C`, ...), whose colours vary per
/// `crate::work_surface::state::ActionStyle`.
pub(crate) fn render_action_keycap_row(
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
/// `lsp::completion_popup`'s own completion popup uses). Backs [`text_tooltip`]; see that function's
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
pub(crate) fn text_tooltip(
    text: impl Into<gpui::SharedString>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_window, cx| cx.new(|_| TextTooltip { text: text.clone() }).into()
}

impl AdeApp {
    /// A themed, blinking 1.5×14 caret bar for a simple single-line pseudo-input - a query/
    /// filter field backed by a plain, append/backspace-only `String` (not a real cursor-
    /// position-aware `EditBuffer`), so it's always a `Line`-style bar at the real end of the
    /// typed text, never affected by [`settings::store::CaretStyle`] (which only ever describes
    /// the code editor's own real, mid-string caret - see that type's own docs).
    ///
    /// GitHub issue #27's "audit every input/contenteditable in the app for missing carets and
    /// fix": [`Self::render_rail_filter_row`] and [`Self::render_settings_keymap_filter_row`]
    /// had *no* caret element at all before this (confirmed by reading both render functions -
    /// just the typed query or a placeholder, no insertion-point indicator whatsoever). Shared
    /// here rather than duplicated per call site, and rather than reusing `crate::palette::
    /// render::AdeApp::render_palette_caret` verbatim: that one is deliberately a two-position
    /// (`margin_right`/`margin_left`) variant for its own empty-vs-typed placeholder placement
    /// (see its own docs) that these simpler, always-after-the-text fields don't need.
    /// Blinks via the same shared [`crate::root::caret_blink`] loop the code editor/palette/tree
    /// rename field use - see that module's own docs for the whole mechanism.
    ///
    /// GitHub issue #45 ("Input blink only on focused input or file") plus a live follow-up
    /// report: every real call site used to place this caret unconditionally *after* whichever
    /// child rendered next in document order, which for an empty field is the *placeholder*
    /// text - a caret visually glued to the end of "filter worktrees and agents" instead of at
    /// the real cursor position (0, i.e. before any text at all). `selector` lets each call site
    /// give this element its own real, measurable `debug_selector` (mirroring
    /// [`crate::palette::render::AdeApp::render_palette_caret`]'s own `"palette-caret"`) so a
    /// real interaction test can assert *where* it painted, not just that a doc comment claims
    /// the right position - see `rail::render::rail_filter_caret_tests`,
    /// `settings::render::settings_keymap_filter_caret_tests`,
    /// `graph_view::render::graph_focus_tests`, and `root::new_file::new_file_caret_tests` for
    /// that coverage.
    ///
    /// `focus_handle` is `caret_blink_visible`'s missing other half (GitHub issue #45's own
    /// title, re-audited): every one of these simple inputs used to paint from
    /// [`AdeApp::caret_blink_visible`] alone, with no check of its *own* focus at all. Since that
    /// flag is one shared bool driven by whichever caret-bearing handle is currently focused
    /// (`crate::root::caret_blink`'s own docs), and these simple inputs are never modal - the
    /// rail filter row, for one, is mounted on screen at the same time as the code editor - an
    /// *unfocused* simple input kept blinking in exact sync whenever the user was simply typing
    /// somewhere else entirely. Painted through a real `gpui::canvas` (the same idiom
    /// `crate::code_surface::editing::caret_paint_quad`'s own caller uses) rather than a plain
    /// `.when(...).bg(...)` specifically so `focus_handle.is_focused(window)` can be read at
    /// paint time - `Window` isn't threaded through every one of this helper's several render-
    /// tree ancestors, and a canvas's paint closures always receive it regardless of what the
    /// surrounding `render_*` call chain's own signatures carry. Mirrors `caret_paint_quad`'s own
    /// rule exactly: focused *and* blink-visible paints solid, focused-but-blinked-off paints
    /// nothing, unfocused paints a dim, non-blinking caret (`theme::syntax::
    /// CARET_UNFOCUSED_OPACITY` - the same constant the code editor's own caret uses, not a
    /// second, independently-chosen value for this surface) rather than vanishing outright -
    /// consistent with every other caret-bearing surface's unfocused treatment in this app.
    pub(crate) fn render_simple_input_caret(
        &self,
        selector: &'static str,
        focus_handle: &FocusHandle,
    ) -> impl IntoElement {
        let caret_blink_visible = self.caret_blink_visible;
        let focus_handle = focus_handle.clone();
        div()
            .flex_none()
            .w(px(1.5))
            .h(px(14.0))
            .debug_selector(move || selector.to_string())
            .child(
                gpui::canvas(
                    move |bounds, window, _cx| {
                        let is_focused = focus_handle.is_focused(window);
                        simple_input_caret_opacity(is_focused, caret_blink_visible).map(|opacity| {
                            gpui::fill(bounds, theme::term::CURSOR.resolve().opacity(opacity))
                        })
                    },
                    |_bounds, quad, window, _cx| {
                        if let Some(quad) = quad {
                            window.paint_quad(quad);
                        }
                    },
                )
                .size_full(),
            )
    }
}

/// [`AdeApp::render_simple_input_caret`]'s paint decision, pulled out as a pure function so it's
/// directly unit-testable without a real GPUI window/focus simulation - mirrors
/// `crate::code_surface::editing::caret_paint_quad`'s own three-way rule (focused-and-blinking
/// solid / focused-and-blinked-off nothing / unfocused dim-and-steady) exactly, just returning an
/// opacity instead of a ready-made [`gpui::PaintQuad`] since this caller's color/bounds aren't
/// available outside the canvas paint closure.
fn simple_input_caret_opacity(is_focused: bool, blink_visible: bool) -> Option<f32> {
    if is_focused && !blink_visible {
        None
    } else if is_focused {
        Some(1.0)
    } else {
        Some(theme::syntax::CARET_UNFOCUSED_OPACITY)
    }
}

/// The fill behind a centered modal panel - the file tree's delete confirmation
/// (`crate::sidebar::render::AdeApp::render_tree_delete_confirm`) and the New file prompt
/// (`crate::root::new_file::AdeApp::render_new_file_prompt`), this app's only two.
///
/// Derived from `theme::surface::SCRIM`, the design handoff's own scrim colour
/// (`design_handoff_jerry_ade/revision/README.md`: "Scrim rgba(6,7,8,.62)"), rather than the raw
/// `gpui::black()` both modals used to hard-code - a literal colour rather than a token, which is
/// exactly what this app's theming discipline exists to keep out. The alpha stays at the 0.35
/// both modals already used: a small centered dialog does not dim as hard as the palette, which
/// replaces the entire workspace and uses the designed 0.62.
pub(crate) fn modal_scrim_bg() -> gpui::Rgba {
    theme::surface::SCRIM.resolve().opacity(0.35)
}

/// One button in a centered modal's action row - `label`, tinted `theme::button::DANGER_FG` when
/// `destructive` (with `DANGER_FG_HOVER` on hover, this app's established destructive-control
/// pair) and `theme::text::BODY` otherwise.
///
/// Shape is `crate::work_surface::render::render_footer_action_button`'s: `h(23)`, `px(10)`,
/// `theme::radius::BUTTON` - the 4px "buttons" radius the design handoff calls for, not the 3px
/// `radius::CHIP` meant for chips and keycaps. Carries a real hover fill; the delete
/// confirmation's two buttons shipped with none at all, so the only clickable controls in that
/// dialog were also the only ones in the app that gave no feedback under the pointer. The caller
/// attaches its own `.on_click`.
pub(crate) fn render_modal_button(
    id: &'static str,
    label: impl Into<gpui::SharedString>,
    destructive: bool,
) -> gpui::Stateful<gpui::Div> {
    let (color, hover_color) = if destructive {
        (theme::button::DANGER_FG, theme::button::DANGER_FG_HOVER)
    } else {
        (theme::text::BODY, theme::text::PRIMARY)
    };
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .cursor_pointer()
        .flex()
        .items_center()
        .h(px(23.0))
        .px(px(10.0))
        .rounded(theme::radius::BUTTON)
        .bg(theme::surface::SEGMENT_TRACK)
        .hover(move |el| el.bg(theme::surface::ROW_HOVER_ALT).text_color(hover_color))
        .font(font(theme::font::SANS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(10.5))
        .text_color(color)
        .child(label.into())
}

/// One group divider's whole vertical footprint, in px: the 1px rule of
/// [`render_menu_group_divider`] plus its 4px margins top and bottom. Exported because the two
/// menus that have to *measure* themselves - `crate::title_bar::menu`'s row-index math and
/// `crate::sidebar::context_menu::menu_height`'s window-edge clamp - both read the real number
/// rather than each restating `1 + 4 + 4`.
pub(crate) const MENU_GROUP_DIVIDER_HEIGHT: Pixels = px(9.0);

/// A thin 1px rule between two groups of rows inside a dropdown or context popover -
/// [`theme::border::DIVIDER`], the same token the title bar's own left-cluster rule uses, inset
/// `mx(10.0)` so it lines up flush with a menu row's own `px(10.0)` label column.
///
/// The app's one in-menu divider, shared by the title bar's File/Edit/View/Agent/Help menus
/// (where it was first built, as `title_bar::menu`'s `render_title_menu_divider`) and the file
/// tree's right-click context menu, whose groups (GitHub issue #19 §1) shipped with no visual
/// separation at all. See [`MENU_GROUP_DIVIDER_HEIGHT`] for the height both of those menus
/// measure against.
pub(crate) fn render_menu_group_divider() -> gpui::AnyElement {
    // The margins are *derived* from the total rather than written alongside it, so the constant
    // the two menus measure themselves against and the element they are measuring cannot
    // disagree - which is the whole reason the constant is public in the first place.
    let margin = (MENU_GROUP_DIVIDER_HEIGHT - MENU_GROUP_DIVIDER_RULE) / 2.0;
    div()
        .h(MENU_GROUP_DIVIDER_RULE)
        .mx(px(10.0))
        .my(margin)
        .bg(theme::border::DIVIDER)
        .into_any_element()
}

/// The divider's own rule thickness - the rest of [`MENU_GROUP_DIVIDER_HEIGHT`] is its margins.
const MENU_GROUP_DIVIDER_RULE: Pixels = px(1.0);

#[cfg(test)]
mod simple_input_caret_opacity_tests {
    use super::simple_input_caret_opacity;

    /// GitHub issue #45's own title, taken literally and pinned at the unit level: a focused
    /// input's caret must actually blink (opacity toggles with `blink_visible`), and an
    /// unfocused input's caret must never depend on the shared blink flag at all - the exact
    /// cross-input bleed the issue reported, where an unfocused simple input kept blinking in
    /// sync with whichever *other* caret-bearing surface was actually focused.
    #[test]
    fn only_a_focused_input_s_opacity_depends_on_the_shared_blink_flag() {
        assert_eq!(
            simple_input_caret_opacity(true, true),
            Some(1.0),
            "focused and blink-on must paint solid"
        );
        assert_eq!(
            simple_input_caret_opacity(true, false),
            None,
            "focused and blink-off must paint nothing - the real blink"
        );
        assert_eq!(
            simple_input_caret_opacity(false, true),
            simple_input_caret_opacity(false, false),
            "an unfocused input's opacity must be identical regardless of the shared blink \
             flag's current phase - this is the exact bug: an unfocused input must never blink \
             just because some other input is focused and blinking"
        );
    }

    #[test]
    fn an_unfocused_input_paints_dim_not_invisible() {
        assert_eq!(
            simple_input_caret_opacity(false, true),
            Some(crate::theme::syntax::CARET_UNFOCUSED_OPACITY),
            "unfocused must still show a real, dim, non-blinking caret - matching every other \
             caret-bearing surface's unfocused treatment in this app - not vanish outright"
        );
    }
}
