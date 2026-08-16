//! The title-bar band itself: the macOS traffic-light cluster, the Windows/Linux caption
//! buttons (minimise/maximise/close), and the left/right cluster composition around them.
//! The five `File Edit View Agent Help` dropdowns those labels open live in
//! [`super::menu`].

use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::plural;

/// Half-diagonal of an 11×1px rect rotated ±45° about its own center - `5.5 * cos(45°)`. Used to
/// place the close glyph's two crossing strokes (see [`render_close_glyph`]).
const CLOSE_GLYPH_HALF_DIAGONAL: f32 = 3.889_87; // 5.5 * cos(45°)

/// The macOS left cluster's reserved width when the real OS traffic lights occupy it - just wide
/// enough that the trailing divider (`render_macos_title_bar_left`'s own 1×16 bar) lands clear of
/// AppKit's own three buttons rather than under them. Not derived from
/// `crate::lib::default_window_options`'s `traffic_light_position` on purpose: that inset is
/// AppKit's own coordinate space (window-left-edge-relative, consumed by
/// `gpui_macos::window::PlatformWindow::move_traffic_light`), while this is a GPUI layout width
/// inside the already-padded title-bar band - the two numbers answer different questions and
/// keeping them as one shared constant would only coincidentally look related.
const MACOS_NATIVE_TRAFFIC_LIGHT_CLUSTER_WIDTH: gpui::Pixels = px(58.0);

/// Whether the *real, running* OS paints its own native traffic-light buttons over this window's
/// title bar - i.e. whether [`crate::default_window_options`]'s `traffic_light_position` is
/// actually consumed by the platform backend this build is running under
/// (`gpui_macos::window::PlatformWindow::move_traffic_light` is the only implementation that
/// reads it; every other platform backend ignores the field). Deliberately keyed to the compiled
/// `target_os`, not to [`crate::keymap::WindowControlsStyle`]: that setting only changes which
/// *skin* this title bar renders (a user can pick "macOS style" while running on Linux, in
/// `crate::keymap::WindowControlsStyle::MacosStyle`'s own tests), and doing that can never make a
/// real AppKit window appear underneath - so the custom dot cluster this file draws must keep
/// existing on every host except a real Mac, regardless of which style is selected.
fn host_draws_native_traffic_lights() -> bool {
    cfg!(target_os = "macos")
}

