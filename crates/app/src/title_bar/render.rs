//! The title-bar band itself: the macOS traffic-light cluster, the Windows/Linux caption
//! buttons (minimise/maximise/close), and the left/right cluster composition around them.
//! The five `File Edit View Agent Help` dropdowns those labels open live in
//! [`super::menu`].

use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

/// Half-diagonal of an 11×1px rect rotated ±45° about its own center - `5.5 * cos(45°)`. Used to
/// place the close glyph's two crossing strokes (see [`render_close_glyph`]).
const CLOSE_GLYPH_HALF_DIAGONAL: f32 = 3.889_87; // 5.5 * cos(45°)

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
    pub(crate) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = self
            .focused_repo()
            .map(|repo| repo.name.clone())
            .unwrap_or_else(|| self.focused_repo_path().display().to_string());
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
