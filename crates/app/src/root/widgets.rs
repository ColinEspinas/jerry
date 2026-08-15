use super::*;
use crate::icon_pack;
use crate::work_surface::agents::ProcessKind;

/// **The** disclosure caret - the one control, at one size, that expands or collapses a list
/// anywhere in this app: a rail worktree row, a Changes-panel section header, a group header.
///
/// `STAGE-A-CHANGELOG.md` §4p is explicit that this is a single control and drew the line for it:
///
/// > Every disclosure caret is one control. […] All five now match the rail: **10px `#8b9197`**,
/// > span width 9 -> 11 so the larger glyph does not crowd its label.
/// >
/// > The line drawn: a **disclosure caret** (expands or collapses a list - rail rows, panel
/// > sections, group headers) gets that treatment. A **dropdown chevron** bound to a button or
/// > chip (the `+` tab launcher, the rebase action chips) is a different control and stays at
/// > 8-8.5px - it is a hint on a target that is already big enough.
///
/// So this is a function rather than a copied snippet: two call sites that must not drift are one
/// call site. `text_size` is the caller's already-scaled `AdeApp::ui_text_size(10.0)` (see
/// `crate::sidebar::render::render_changes_footer`'s own docs for why scaling is passed in rather
/// than computed here), and the caller owns the click handler - the caret is a glyph, not a button,
/// and every one of its call sites already has a larger clickable row or header around it.
///
/// The caller also owns the **colour**, and deliberately so: both call sites want the same
/// `#8b9197` ([`theme::changes::SECTION_CARET`] and [`theme::text::DIM`] are one value), but the
/// rail's caret also carries §4o's hover lift to `#c2c7cc`, armed on the 13x27 hit box around this
/// glyph. A `text_color` set *here* would win over that hover the same way a CSS `color` on the
/// child wins over one on the hovered parent, and the lift would silently do nothing. Inheriting
/// is what makes it work.
///
/// Deliberately *not* `crate::sidebar::render::render_tree_caret`: that one reserves its width for
/// a non-expandable **file** row so the tree's icon column stays aligned, which is a second job
/// this control does not have.
pub(crate) fn render_disclosure_caret(open: bool, text_size: Pixels) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(11.0))
        .flex()
        .items_center()
        .justify_center()
        .font(font(theme::font::MONO))
        .text_size(text_size)
        .child(if open { "\u{25be}" } else { "\u{25b8}" })
}

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

/// Git's own status letter for one file - `A`/`M`/`D` - in the fixed 9px, centred column
/// `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4j specifies, with the tooltip
/// that spells it out.
///
/// One function for all four sites that show it (the Uncommitted rows, the diff toolbar above a
/// file, the commit file list, and the review file list) rather than four hand-written spans, for
/// `REVISION-2026-08-14.md` §7 rule 7's reason: "a row of icons needs one shared optical box, not
/// one size per icon". The 9px box is what makes every filename in a list start on the same x -
/// which the word pills it replaced could not do, since they were absent on most rows and two
/// different widths on the rest.
///
/// This **replaced** `render_tag_pill`, and that function is gone rather than left beside it, per
/// §7 rule 5: "Replacing a control means deleting its old keys in the same edit - a key defined
/// twice is two specifications of one thing, and the reader cannot tell which is real."
///
/// Stateful (`.id(..)`) because the tooltip is: GPUI puts `tooltip` on
/// `StatefulInteractiveElement`, so an icon-only control that has to explain itself has to carry
/// an id. `id` is the caller's, since only the caller knows which file's row this letter is in.
pub(crate) fn render_status_letter(
    id: impl Into<gpui::ElementId>,
    letter: changes::StatusLetter,
    text_size: Pixels,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(9.0))
        .flex()
        .justify_center()
        .font(font(theme::font::MONO))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(text_size)
        .text_color(letter.color())
        .tooltip(text_tooltip(letter.tooltip()))
        .child(letter.glyph())
}