impl AdeApp {
    /// The three flat-circle window controls in the title bar's left cluster - painted only when
    /// [`Self::render_macos_title_bar_left`] is used *and* [`host_draws_native_traffic_lights`]
    /// is `false` (a non-Mac host running the macOS-style skin; on a real Mac, AppKit's own
    /// native traffic lights occupy this spot instead - see that function's docs), wired to real
    /// GPUI window-control methods (`Window::remove_window`/`minimize_window`, verified against
    /// `vendor/zed/crates/gpui/src/window.rs:2016,5520`). Left-to-right order (close, minimize,
    /// maximize) follows the macOS traffic-light convention; the design doesn't colour-code
    /// these dots, so there's no ordering hint from the mockup itself.
    ///
    /// The third dot deliberately calls `Window::toggle_fullscreen`, not `Window::zoom_window`:
    /// an earlier version called `zoom_window` here, copied from
    /// `vendor/zed/crates/platform_title_bar/src/platforms/platform_linux.rs`'s own
    /// `WindowControl::on_click` - correct for Linux, where the third caption button really is a
    /// zoom/restore toggle, but wrong for macOS, where the green traffic light's real convention
    /// is a fullscreen toggle. `Window::zoom_window` calls AppKit's `zoom(_:)`, which Apple
    /// documents as a no-op while the window is already in full-screen mode - so the old code
    /// could enter fullscreen (via the real, OS-drawn traffic light underneath, before this dot
    /// existed) but this dot itself could never leave it again: a real, reproduced dead end,
    /// found running this app on macOS for the first time.
    ///
    /// The wrapping row stops left-click propagation on mouse-down so pressing a dot can never
    /// also arm [`Self::render_title_bar`]'s window-move drag.
    pub(in crate::title_bar) fn render_window_controls(&self) -> impl IntoElement {
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
                window.toggle_fullscreen();
            }))
    }

    /// The macOS-style left cluster: [`Self::render_window_controls`]'s three dots, plus a
    /// trailing 1×16 divider - except on a real Mac, where AppKit already paints its own native
    /// traffic lights over this same band (`crate::default_window_options`'s
    /// `traffic_light_position`, see [`host_draws_native_traffic_lights`]). Drawing the custom
    /// dots there too used to leave two overlapping, differently-positioned window-control
    /// clusters on screen at once; this reserves their real footprint with a plain spacer instead
    /// of drawing a second set under buttons that already exist.
    fn render_macos_title_bar_left(&self) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(2.0))
            .child(if host_draws_native_traffic_lights() {
                div()
                    .flex_none()
                    .w(MACOS_NATIVE_TRAFFIC_LIGHT_CLUSTER_WIDTH)
                    .into_any_element()
            } else {
                self.render_window_controls().into_any_element()
            })
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
    /// [`crate::work_surface::render::AdeApp::render_tab_strip_plus`] uses for the `+`
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
                        let next = if this.title_menu_open == Some(menu) {
                            None
                        } else {
                            Some(menu)
                        };
                        // GitHub issue #176. This used to close only the `+` menu by hand - the
                        // shared sweep covers the file tree's context menu, the commit composer's
                        // menu and the graph's two menus as well, none of which this label knew
                        // about. `Title` is kept so switching File → Edit stays one open menu
                        // rather than briefly none.
                        let _ = this.close_menu_surfaces_except(Some(menus::MenuSurface::Title));
                        this.title_menu_open = next;
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

    /// The Windows/Linux title bar's three caption buttons (minimise/maximise/close), pinned to
    /// the band's right edge, 44px wide × full band height, bleeding past the band's own 12px
    /// right padding (`.mr(px(-12.0))`). Minimize/close call the same real
    /// [`Window::minimize_window`]/[`Window::remove_window`] [`Self::render_window_controls`]'s
    /// macOS dot cluster uses - two skins over identical real window-control behaviour there, not
    /// two independently implemented ones. Maximize does not: this button keeps
    /// [`Window::zoom_window`] (real Windows/Linux "restore/maximize" semantics), while the macOS
    /// cluster's third dot calls [`Window::toggle_fullscreen`] instead (real macOS green-light
    /// semantics) - see that dot's own docs for why they diverge. Only the close button's glyph
    /// uses [`theme::text::SECONDARY`]; minimize/maximize use the dimmer
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
    pub(crate) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // GitHub issue #90: a genuinely empty window (`Self::render_empty_state`) has no real
        // repo to name - `Self::focused_repo_path`'s own defensive fallback would otherwise
        // silently show a *different*, merely-known-but-unfocused repo's name here (or an empty
        // string), which would misleadingly suggest something is open when nothing is.
        let project_name = match self.focused_repo() {
            Some(repo) => repo.name.clone(),
            None => "No Folder Open".to_string(),
        };
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.is_main)
            .and_then(|item| item.branch.clone());
        let macos = self.window_controls_style().is_macos();
        let agent_state_chips = self.render_title_bar_agent_state_chips(cx);

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
            // GitHub issue #106: double-clicking empty title-bar space must maximize/restore the
            // window, the same real OS convention every native title bar gives for free -
            // matching `vendor/zed/crates/platform_title_bar/src/platform_title_bar.rs`'s own
            // split: macOS gets `Window::titlebar_double_click` (the real "zoom" gesture macOS
            // itself defines, which can also *minimize* depending on the user's own System
            // Settings, unlike a plain maximize toggle), everywhere else gets `Window::
            // zoom_window`. [`Self::render_windows_caption_buttons`]'s own maximize button
            // already calls `zoom_window` too, so this is the same real control, not a second
            // one. `ClickEvent::click_count()` (not two `on_mouse_down` events) is GPUI's own
            // real double-click detection, consistent with every other click handler in this
            // app.
            .on_click(cx.listener(move |_this, event: &ClickEvent, window, _cx| {
                if event.click_count() == 2 {
                    if macos {
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
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
            .when_some(agent_state_chips, |el, chips| el.child(chips))
            .when(!macos, |el| el.child(self.render_windows_caption_buttons()))
    }

    /// §4's title-bar row: `2 agents need input · 1 agent failed · 4 agents running`, one real
    /// chip per non-empty state, entirely absent (not an empty row) when
    /// [`title_bar_agent_state_chips`] returns nothing. Reads [`Self::build_agent_rows`] - the
    /// exact same real per-agent [`Status`] rows the rail and the status bar's own five-square
    /// dot row (`crate::status_bar::render::AdeApp::render_status_urgency_counters`) already
    /// compute - and [`rail::urgency_counts`] over them, so this can never disagree with either
    /// of those about how many agents are in which state.
    fn render_title_bar_agent_state_chips(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let rows = self.build_agent_rows(cx);
        let chips = title_bar_agent_state_chips(&rows);
        if chips.is_empty() {
            return None;
        }
        Some(
            div()
                .id("title-bar-agent-state-chips")
                .debug_selector(|| "title-bar-agent-state-chips".to_string())
                .flex()
                .items_center()
                // 9px, the mock's own gap for the compact chips - wider than the 6 the old
                // plated pills used, because without their backgrounds two chips 6px apart read
                // as one four-part cluster.
                .gap(px(9.0))
                .children(chips.into_iter().map(|(status, count, sentence)| {
                    render_title_bar_agent_state_chip(status, count, sentence)
                }))
                .into_any_element(),
        )
    }
}

/// The real, non-empty title-bar chips to render, already carrying their final display text, in
/// [`Status::ORDER`] order (`needs input` → `failed` → `running` - [`Status::Review`]/
/// [`Status::Idle`] never produce a chip here, see [`title_bar_agent_state_chip_text`]'s docs).
/// Pure and GPUI-window-free so it can be unit tested directly against hand-built [`AgentRow`]s,
/// the same way `crate::rail::state`'s own `urgency_counts` tests do - not a second,
/// independent count, just a filter/format pass over the one real
/// [`rail::urgency_counts`] already computed from [`AgentRow::status`].
fn title_bar_agent_state_chips(rows: &[AgentRow]) -> Vec<(Status, usize, String)> {
    rail::urgency_counts(rows)
        .into_iter()
        .filter_map(|(status, count)| {
            title_bar_agent_state_chip_text(status, count).map(|text| (status, count, text))
        })
        .collect()
}

/// Title-bar text for one agent-status chip
/// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §4: "Title bar |
/// `2 agents need input · 1 agent failed · 4 agents running` | agent-level, one chip per
/// non-empty state", confirmed verbatim by
/// `design_handoff_jerry_ade/revision 3/CHANGELOG.md`'s own "Counts" section). `None` for a
/// zero count - hidden entirely, never an empty chip - and for [`Status::Review`]/
/// [`Status::Idle`]: the design's own title-bar example names exactly three states
/// (`Status::Ask`/`Status::Fail`/`Status::Run`), and finished/idle counts already have a
/// real home - the rail's own agent rows, and the status bar's five-way dot row - so repeating
/// them a third time here would just restate the same number in a fourth place.
///
/// Each string names its own unit (`"N agents ..."`, never bare `"N ..."`) per §4's own rule
/// ("No two counters in the window may share wording while counting different units"), and
/// pluralizes correctly at exactly 1 vs 2+ - `Status::Ask` conjugates its verb too (`"1 agent
/// needs input"` vs `"2 agents need input"`), matching the design's own two example counts.
///
/// Both halves of that agreement come from [`crate::root::plural`]: the noun via
/// [`plural::count`], the verb via [`plural::form`]. This is the sentence-conjugation case
/// rev 6 §7 rule 9 calls out, and it deliberately goes through the same helper as every bare
/// `"N files"` in the window rather than keeping its own `== 1` test.
fn title_bar_agent_state_chip_text(status: Status, count: usize) -> Option<String> {
    if count == 0 {
        return None;
    }
    let agents = plural::count(count, "agent", None);
    match status {
        Status::Ask => Some(format!(
            "{agents} {} input",
            plural::form(count, "needs", "need")
        )),
        Status::Fail => Some(format!("{agents} failed")),
        Status::Run => Some(format!("{agents} running")),
        Status::Review | Status::Idle => None,
    }
}

/// One title-bar agent-state chip, rebuilt for rev 6 as a **compact dot + count**
/// (`design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4b's table: "agent status counts |
/// title-bar text badges · footer dot cluster | **title bar, compact dots**, words in the
/// tooltip").
///
/// The sentence is not deleted, it *moves*: `sentence` - still
/// [`title_bar_agent_state_chip_text`]'s fully conjugated `"2 agents need input"` - becomes this
/// chip's real tooltip, so nothing is lost, only demoted from a permanent claim on the window's
/// centre line to something you point at. The pill background goes with the words: a 5×5 square
/// and a bare number need no plate behind them, and the plate was most of what made three of
/// these read as a banner.
///
/// `count` is passed in rather than parsed back out of `sentence`: the two must be the same
/// number by construction, not by string surgery, so [`title_bar_agent_state_chips`] hands over
/// both halves of one derivation.
///
/// The count's own colour follows the mock's `titleCounts`: `ask`/`fail` keep their state hue
/// (those are the two you are meant to see across the room), `run` drops to a neutral tone -
/// running agents are the ordinary condition and spending the attention colour on them devalues
/// it everywhere else, the same rule §4c applies to the load meter and §4b to the platform pill.
fn render_title_bar_agent_state_chip(
    status: Status,
    count: usize,
    sentence: String,
) -> impl IntoElement {
    let count_color: gpui::Rgba = match status {
        Status::Run => theme::text::DIMMER.into(),
        _ => status.color(),
    };
    div()
        .id(("title-bar-agent-state-chip", status as usize))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.0))
        .tooltip(crate::root::widgets::text_tooltip(sentence))
        .child(
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded(px(1.0))
                .bg(status.color()),
        )
        .child(
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(count_color)
                .child(count.to_string()),
        )
}

/// One flat-circle window-control button - `on_activate` is called with real `&mut Window`/
/// `&mut App` access so it can invoke a real `Window` control method.
pub(in crate::title_bar) fn window_control_dot(
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
/// Minimise calls the same real `Window::minimize_window` the macOS dot cluster's own minimize
/// dot already uses; maximise calls `Window::zoom_window` (see
/// [`Self::render_windows_caption_buttons`]'s own docs for why that's Windows/Linux-only now).
/// Either way, the test backend's `TestWindow: PlatformWindow` impl
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

    /// GitHub issue #106's own real `click_count() == 2` guard on the title bar's double-click-
    /// to-maximize handler - a *single* click on empty title-bar space must not call `Window::
    /// zoom_window`/`Window::titlebar_double_click` at all. The positive case (a real double
    /// click really does maximize) can't be driven live here for the same reason minimise/
    /// maximise have no live-click test above: the test backend's `zoom_window`/
    /// `titlebar_double_click` are `unimplemented!()` and would panic the test process - verified
    /// manually against a real running window instead. This test is what a bug letting a *single*
    /// click reach that same call would actually crash, so it's real coverage of the guard, not a
    /// placeholder.
    #[gpui::test]
    fn a_single_click_on_empty_title_bar_space_never_reaches_the_maximize_call(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let _ = app;

        // Well clear of the left cluster (macOS dots / Windows-Linux menu labels) and the right
        // caption buttons - genuinely empty title-bar space, the same area a real double-click
        // would land on to maximize the window.
        cx.simulate_click(gpui::point(px(400.0), px(19.0)), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            cx.windows().len(),
            1,
            "a single click must never reach the real zoom/maximize call - if it did, the test \
             backend's own `unimplemented!()` for that call would have panicked this test process \
             instead of reaching this assertion"
        );
    }
}

/// Real, interactive coverage for the macOS dot cluster's third ("maximize") dot - unlike
/// [`caption_button_tests`]'s minimise/maximise, this one can be live-click-tested: it now calls
/// [`Window::toggle_fullscreen`] (this module's own fix for the dead end this bug report is
/// named after), and unlike `zoom`/`minimize`, GPUI's `TestWindow` gives `toggle_fullscreen` a
/// real implementation (`vendor/zed/crates/gpui/src/platform/test/window.rs`: flips a real
/// `is_fullscreen` flag, no `unimplemented!()`), so a click here can be asserted against instead
/// of only verified manually.
///
/// `#[cfg(not(target_os = "macos"))]`: this test clicks the coordinates the *custom* dot cluster
/// paints at, which [`host_draws_native_traffic_lights`] now means only exist on a non-Mac host
/// running the macOS-style skin (`WindowControlsStyle::MacosStyle`, forced below regardless of
/// the real host) - on a real Mac, that spot is a plain spacer reserving room for AppKit's own
/// native buttons instead (see [`AdeApp::render_macos_title_bar_left`]), so this test would just
/// click empty space there.
#[cfg(all(test, not(target_os = "macos")))]
mod macos_dot_cluster_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Clicking the real macOS "maximize" dot toggles the real window's fullscreen state both
    /// ways - into fullscreen, and back out again. Before this module's fix, the second click
    /// called `Window::zoom_window` (AppKit's `zoom(_:)`), which Apple documents as a no-op while
    /// already fullscreen: the window would have stayed stuck in fullscreen exactly as it did
    /// running this app on macOS for the first time - this test would have caught that
    /// regression by failing its second assertion.
    #[gpui::test]
    fn clicking_the_macos_maximize_dot_toggles_fullscreen_both_ways(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        // Pin the macOS dot-cluster variant regardless of the real host OS this test happens to
        // run on, so the test is deterministic everywhere (the same reasoning
        // `caption_button_tests` pins the opposite variant for).
        app.update(cx, |app, cx| {
            app.set_window_controls_style(WindowControlsStyle::MacosStyle, cx);
        });
        cx.run_until_parked();

        // Left cluster offset from the window's left edge: the title bar's own 12px padding
        // (`render_title_bar`'s `.px(px(12.0))`) + the macOS left cluster's 2px (`
        // render_macos_title_bar_left`'s `.pl(px(2.0))`) + the dot row's own 2px (`
        // render_window_controls`'s `.pl(px(2.0))`) = 16px to the close dot's left edge. Each dot
        // is 11px wide with an 8px gap (`render_window_controls`'s `.gap(px(8.0))`), so the third
        // ("maximize") dot's box starts at 16 + 2 * (11 + 8) = 54px; its center is 5.5px further.
        // Vertical center is the same 19px (half of the 38px `theme::band::TITLE_BAR`)
        // `caption_button_tests`'s own close-button test already uses.
        let maximize_dot_center = gpui::point(px(54.0) + px(5.5), px(19.0));

        assert!(
            !cx.update(|window, _app| window.is_fullscreen()),
            "window must not start fullscreen"
        );

        cx.simulate_click(maximize_dot_center, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.update(|window, _app| window.is_fullscreen()),
            "clicking the maximize dot once must enter fullscreen"
        );

        cx.simulate_click(maximize_dot_center, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            !cx.update(|window, _app| window.is_fullscreen()),
            "clicking the maximize dot a second time must leave fullscreen again - this is the \
             exact click that used to reach `Window::zoom_window` and silently do nothing"
        );

        let _ = app;
    }
}

