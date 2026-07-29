use super::*;

impl AdeApp {
    /// Applies one `on_drag_move` tick for `target`'s pane, deriving the new width directly from
    /// the drag's current absolute cursor x position and [`Self::body_bounds`] via
    /// `crate::layout`'s clamp math - no "armed" drag-start baseline is carried between ticks.
    /// `target` comes straight from the `PaneResizeDrag` payload the event itself carries, so
    /// there's no separate "which drag is active" state that could disagree with it.
    pub(super) fn apply_pane_resize(
        &mut self,
        target: ResizeTarget,
        cursor_x: Pixels,
        cx: &mut Context<Self>,
    ) {
        let new_width = match target {
            ResizeTarget::Rail => {
                layout::rail_width_for_cursor(self.body_bounds.left().as_f32(), cursor_x.as_f32())
            }
            ResizeTarget::Panel => {
                layout::panel_width_for_cursor(self.body_bounds.right().as_f32(), cursor_x.as_f32())
            }
        };
        match target {
            ResizeTarget::Rail => self.rail_width = px(new_width),
            ResizeTarget::Panel => self.panel_width = px(new_width),
        }
        cx.notify();
    }

    /// One drag-to-resize splitter: a thin (6px) invisible strip straddling the pane's edge,
    /// matching `vendor/zed/crates/workspace/src/dock.rs`'s own resize-handle shape
    /// (`RESIZE_HANDLE_SIZE = 6px`, absolutely positioned over the edge, `.occlude()` so it -
    /// not whatever's underneath - receives the mouse).
    pub(super) fn render_resize_handle(
        &self,
        target: ResizeTarget,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        const HANDLE_WIDTH: f32 = 6.0;
        let id = match target {
            ResizeTarget::Rail => "rail-resize-handle",
            ResizeTarget::Panel => "panel-resize-handle",
        };

        let mut handle = div()
            .id(id)
            .absolute()
            .top(px(0.0))
            .h_full()
            .w(px(HANDLE_WIDTH))
            .cursor_col_resize()
            .occlude()
            .on_drag(PaneResizeDrag(target), move |drag, _offset, _window, cx| {
                cx.new(|_| *drag)
            })
            // Only stops the mouse-down from propagating; the drag's baseline is
            // [`Self::body_bounds`] plus the current cursor position on each `on_drag_move`
            // tick, not anything captured here.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            );

        handle = match target {
            ResizeTarget::Rail => handle.right(px(-HANDLE_WIDTH / 2.0)),
            ResizeTarget::Panel => handle.left(px(-HANDLE_WIDTH / 2.0)),
        };
        handle
    }
}

/// Which of the two drag-to-resize splitters (rail or panel) is being dragged - see
/// [`AdeApp::apply_pane_resize`] and `crate::layout`'s clamp math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResizeTarget {
    Rail,
    Panel,
}

/// The invisible payload/"drag ghost" GPUI's drag-and-drop system requires to start a trackable
/// drag (`Interactivity::on_drag`'s `T`/`W` type parameters). Renders nothing (`gpui::Empty`),
/// matching `vendor/zed/crates/workspace/src/workspace.rs`'s own `DraggedDock` - the precedent
/// for using GPUI's drag system to implement a resize handle rather than a drag-and-drop
/// interaction (per `Interactivity::on_drag_move`'s own doc comment: "Useful for implementing
/// draggable UIs that don't conform to a drag and drop style interaction, like resizing").
#[derive(Debug, Clone, Copy)]
pub(super) struct PaneResizeDrag(pub(super) ResizeTarget);

impl gpui::Render for PaneResizeDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}