/// The Changes row's `committed` tag - a file that differs from the base branch only because a
/// real commit on this branch already holds that difference (`crate::sidebar::changes::
/// is_committed_clean`, GitHub issue #220).
///
/// Deliberately **not** a [`changes::StatusLetter`] variant: the letter is documented as git's
/// own report of what happened to the file, and this is orthogonal to it - a committed-clean file
/// can perfectly well also be an addition, and would then need both this and an `A`. That is the
/// same reason `crate::sidebar::render::render_moved_tag` is its own neutral chip rather than a
/// fourth letter, and this matches its look exactly so the row's two status-independent pills read
/// as one family.
pub(crate) fn render_committed_tag() -> impl IntoElement {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(theme::radius::CHIP)
        .bg(theme::surface::CHIP_NEUTRAL)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(theme::text::GHOST)
        .child("committed")
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

/// This app's standard clickable-row hover fill - just `.hover(|el| el.bg(fill))`. GitHub issue
/// #128 found eight rows across six files that each needed this and had each reimplemented the
/// same one-line wiring independently (several not at all, which was the bug); a single generic
/// helper is what keeps that from drifting apart again the way GitHub issue #129 later had to
/// clean up for a different set of copy-pasted tokens (menu bg/border/radius/shadow) - see that
/// issue's own `theme.rs` docs for the sibling problem. For a row that only wants this while some
/// condition holds (e.g. "not already the active tab"), wrap the call in the builder's own
/// `.when(cond, |el| hover_bg(el, fill))` rather than adding a second flag here - `.when` is
/// already this crate's one real "conditionally apply a transform" idiom.
pub(crate) fn hover_bg<E: InteractiveElement>(el: E, fill: impl Into<gpui::Fill>) -> E {
    let fill = fill.into();
    el.hover(move |style| style.bg(fill))
}

/// This app's "clickable keycap row" hover shape - a rounded chip-style fill, used by every
/// plain clickable row whose only content is a keycap (or keycap + short label) with no
/// surrounding padding box of its own: the settings panel's `esc`-to-close row and the status
/// bar's "commands / ⌘P" hint. See [`hover_bg`]'s own docs for why this is a shared helper
/// rather than copy-pasted per call site.
pub(crate) fn hover_keycap_row<E: InteractiveElement + Styled>(el: E) -> E {
    el.rounded(theme::radius::CHIP)
        .hover(|style| style.bg(theme::surface::ROW_HOVER_ALT))
}

/// The one real "floating dropdown/context-menu" chrome - background, border, radius, shadow -
/// every menu popover in the app now builds on (GitHub issue #129): the `+` menu, the title bar
/// menu, the file tree's context menu, the git graph's push/row menus, and the commit composer's
/// split-button menu. Before this, each of those six popovers hand-wrote the same five style
/// calls (`.bg(surface::PALETTE).border_1().border_color(border::POPOVER).rounded(radius::CARD)
/// .shadow(...)`) independently - real, live-caught drift (a follow-up audit found the commit
/// menu's own shadow had quietly drifted to a different blur/alpha with no real reason for the
/// difference) is exactly the failure mode a shared function, not a shared *value*, closes: a
/// seventh menu written by copying an existing one still risks copying a stale variant, but a
/// seventh menu built on this function can't drift from the other six at all.
///
/// `shadow` is the one real parameter, not a second flavor to copy-paste around: every popover
/// shares the same blur/alpha (`0.55`) inside [`theme::shadow::MENU`],
/// and only the `y` offset's sign genuinely differs, for the commit menu's own upward-opening
/// direction (see that constant's own docs).
///
/// Deliberately not used by the command palette (kept its own distinct chrome on purpose - GitHub
/// issue #129's own scope) or the LSP completion popup/hover card/plain tooltips (a real, mockup-
/// verified different recipe - see `crate::lsp::completion_popup`'s own module docs for why).
pub(crate) fn menu_popover_chrome<E: InteractiveElement + Styled>(
    el: E,
    shadow: (Pixels, Pixels, Pixels),
) -> E {
    let (shadow_x, shadow_y, shadow_blur) = shadow;
    el.bg(theme::surface::PALETTE)
        .border_1()
        .border_color(theme::border::POPOVER)
        .rounded(theme::radius::CARD)
        .shadow(vec![gpui::BoxShadow::new(
            shadow_x,
            shadow_y,
            gpui::black().opacity(0.55),
        )
        .blur_radius(shadow_blur)])
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
    /// rule exactly (GitHub issue #107): focused *and* blink-visible paints solid, anything else -
    /// focused-but-blinked-off, or simply unfocused - paints nothing at all.
    pub(crate) fn render_simple_input_caret(
        &self,
        selector: impl Into<gpui::SharedString>,
        focus_handle: &FocusHandle,
    ) -> impl IntoElement {
        let caret_blink_visible = self.caret_blink_visible;
        let focus_handle = focus_handle.clone();
        // `impl Into<SharedString>` rather than `&'static str`: a caret that belongs to a *row of
        // a list* has one selector per row (`diff-note-3-caret`), and `debug_bounds` is a map
        // keyed by selector - two rows sharing one name would collapse to whichever painted last,
        // which is exactly the ambiguity `only_the_card_being_typed_into_paints_a_caret` has to be
        // able to see through.
        let selector = selector.into();
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

    /// **The** caret+text row every hand-rolled single-line input in this app should be built
    /// from - the complete structure, not just the caret glyph.
    ///
    /// ## Why this exists (a live report, three times over)
    ///
    /// [`Self::render_simple_input_caret`] has been shared since GitHub issue #27, but it only
    /// ever returned the 1.5px blinking bar. Every one of the app's ~nine simple inputs still
    /// hand-assembled the *row around it* - where `flex_1`/`min_w_0` go, whether the row centres
    /// its items, whether the caret is drawn before the placeholder or after the text - and that
    /// surrounding structure is exactly what kept being got wrong, one field at a time, with a
    /// separate live bug report each time.
    ///
    /// The failure is always the same one, and it is not obvious by reading a single call site:
    /// putting `.flex_1().min_w_0()` on the **text** element makes that element's layout box
    /// stretch across all the row's remaining width whatever the text actually says, so the
    /// `flex_none` caret that follows it is pushed to the far right edge of the field instead of
    /// sitting against the last glyph. It looks correct in any field narrow enough that the text
    /// fills it, and wrong in every other one - which is why it kept shipping.
    ///
    /// So the rule this encodes is: **`flex_1`/`min_w_0` belong on the wrapper that holds both
    /// the caret and the text, never on the text itself.** The text is intrinsically sized and
    /// merely allowed to shrink and clip; the caret sits immediately after it.
    ///
    /// The second rule it encodes is GitHub issue #45's own: an empty field's real cursor
    /// position is 0, so the caret is drawn **before** the placeholder while the field is blank
    /// and **after** the text once there is any - never appended past whatever placeholder string
    /// happens to be rendering. And there is deliberately no gap between the text and the caret:
    /// a cursor sits flush against the last glyph (`crate::rail::render`'s own live report).
    ///
    /// The third rule, added by GitHub issue #162: the caret sits at
    /// [`SimpleInput::caret_offset`], not unconditionally at the end. Now that
    /// `crate::text_history::TextField` has a real caret, the text is split at it and the bar is
    /// drawn *between* the two halves. With the caret at the end - which is where it is for every
    /// field until the user presses Left or Home - the trailing half is empty and this renders
    /// exactly the structure it always did, which is why the existing caret-position tests still
    /// measure what they measured.
    ///
    /// [`SimpleInput::text_selector`] deliberately stays on the **leading** half rather than
    /// moving to a wrapper around both: those tests assert "the caret is at or past the text's
    /// right edge", and a wrapper containing the caret would enclose it and make that assertion
    /// unsatisfiable by construction.
    ///
    /// The caller still owns the box *around* this row - its border, padding, height and
    /// background - because that genuinely differs per field. What it no longer owns is the part
    /// that was never supposed to differ.
    pub(crate) fn render_simple_input_row(&self, input: SimpleInput<'_>) -> impl IntoElement {
        let SimpleInput {
            caret_selector,
            text_selector,
            focus_handle,
            text,
            caret_offset,
            placeholder,
            font: font_name,
            text_size,
            text_color,
            placeholder_color,
        } = input;
        // "Blank" by the same rule every caller used: nothing typed at all. A field holding only
        // spaces is holding real text and shows it, with the caret after it.
        let is_blank = text.is_empty();
        // Clamped and re-aligned to a real `char` boundary here as well as in `TextField`: this
        // helper also serves callers that pass an offset from somewhere else, and slicing a
        // `&str` mid-character panics.
        let split_at = (0..=text.len())
            .rev()
            .find(|offset| *offset <= caret_offset && text.is_char_boundary(*offset))
            .unwrap_or(0);
        let (before_caret, after_caret) = text.split_at(split_at);
        let leading = if is_blank {
            placeholder.to_string()
        } else {
            before_caret.to_string()
        };
        // `None` is a genuinely read-only row - a pinned note card that is not the one being
        // typed into, say. It cannot be expressed by passing a handle that happens to be
        // unfocused: several of these rows can be on screen sharing *one* focus handle (only one
        // draft is ever open), and a caret keyed on that shared handle would paint in every one
        // of them the moment any of them was focused.
        let caret = |el: gpui::Div| match focus_handle {
            Some(handle) => {
                el.child(self.render_simple_input_caret(caret_selector.clone(), handle))
            }
            None => el,
        };
        // One shared shape for both halves: intrinsically sized, and allowed to shrink and clip
        // rather than to wrap or to push the caret out of the row. `min_w_0` here is a *floor* of
        // zero on an auto-sized box, which is a different thing from the `flex_1` that used to sit
        // beside it and is what makes a field narrower than its own text degrade to a clipped line
        // instead of a two-line row.
        let span = |content: String, color: theme::ColorToken| {
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .font(font(font_name))
                .text_size(text_size)
                .text_color(color)
                .child(content)
        };
        div()
            // On the wrapper, which is the whole point - see this method's own docs.
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .when(is_blank, caret)
            .child(
                span(
                    leading,
                    if is_blank {
                        placeholder_color
                    } else {
                        text_color
                    },
                )
                .debug_selector(move || text_selector.to_string()),
            )
            .when(!is_blank, caret)
            // Only when there really is text after the caret, so a field with the caret at its end
            // paints the exact element tree it painted before this method knew about carets.
            .when(!is_blank && !after_caret.is_empty(), |el| {
                el.child(span(after_caret.to_string(), text_color))
            })
    }
}

/// One [`AdeApp::render_simple_input_row`] field: what it holds, what it says while empty, and
/// how it is painted.
///
/// A struct rather than eight positional parameters, for the same reason
/// `crate::code_surface::diff_view::DiffLineChrome` is one: past `clippy::too_many_arguments`, and
/// four of these are `&str`/colour pairs that would be trivially transposable at a call site.
pub(crate) struct SimpleInput<'a> {
    /// The caret element's own `debug_selector`, so a render test can assert *where* it painted.
    /// A [`SharedString`] for the same reason [`Self::text_selector`] is one.
    pub caret_selector: gpui::SharedString,
    /// The text element's `debug_selector`, for the same reason. A [`SharedString`] rather than a
    /// `&'static str` because a field that is a *row of a list* has one selector per row.
    pub text_selector: gpui::SharedString,
    /// The handle the caret watches - it paints only while this one is really focused. `None`
    /// draws no caret at all, which is what a read-only row of an otherwise-editable list is.
    pub focus_handle: Option<&'a FocusHandle>,
    /// What the field holds right now.
    pub text: &'a str,
    /// Where the insertion point is, as a byte offset into [`Self::text`] -
    /// `crate::text_history::TextField::caret`. Clamped and re-aligned to a `char` boundary by
    /// the renderer, so an out-of-range or mid-character value degrades to the nearest real one
    /// rather than panicking.
    pub caret_offset: usize,
    /// What it says while that is empty.
    pub placeholder: &'a str,
    /// The font family, as a `crate::theme::font` name.
    pub font: &'static str,
    /// Already scaled by the caller's own `AdeApp::ui_text_size` where that applies.
    pub text_size: Pixels,
    /// The colour of real typed text.
    pub text_color: theme::ColorToken,
    /// The (usually dimmer) colour of the placeholder.
    pub placeholder_color: theme::ColorToken,
}

/// [`AdeApp::render_simple_input_caret`]'s paint decision, pulled out as a pure function so it's
/// directly unit-testable without a real GPUI window/focus simulation - mirrors
/// `crate::code_surface::editing::caret_paint_quad`'s own rule (GitHub issue #107): solid only
/// while genuinely focused and blink-visible, nothing at all otherwise, just returning an opacity
/// instead of a ready-made [`gpui::PaintQuad`] since this caller's color/bounds aren't available
/// outside the canvas paint closure.
fn simple_input_caret_opacity(is_focused: bool, blink_visible: bool) -> Option<f32> {
    if is_focused && blink_visible {
        Some(1.0)
    } else {
        None
    }
}

impl AdeApp {
    /// One agent-kind chip (GitHub issue #5's "custom icon packs"): a real, user-supplied image
    /// from the active icon pack (`crate::icon_pack::resolve_icon`) if one exists for `kind`, at
    /// its own real colors (no theme tint - see `crate::icon_pack`'s own module docs on why),
    /// else this app's existing default look (a `size`-square rounded chip, tinted per
    /// `work_surface::agent_tint`, showing `work_surface::agent_initial`'s single letter) -
    /// unchanged from before this feature existed. `size` is also the image's real width/height,
    /// so a pack icon fills exactly the same box the default chip already occupies at every one
    /// of this helper's real call sites, rather than needing per-call-site size plumbing.
    ///
    /// Painted with `gpui::img()`, not `gpui::svg()` (GitHub issue #309): `svg()` rasterises to
    /// an alpha mask and paints it tinted with the element's own `style.text.color`
    /// (`vendor/zed/crates/gpui/src/elements/svg.rs`'s `paint` zips the path with that color and
    /// skips painting entirely when it's `None`), so a full-color pack icon drawn through `svg()`
    /// with no `.text_color()` set - the case this branch was in before this fix - painted
    /// nothing at all, an empty box, regardless of what colors the pack's own file defined.
    /// `img()`'s `paint` (`elements/img.rs`) never consults `style.text.color`; it decodes the
    /// source's own real pixels (raster or SVG - `Img::extensions()` covers both) and paints them
    /// as a full-color `PolychromeSprite`, which is what "keeps its own colours" actually
    /// requires in this GPUI version.
    pub(crate) fn render_agent_chip_icon(
        &self,
        kind: ProcessKind,
        size: Pixels,
        font_size: Pixels,
    ) -> gpui::AnyElement {
        if let Some(icon_path) = icon_pack::resolve_icon(
            &self.settings.icon_pack,
            work_surface::agent_icon_name(kind),
        ) {
            return gpui::img(icon_path)
                .flex_none()
                .w(size)
                .h(size)
                .debug_selector(|| "agent-chip-icon-pack-image".to_string())
                .into_any_element();
        }
        let (chip_fg, chip_bg) = work_surface::agent_tint(kind);
        let chip_glyph = work_surface::agent_initial(kind);
        div()
            .flex_none()
            .w(size)
            .h(size)
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::radius::CHIP)
            .bg(chip_bg)
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(font_size)
            .text_color(chip_fg)
            .debug_selector(|| "agent-chip-icon-default".to_string())
            .child(chip_glyph)
            .into_any_element()
    }
}