/// Pure coverage for [`title_bar_agent_state_chip_text`]/[`title_bar_agent_state_chips`] -
/// no GPUI window needed, since neither function touches one. Hand-built [`AgentRow`]s, the
/// same idiom `crate::rail::state`'s own `urgency_counts`/`filter_agents` tests use (see that
/// module's private `row` helper) - real production types, just constructed directly instead
/// of derived from a live process, so wording/pluralization/visibility rules are exercised
/// exhaustively without a slow or environment-dependent real spawn. The genuinely-live half of
/// this feature (a real spawned agent's real derived `Status` actually reaching this same code)
/// is separately covered by `title_bar_agent_state_chips_live_tests` below.
#[cfg(test)]
mod agent_state_chip_text_tests {
    use super::*;

    fn row(id: u64, status: Status) -> AgentRow {
        AgentRow {
            id,
            kind: ProcessKind::claude(),
            title: format!("agent-{id}"),
            cwd: PathBuf::from(format!("/wt-{id}")),
            status,
            branch: Some("feature-x".to_string()),
            add: 0,
            del: 0,
            exit_code: None,
            activity: None,
            elapsed: Duration::ZERO,
            review_file_count: None,
        }
    }

    #[test]
    fn a_zero_count_is_hidden_for_every_status() {
        for status in Status::ORDER {
            assert_eq!(
                title_bar_agent_state_chip_text(status, 0),
                None,
                "a zero count must never render a chip, for {status:?}"
            );
        }
    }

