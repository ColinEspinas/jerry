use super::*;

impl AdeApp {
    /// Applies one real `on_drag_move` tick for `target`'s pane, deriving the new width
    /// directly from the drag's current absolute cursor x position and [`Self::body_bounds`]
    /// via `crate::layout`'s pure, unit-tested clamp math - no "armed" drag-start baseline is
    /// carried between ticks (see [`Self::body_bounds`]'s docs for the verified
    /// `vendor/zed/crates/workspace/src/workspace.rs` precedent this follows). Since `target`
    /// comes straight from the `PaneResizeDrag` payload the event itself carries, this is
    /// always acting on the pane actually being dragged - there is no separate "is some other
    /// drag currently armed" state that could disagree with it.
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

    /// One real drag-to-resize splitter (`design_handoff_jerry_ade/README.md`'s Layout table: rail
    /// "276 (range 240–340)", panel "320 (260 in empty states)"), a thin (6px) invisible strip
    /// straddling the pane's edge - verified against `vendor/zed/crates/workspace/src/dock.rs`'s
    /// own real resize-handle shape (`RESIZE_HANDLE_SIZE = 6px`, absolutely positioned over the
    /// edge via `.right(-RESIZE_HANDLE_SIZE / 2.)`/`.left(-RESIZE_HANDLE_SIZE / 2.)`, `.occlude()`
    /// so it - not whatever's underneath - receives the mouse, and a real `col-resize` cursor via
    /// `.cursor_col_resize()`).
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
            // Only stops the mouse-down from propagating (e.g. into whatever's under the
            // handle) - verified against `vendor/zed/crates/workspace/src/dock.rs`'s own
            // resize handle, whose mouse-down handler does likewise and carries no drag-start
            // state of its own; the drag's baseline is [`Self::body_bounds`] plus the current
            // cursor position on each `on_drag_move` tick, not anything captured here.
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

/// Which of the two real drag-to-resize splitters (`design_handoff_jerry_ade/README.md`'s
/// Layout table: rail "276 (range 240–340)", panel "320 (260 in empty states)") is being
/// dragged - see [`AdeApp::apply_pane_resize`] and `crate::layout`'s pure clamp math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResizeTarget {
    Rail,
    Panel,
}

/// The invisible payload/"drag ghost" GPUI's real drag-and-drop system requires to start a
/// trackable drag (`Interactivity::on_drag`'s `T`/`W` type parameters - see this file's use of
/// `.on_drag`/`.on_drag_move` on the resize handles). Renders nothing (`gpui::Empty`), matching
/// `vendor/zed/crates/workspace/src/workspace.rs`'s own `DraggedDock` - the real, verified
/// precedent for using GPUI's drag system to implement a resize handle rather than a
/// drag-and-drop interaction (see that type's doc comment: "Useful for implementing draggable
/// UIs that don't conform to a drag and drop style interaction, like resizing").
#[derive(Debug, Clone, Copy)]
pub(super) struct PaneResizeDrag(pub(super) ResizeTarget);

impl gpui::Render for PaneResizeDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}
