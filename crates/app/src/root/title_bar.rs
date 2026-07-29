use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

/// The five, real, honestly-inert Windows/Linux menu-row labels (`design_handoff_jerry_ade/
/// CHANGELOG.md`'s 2026-07-29 entry, change 1: "a menu row instead (`File Edit View Session
/// Help` ...)"). This app has no real File/Edit/View/Session/Help menu hierarchy to open (no
/// menu items exist anywhere in this codebase yet), so per this step's own task brief - "either
/// build it real or leave it as a real, honest hover-only label with no click handler" - these
/// are the latter: real, hoverable, correctly-styled labels with no `cursor_pointer()`/
/// `on_click()` at all, never a dropdown that looks openable but shows nothing. Building one
/// fake-looking exception (e.g. wiring `File` to close the window instead of opening a `File`
/// menu) was judged more misleading than an honestly static label, not less - see
/// [`AdeApp::render_windows_title_bar_left`]'s docs for the full judgment call.
const WINDOWS_MENU_ITEMS: [&str; 5] = ["File", "Edit", "View", "Session", "Help"];

/// Two 11×1px lines crossing at ±45° about their shared center - the real vector-path
/// substitute this app uses for the close caption button's `×` glyph (see
/// [`render_close_glyph`]'s own docs for why). Derived from `design_handoff_jerry_ade/
/// Jerry.dc.html`'s own two `width:11px;height:1px` rects rotated `±45deg`: rotating an 11×1
/// rect about its own center leaves its long axis 11px end-to-end, just now angled 45° off
/// horizontal, so each endpoint sits `5.5 * cos(45°)` from the center along both axes.
const CLOSE_GLYPH_HALF_DIAGONAL: f32 = 3.889_87; // 5.5 * cos(45°)

impl AdeApp {
    /// The three flat-circle window controls in the title bar's left cluster (matching
    /// `design_handoff_jerry_ade/Jerry.dc.html`'s three `#3a3f44` dots at the very start of
    /// its title bar - the design doesn't colour-code them the way macOS's traffic lights
    /// are, so this keeps that flat, neutral look while wiring each dot to a real GPUI
    /// window-control method (verified at `vendor/zed/crates/gpui/src/window.rs`:
    /// `remove_window` (`:2016`, used directly by `vendor/zed/crates/gpui/examples/
    /// on_window_close_quit.rs:19`), `minimize_window` (`:5520`), `zoom_window` (`:2489`,
    /// toggles maximize/restore) - the same three calls
    /// `vendor/zed/crates/platform_title_bar/src/platforms/platform_linux.rs`'s own
    /// `WindowControl::on_click` makes. Left-to-right order (close, minimize, maximize)
    /// mirrors that same three-flat-dot visual grouping's most common real-world reading
    /// (macOS traffic lights); this design deliberately doesn't colour-code them, so there
    /// is no ordering hint from the mockup itself - a judgment call, not a spec value.
    ///
    /// The wrapping row stops left-click propagation on mouse-down, mirroring
    /// `vendor/zed/crates/platform_title_bar/src/platforms/platform_linux.rs`'s
    /// `LinuxWindowControls` (`.on_mouse_down(MouseButton::Left, |_, _, cx|
    /// cx.stop_propagation())`), so pressing a dot can never also arm
    /// `Self::render_title_bar`'s window-move drag.
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

    /// The macOS-style left cluster: [`Self::render_window_controls`]'s three dots, plus the
    /// trailing 1×16 divider - `Jerry.dc.html`'s own `tbMac` block (`gap:8px;padding-left:2px`
    /// wrapper, divider `margin-left:6px`).
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