    #[test]
    fn review_and_idle_never_get_a_title_bar_chip_even_with_a_real_nonzero_count() {
        // The design's own title-bar example names exactly three states (§4) - finished and
        // idle counts already have a real home in the rail's own agent rows and the status
        // bar's five-way dot row, so they must stay silent here even when genuinely non-zero.
        assert_eq!(title_bar_agent_state_chip_text(Status::Review, 3), None);
        assert_eq!(title_bar_agent_state_chip_text(Status::Idle, 5), None);
    }

    /// The title bar's half of revision 6's `Review ready` → `Finished` rename (GitHub issue
    /// #280) - twin of `crate::rail::render`'s `status_wording_tests`, which sweeps every other
    /// status-derived string in the window. Kept here because this formatter is private to this
    /// module. Today [`Status::Review`] produces no chip at all, so this passes vacuously for
    /// that status; the point is that it keeps passing if a future revision gives it one.
    #[test]
    fn no_title_bar_chip_text_ever_says_review_ready() {
        for status in Status::ORDER {
            for count in [0, 1, 2, 12] {
                let Some(text) = title_bar_agent_state_chip_text(status, count) else {
                    continue;
                };
                assert!(
                    !text.to_lowercase().contains("review ready"),
                    "revision 6 renamed this status to 'Finished' - no title-bar chip may say \
                     'review ready' again, got {text:?} for {status:?}"
                );
            }
        }
    }