/// The fill behind a centered modal panel - the New file prompt
/// (`crate::root::new_file::AdeApp::render_new_file_prompt`).
///
/// Derived from `theme::surface::SCRIM`, the design handoff's own scrim colour
/// (`design_handoff_jerry_ade/revision/README.md`: "Scrim rgba(6,7,8,.62)"), rather than a raw
/// `gpui::black()` literal - a literal colour rather than a token, which is exactly what this
/// app's theming discipline exists to keep out. The alpha stays at 0.35: a small centered dialog
/// does not dim as hard as the palette, which replaces the entire workspace and uses the
/// designed 0.62.
pub(crate) fn modal_scrim_bg() -> gpui::Rgba {
    theme::surface::SCRIM.resolve().opacity(0.35)
}

/// One button in a centered modal's action row - `label`, tinted `theme::button::DANGER_FG` when
/// `destructive` (with `DANGER_FG_HOVER` on hover, this app's established destructive-control
/// pair) and `theme::text::BODY` otherwise.
///
/// Shape is `crate::work_surface::render::render_footer_action_button`'s: `h(23)`, `px(10)`,
/// `theme::radius::BUTTON` - the 4px "buttons" radius the design handoff calls for, not the 3px
/// `radius::CHIP` meant for chips and keycaps. Carries a real hover fill. The caller attaches its
/// own `.on_click`.
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
/// `crate::menu::model::menu_height`'s window-edge clamp - both read the real number
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

    /// GitHub issue #107: an earlier version painted a dim, non-blinking caret while unfocused.
    /// Colin asked for it to disappear entirely instead, the same way every other unfocused-state
    /// affordance in this app already does.
    #[test]
    fn an_unfocused_input_paints_nothing_at_all() {
        assert_eq!(
            simple_input_caret_opacity(false, true),
            None,
            "unfocused must paint nothing, regardless of the shared blink flag's phase"
        );
    }
}