    /// The Windows/Linux-style left cluster: [`WINDOWS_MENU_ITEMS`]'s five real, honestly
    /// hover-only labels, plus the same trailing divider - `Jerry.dc.html`'s own `tbWin` block
    /// (`gap:2px;margin-left:-4px` wrapper, so each item's own `8px` horizontal padding lines
    /// its label up under the divider the same way the macOS cluster's `2px` left padding
    /// does). See [`WINDOWS_MENU_ITEMS`]'s own docs for why these are inert rather than a fake
    /// dropdown.
    fn render_windows_title_bar_left(&self) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .ml(px(-4.0))
            .children(WINDOWS_MENU_ITEMS.iter().map(|label| {
                div()
                    .flex_none()
                    .h(theme::band::TITLE_BAR_MENU_ITEM)
                    .px(px(8.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(11.0))
                            .text_color(theme::text::DIM)
                            .child(*label),
                    )
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

    /// The Windows/Linux title bar's three real caption buttons (minimise/maximise/close),
    /// pinned to the band's right edge - `design_handoff_jerry_ade/CHANGELOG.md`'s 2026-07-29
    /// entry, change 1: "three caption buttons pinned to the right edge, 44 wide × full band
    /// ..., glyphs `#a9b0b7`, close hover bg `#8c3a38`, others `#1b1f22`. Caption buttons sit
    /// **outside** the 12px band padding." `Jerry.dc.html`'s own `tbWin` trailing block:
    /// `align-self:stretch` (here, [`Styled::self_stretch`]) so each button fills the full
    /// 38px band height, and `margin:0 -12px 0 2px` (here, `.ml(px(2.0))` on the container plus
    /// this method's caller relying on the band's own right edge) to bleed past the band's own
    /// 12px right padding.
    ///
    /// Real, not decorative: wired to the exact same [`Window::minimize_window`]/
    /// [`Window::zoom_window`]/[`Window::remove_window`] calls
    /// [`Self::render_window_controls`]'s own docs verify - the macOS dot cluster and these
    /// caption buttons are two skins over identical real window-control behaviour, never two
    /// independently (and possibly divergently) implemented ones.
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
                theme::surface::ROW_HOVER_ALT,
                render_minimize_glyph(),
                |window, _cx| window.minimize_window(),
            ))
            .child(render_caption_button(
                "title-bar-caption-maximize",
                theme::surface::ROW_HOVER_ALT,
                render_maximize_glyph(),
                |window, _cx| window.zoom_window(),
            ))
            .child(render_caption_button(
                "title-bar-caption-close",
                theme::surface::TITLE_BAR_CLOSE_HOVER,
                render_close_glyph(theme::text::SECONDARY),
                |window, _cx| window.remove_window(),
            ))
    }