    #[test]
    fn ask_conjugates_its_verb_at_exactly_one_vs_two_or_more() {
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Ask, 1).as_deref(),
            Some("1 agent needs input")
        );
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Ask, 2).as_deref(),
            Some("2 agents need input")
        );
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Ask, 7).as_deref(),
            Some("7 agents need input")
        );
    }

    #[test]
    fn fail_pluralizes_the_noun_only_the_verb_does_not_conjugate() {
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Fail, 1).as_deref(),
            Some("1 agent failed")
        );
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Fail, 2).as_deref(),
            Some("2 agents failed")
        );
    }

    #[test]
    fn run_matches_the_design_doc_s_own_worked_example_exactly() {
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Run, 1).as_deref(),
            Some("1 agent running")
        );
        // `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §4's own literal
        // example text - "4 agents running" - reproduced verbatim.
        assert_eq!(
            title_bar_agent_state_chip_text(Status::Run, 4).as_deref(),
            Some("4 agents running")
        );
    }

    /// Rev 6 §7 rule 4, quoted verbatim: "Two states distinguished anywhere in the app are never
    /// summed anywhere in it."
    ///
    /// The title bar is where `ask`/`fail`/`run` are distinguished, so no agent may contribute to
    /// more than one chip, and no chip may carry a count larger than the number of agents really
    /// in that state. Proved by construction over a mixed row set: the chip counts must partition
    /// the rows exactly, never overlap.
    #[test]
    fn no_agent_is_ever_counted_in_two_chips_at_once() {
        let rows = vec![
            row(1, Status::Ask),
            row(2, Status::Ask),
            row(3, Status::Fail),
            row(4, Status::Run),
            row(5, Status::Review),
            row(6, Status::Idle),
        ];
        let chips = title_bar_agent_state_chips(&rows);

        for (status, count, _) in &chips {
            let really_in_that_state = rows.iter().filter(|r| r.status == *status).count();
            assert_eq!(
                *count, really_in_that_state,
                "{status:?}'s chip must count exactly the agents really in that state - a chip \
                 that summed two states would report more"
            );
        }
        let charged: usize = chips.iter().map(|(_, count, _)| count).sum();
        let chip_eligible = rows
            .iter()
            .filter(|r| matches!(r.status, Status::Ask | Status::Fail | Status::Run))
            .count();
        assert_eq!(
            charged,
            chip_eligible,
            "the chips together must charge each eligible agent exactly once - {charged} charged \
             across {} chips for {chip_eligible} eligible agents",
            chips.len()
        );
    }

    /// A single agent can only ever be in one state, so exactly one chip can ever exist for a
    /// one-agent window - the degenerate case of the rule above, and the one a "count it in both"
    /// bug would show up in first.
    #[test]
    fn one_agent_produces_exactly_one_chip_whatever_its_state() {
        for status in [Status::Ask, Status::Fail, Status::Run] {
            let chips = title_bar_agent_state_chips(&[row(1, status)]);
            assert_eq!(
                chips.len(),
                1,
                "one agent in {status:?} must produce exactly one chip"
            );
            assert_eq!(chips[0].1, 1);
        }
    }

    /// §4b's compaction: the chip shows a bare count, and the fully conjugated sentence is what
    /// the tooltip carries - so the two halves must come out of the same call, and the number
    /// shown must be the number the sentence names.
    #[test]
    fn each_chip_carries_both_its_compact_count_and_its_full_sentence() {
        let rows = vec![
            row(1, Status::Ask),
            row(2, Status::Ask),
            row(3, Status::Run),
        ];
        for (_, count, sentence) in title_bar_agent_state_chips(&rows) {
            assert!(
                sentence.starts_with(&format!("{count} ")),
                "the tooltip sentence must name the same number the chip shows - {count} vs \
                 {sentence:?}"
            );
            assert!(
                sentence.contains("agent"),
                "the unit noun moves into the tooltip rather than disappearing: {sentence:?}"
            );
        }
    }

    #[test]
    fn zero_agents_anywhere_produces_an_entirely_empty_chip_list() {
        assert_eq!(title_bar_agent_state_chips(&[]), Vec::new());
    }

    #[test]
    fn only_non_empty_states_produce_a_chip_and_review_idle_are_excluded() {
        let rows = vec![
            row(1, Status::Ask),
            row(2, Status::Review),
            row(3, Status::Idle),
        ];
        assert_eq!(
            title_bar_agent_state_chips(&rows),
            vec![(Status::Ask, 1, "1 agent needs input".to_string())],
            "only the one non-empty Ask/Fail/Run state should produce a chip - Review and Idle \
             must stay silent even though they are the majority of the rows"
        );
    }

    #[test]
    fn every_shown_state_at_once_renders_in_needs_input_failed_running_order() {
        let rows = vec![
            row(1, Status::Ask),
            row(2, Status::Ask),
            row(3, Status::Fail),
            row(4, Status::Run),
            row(5, Status::Run),
            row(6, Status::Run),
            row(7, Status::Run),
        ];
        assert_eq!(
            title_bar_agent_state_chips(&rows),
            vec![
                (Status::Ask, 2, "2 agents need input".to_string()),
                (Status::Fail, 1, "1 agent failed".to_string()),
                (Status::Run, 4, "4 agents running".to_string()),
            ],
            "must match the design doc's own worked example (§4) both in wording and order"
        );
    }

    #[test]
    fn a_real_status_change_between_two_row_sets_is_reflected_in_the_chip_list() {
        // Simulates the same "spawn agents, change their derived status" shape the live tests
        // below exercise with a real process - here at the pure-data level, over the exact
        // `AgentRow`/`Status` types `crate::rail::render::AdeApp::build_agent_rows` produces.
        let before = vec![row(1, Status::Run)];
        assert_eq!(
            title_bar_agent_state_chips(&before),
            vec![(Status::Run, 1, "1 agent running".to_string())]
        );

        let mut after = before;
        after[0].status = Status::Fail;
        assert_eq!(
            title_bar_agent_state_chips(&after),
            vec![(Status::Fail, 1, "1 agent failed".to_string())],
            "the chip list must track a changed row's status, not the row set's identity"
        );
    }
}

