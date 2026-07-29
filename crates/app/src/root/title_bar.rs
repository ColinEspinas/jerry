use super::*;

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

    /// The 38px title-bar band (`design_handoff_jerry_ade/README.md`'s Layout table: height
    /// 38, bg `#101214`, bottom border `#1e2225`) - real window content, not OS chrome (see
    /// this step's task docs: the README's "the real app gets OS window chrome" refers to
    /// the *outer* window frame, and this band draws itself regardless of that). It carries
    /// [`Self::render_window_controls`], a divider, and the real project name/branch (the
    /// repository directory name and the main worktree's real detected branch, once
    /// `Self::load_worktrees` has resolved - never a placeholder string).
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
            .child(self.render_window_controls())
            .child(div().w(px(1.0)).h(px(16.0)).bg(theme::border::DIVIDER))
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