    /// The 38px title-bar band (`design_handoff_jerry_ade/README.md`'s Layout table: height
    /// 38, bg `#101214`, bottom border `#1e2225`) - real window content, not OS chrome (see
    /// this step's task docs: the README's "the real app gets OS window chrome" refers to
    /// the *outer* window frame, and this band draws itself regardless of that). It carries a
    /// real, platform-dependent left cluster ([`Self::render_macos_title_bar_left`] or
    /// [`Self::render_windows_title_bar_left`], per [`Self::window_controls_style`]'s
    /// [`WindowControlsStyle::is_macos`] - `design_handoff_jerry_ade/CHANGELOG.md`'s
    /// 2026-07-29 entry, change 1), the real project name/branch (the repository directory
    /// name and the main worktree's real detected branch, once `Self::load_worktrees` has
    /// resolved - never a placeholder string), and, on the Windows/Linux variant only, the
    /// real caption buttons ([`Self::render_windows_caption_buttons`]) pinned to the right
    /// edge.
    ///
    /// ## Dragging the window
    ///
    /// GPUI has no single "make this element drag the window" method; the real pattern
    /// (verified against `vendor/zed/crates/platform_title_bar/src/platform_title_bar.rs`'s
    /// own title bar, which faces the identical "Wayland/X11 have no native draggable
    /// titlebar for a client-side-decorated window" problem) is: mark the area with
    /// `.window_control_area(WindowControlArea::Drag)` (`vendor/zed/crates/gpui/src/
    /// elements/div.rs:1167`, a hit-test hint the compositor consults for double-click/
    /// right-click gestures - `vendor/zed/crates/gpui/src/window.rs:1747`'s
    /// `on_hit_test_window_control`), then drive the actual move from ordinary mouse
    /// events: arm [`Self::title_bar_move_armed`] on left mouse-down, and on the next
    /// mouse-move (still armed) call the real `Window::start_window_move`
    /// (`window.rs:2502` - "tells the compositor to take control of window movement
    /// (Wayland and X11)") and disarm. `on_mouse_up`/`on_mouse_down_out` also disarm, so a
    /// click that never moves (e.g. clicking to focus the window) never starts a move.
    /// [`Self::render_window_controls`] (the macOS dot cluster) and
    /// [`Self::render_windows_caption_buttons`] both stop propagation on their own mouse-down,
    /// so pressing any of those real, clickable controls can never also arm this drag.
    /// [`Self::render_windows_title_bar_left`]'s menu row deliberately does **not** - its five
    /// labels are honestly inert (see [`WINDOWS_MENU_ITEMS`]'s own docs: no `on_click` at all),
    /// so there is no click-triggered action there for a stray drag-arm to double-fire against;
    /// a press-and-drag starting on one of those labels falls through and starts a window move
    /// like any other blank patch of the band, same as clicking-and-holding one would do on a
    /// real OS title bar with an inert menu label under the cursor.
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
                self.render_windows_title_bar_left()
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

/// One flat-circle window-control button (see `AdeApp::render_window_controls`'s docs for
/// why these are real controls, not decoration) - `on_activate` is called with real
/// `&mut Window`/`&mut App` access so it can invoke a real `Window` control method.
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

/// One Windows/Linux caption button - 44 wide × full band height (`self_stretch`d by the
/// caller), `hover_bg` on hover, `glyph` centered inside, wired to a real `Window` control
/// method via `on_activate` - see [`AdeApp::render_windows_caption_buttons`]'s docs.
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

/// The minimise caption button's glyph - a plain 10×1px rect (`Jerry.dc.html`'s
/// `width:10px;height:1px;background:#8b9197`).
fn render_minimize_glyph() -> impl IntoElement {
    div().w(px(10.0)).h(px(1.0)).bg(theme::text::DIM)
}

/// The maximise caption button's glyph - a plain 9×9px 1px outline (`Jerry.dc.html`'s
/// `width:9px;height:9px;border:1px solid #8b9197`).
fn render_maximize_glyph() -> impl IntoElement {
    div()
        .w(px(9.0))
        .h(px(9.0))
        .border_1()
        .border_color(theme::text::DIM)
}

/// The close caption button's `×` glyph - two 11×1px lines crossing at ±45°
/// (`design_handoff_jerry_ade/Jerry.dc.html`'s two `transform:rotate(45deg)`/
/// `rotate(-45deg)` 11×1 rects). GPUI's `Style` has no CSS-transform-style `rotate` (verified:
/// no `rotate`/`transform` field anywhere in `vendor/zed/crates/gpui/src/style.rs`), so this
/// uses the real, verified vector-path substitute instead
/// (`vendor/zed/crates/gpui/examples/painting.rs`'s own `PathBuilder::stroke` +
/// `canvas`/`Window::paint_path` pattern): two straight strokes whose endpoints are exactly
/// where an 11×1 rect rotated ±45° about its own center would put them - see
/// [`CLOSE_GLYPH_HALF_DIAGONAL`]'s own docs for that geometry.
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

/// Real, interactive coverage for the Windows/Linux caption buttons - driven through GPUI's
/// actual `TestAppContext`/`VisualTestContext` harness (a real window, real hit-testing, real
/// click dispatch), mirroring `crate::root::focus::palette_focus_tests`'s own "not a mock of
/// any of those" shape.
///
/// ## Why only `close` gets a live-click test here
///
/// `AdeApp::render_windows_caption_buttons`'s minimise/maximise buttons call the exact same
/// real `Window::minimize_window`/`Window::zoom_window` the macOS dot cluster already used
/// (verified once, at `AdeApp::render_window_controls`'s own docs) - but actually *clicking*
/// them under this crate's `#[gpui::test]` harness was checked against the real
/// `TestWindow: PlatformWindow` impl (`vendor/zed/crates/gpui/src/platform/test/window.rs`)
/// before attempting it, and both `fn minimize(&self) { unimplemented!() }` and `fn zoom(&self)
/// { unimplemented!() }` are real, deliberate panics in that test backend (`is_maximized`
/// always returns `false` too, so there's no real toggled-state to assert against even if the
/// call didn't panic). A live click on either would crash the test process, not fail an
/// assertion - so this suite covers `close` only, the one caption button whose real backing
/// call ([`Window::remove_window`], which just flips an internal `removed` flag consumed by
/// `vendor/zed/crates/gpui/src/app.rs`'s own window-update trail) *is* implemented and
/// observable in the test harness. Minimise/maximise were instead verified manually against a
/// real running window (screenshots) - see this step's own report.
///
/// Click coordinates are computed from the real, already-rendered window's own
/// `Window::viewport_size` (the same "compute a click position from real, already-known layout
/// numbers" technique `vendor/zed/crates/editor/src/editor_tests.rs` uses throughout for its
/// own precise-pixel clicks) rather than a hardcoded guess: the close button is the rightmost
/// of the three 44px-wide caption buttons pinned flush to the title bar's right edge
/// (`AdeApp::render_windows_caption_buttons`), so its real center is always `(viewport_width -
/// 22, 19)` - 22px in from the right edge, 19px down (half the 38px band) - regardless of the
/// real test display's own size.
#[cfg(test)]
mod caption_button_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Clicking the real close caption button on the Windows/Linux title bar variant actually
    /// calls the real `Window::remove_window` (the same real API [`window_control_dot`]'s own
    /// close dot already used) and closes the real window - not a mock, not a click handler
    /// that merely looks wired up.
    #[gpui::test]
    fn clicking_the_close_caption_button_closes_the_real_window(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        // Pin the Windows/Linux caption-button variant regardless of the real host OS this
        // test happens to run on, so the test is deterministic everywhere (see `crate::keymap`'s
        // module docs for why one shared `WindowControlsStyle` field drives this).
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