/// Genuinely-live coverage: real spawned [`ProcessKind::Shell`] agents in a real
/// [`gpui::TestAppContext`] window, read back through the real
/// [`crate::rail::render::AdeApp::build_agent_rows`]/[`Self::render_title_bar_agent_state_chips`]/
/// [`Self::render_title_bar`] path - proves the wiring itself, not just the pure formatting
/// logic `agent_state_chip_text_tests` above already covers exhaustively.
///
/// Only `Status::Run` is driven live here (a freshly spawned agent is real, immediately
/// observable, and needs no artificial delay or environment-dependent trick to reach): getting
/// a real, deterministic `Status::Fail`/`Status::Ask` out of a live process without either a
/// slow real wait (15s+ for `Status::Ask`'s idle threshold) or mutating process-global
/// environment state (`$SHELL`/`$PATH`, which this workspace's own established discipline
/// forbids in a test - see `pty_core::resolve_in_path_var`'s and `lsp_core::client::
/// resolve_server_binary_with`'s docs for the exact same call) isn't practical through
/// [`crate::work_surface::agents::Agents::spawn`]'s real `ProcessKind`-only spec. Those two states'
/// text/visibility rules are instead covered - just as really, over the identical `Status`/
/// `AgentRow` types - by the pure tests above.
#[cfg(test)]
mod agent_state_chip_live_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn worktree_item(path: PathBuf, label: &str) -> WorktreeItem {
        WorktreeItem {
            path,
            label: label.to_string(),
            branch: Some(label.to_string()),
            is_main: false,
            is_bare: false,
            is_detached: false,
            short_sha: None,
            is_locked: false,
            lock_reason: None,
            is_broken: false,
            broken_reason: None,
            error: None,
        }
    }

    /// `AdeApp::new_with_settings` (`crate::root::state`, see its own "a fresh window starts
    /// with one shell in the repo root" comment) always auto-spawns exactly one real
    /// `ProcessKind::Shell` agent at startup, so `open_test_app` never actually starts at zero
    /// agents - there is no real "empty app" state to observe live. This closes that one real
    /// startup agent via the real `Self::close_agent` path (the same one the tab strip's `×`
    /// and `Self::archive_agent` use) to reach a genuine, live zero-agents state, and proves
    /// the title bar's chip row really disappears in response - not just that it starts out
    /// `None` before anything has run. `Self::render_title_bar` itself is also exercised at
    /// both ends, unmodified, to prove the real production method doesn't panic either way.
    ///
    /// The startup shell is retagged to a real `AgentKind` via `Agents::set_kind_for_test`
    /// immediately after spawn: the rail (and so the title bar chip it drives) never produces a
    /// row for a plain shell at all (`AdeApp::build_agent_rows`'s own docs), so this test's real
    /// subject - "the chip row disappears once the one open real agent closes" - needs a real
    /// agent kind to have anything to observe in the first place. The retag doesn't touch the
    /// real spawned process, only the bookkeeping `kind` the rail/chip logic reads.
    #[gpui::test]
    fn closing_the_only_real_agent_makes_the_chip_row_disappear(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let startup_agent_id = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("the real startup shell must be the active agent");
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(startup_agent_id, ProcessKind::claude());
        });

        let chips_at_startup = app.update(cx, |app, cx| app.render_title_bar_agent_state_chips(cx));
        assert!(
            chips_at_startup.is_some(),
            "the retagged startup process is a genuine Status::Run agent and must produce a \
             real, visible chip"
        );
        app.update(cx, |app, cx| {
            let _ = app.render_title_bar(cx);
        });

        app.update_in(cx, |app, window, cx| {
            app.close_agent(startup_agent_id, window, cx);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, cx| app.build_agent_rows(cx))
                .is_empty(),
            "closing the one real open agent must leave zero rows - not a fabricated zero, a \
             genuinely empty real agent list"
        );
        let chips_after_close =
            app.update(cx, |app, cx| app.render_title_bar_agent_state_chips(cx));
        assert!(
            chips_after_close.is_none(),
            "zero real agents left open must hide the chip row entirely, not render an empty \
             one - this is the live version of \
             `agent_state_chip_text_tests::zero_agents_anywhere_produces_an_entirely_empty_chip_list`"
        );
        app.update(cx, |app, cx| {
            let _ = app.render_title_bar(cx);
        });
    }

    /// Spawning a real second agent on top of the real startup shell is a genuine state change
    /// over live process data - not a hand-built row - and the chip text must track it,
    /// including the exact singular → plural flip at 1 → 2.
    ///
    /// Both the startup shell and the second spawned process are retagged to a real `AgentKind`
    /// via `Agents::set_kind_for_test` (see the sibling test's own docs for why): the rail no
    /// longer produces any row at all for a plain shell, but this test is specifically about the
    /// chip's real PTY-activity/idle-duration tracking, which is genuinely cheaper and more
    /// deterministic to exercise against real shell processes than against real `claude`/`codex`
    /// CLI spawns - the retag keeps that real PTY behavior while making the two agents count as
    /// real agents for the chip logic under test.
    #[gpui::test]
    fn a_second_real_spawned_agent_flips_the_live_chip_from_singular_to_plural(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_b = tempfile::tempdir().expect("tempdir wt-b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let startup_agent_id = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("the real startup shell must be the active agent");
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(startup_agent_id, ProcessKind::claude());
        });

        let rows_at_startup = app.update(cx, |app, cx| app.build_agent_rows(cx));
        assert_eq!(
            rows_at_startup
                .iter()
                .filter(|row| row.status == Status::Run)
                .count(),
            1,
            "the real startup shell must be observably Status::Run (recently active, still \
             alive) - real derived status from a real process, not an assumption"
        );
        assert_eq!(
            title_bar_agent_state_chips(&rows_at_startup),
            vec![(Status::Run, 1, "1 agent running".to_string())],
            "one real running agent must read as the singular form"
        );
        let first_agent_id = startup_agent_id;

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_b.path().to_path_buf(), "wt-b")];
        });
        let second_agent_id = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(second_agent_id, ProcessKind::codex());
        });
        // The pty spawn is async (`TerminalPane::spawn_process`) - let it actually start
        // before reading a live `Status` off it, the same real seam
        // `rail_row_tests::worktree_is_expanded_defaults_to_the_real_idle_rooted_rule` uses.
        cx.run_until_parked();

        // Spawning this real second agent means a real, uncontrolled amount of wall-clock time
        // elapses between it and the first agent's own last real activity (a real subprocess
        // spawn - `TerminalPane::spawn_process` - under real full-suite parallel load, where
        // dozens of other tests' own real subprocesses are contending for the same CPU cores,
        // can genuinely take over `rail::status::RUN_RECENT_OUTPUT_WINDOW`'s real 2 real
        // seconds; this was observed directly - `idle_duration` on the first agent measured at
        // 2.91s in a loaded run, tipping it from `Status::Run` to `Status::Idle` and failing
        // this test on a dimension it never meant to exercise). A real user with two
        // simultaneously open agents naturally keeps glancing at/using each one, not leaving the
        // first one motionless for that long the instant a second is spawned - so give the first
        // agent's pty one more real, harmless Ctrl-L (the exact same real
        // `TerminalPane::clear`/ECHOCTL round trip `terminal_clear_action_tests` and
        // `clear_pty_signal_tests` already prove reaches a real pty) and wait for its real echo,
        // which refreshes its real `activity_at` through the exact same product code path a real
        // keystroke would. This is what the test is actually about - the chip correctly tracking
        // two genuinely live agents - not an incidental race against how long the second agent's
        // own real spawn happened to take.
        let first_pane = app
            .read_with(cx, |app, _| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == first_agent_id)
                    .map(|agent| agent.pane.clone())
            })
            .expect("the first agent must still be open");
        first_pane.update(cx, |pane, cx| pane.clear(cx));
        // `clear()`'s real Ctrl-L reaches the real pty either way, but *what* comes back differs
        // by how far along the real shell's own line editor is: a shell whose readline has
        // already taken over the tty (as this long-alive first agent's has, unlike a
        // freshly-spawned one) intercepts Ctrl-L as its own "redraw", never echoing a literal
        // "^L" - so what's checked here is the actual thing this test cares about,
        // `idle_duration` itself, not a literal byte sequence that this particular shell state
        // was never going to produce.
        let refresh_deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let mut first_agent_refreshed = false;
        loop {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(8));
            cx.run_until_parked();
            let idle = first_pane.read_with(cx, |pane, _| pane.idle_duration());
            if idle.is_some_and(|idle| idle < std::time::Duration::from_secs(1)) {
                first_agent_refreshed = true;
                break;
            }
            if std::time::Instant::now() >= refresh_deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            first_agent_refreshed,
            "expected the first agent's real pty activity to refresh after this real Ctrl-L \
             round trip"
        );

        let rows_after_second_spawn = app.update(cx, |app, cx| app.build_agent_rows(cx));
        let running_count = rows_after_second_spawn
            .iter()
            .filter(|row| row.status == Status::Run)
            .count();
        assert_eq!(
            running_count, 2,
            "the real startup shell plus this real second spawn must both be observed running \
             - this is the real state change the chip text must track"
        );
        assert_eq!(
            title_bar_agent_state_chips(&rows_after_second_spawn),
            vec![(Status::Run, 2, "2 agents running".to_string())],
            "the chip text must flip from singular to plural on this real second spawn, not \
             stay frozen at \"1 agent running\""
        );

        // The real, unmodified render method must still run without panicking with two live
        // agents open.
        app.update(cx, |app, cx| {
            let _ = app.render_title_bar(cx);
        });
    }
}
