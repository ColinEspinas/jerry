//! Real, themed, overlay scrollbars (GitHub issue #30) for every scrollable region audited in
//! that issue: the file tree and Changes list (`crate::sidebar`), the code editor and merge
//! hand-edit buffer (`crate::code_surface`, `crate::merge`), Settings' nav/content columns
//! (`crate::settings`), the agent rail's worktree list (`crate::rail`), the command palette's
//! result list (`crate::palette`), and the read-only diff view (`crate::code_surface::diff_view`).

use gpui::{AnyElement, Rgba, ScrollHandle};

use super::*;
use crate::root::scrollbar_geometry as geometry;

/// The scrollbar's own thickness (track + thumb width/height) - the hit target as well as the
/// visual width.
const SCROLLBAR_SIZE: f32 = 10.0;

/// How far clear of the track's edges the painted thumb floats - a 2px transparent border. A
/// plain `f32` mirror of [`theme::scrollbar::THUMB_INSET`], for the same
/// const-context reason [`SCROLLBAR_SIZE`] is, and pinned to it by the same test.
const THUMB_INSET: f32 = 2.0;

/// The inset only means anything if it leaves a thumb to paint. Both operands are `const`, so this
/// is a genuine compile-time guard rather than a test - retuning §4p's numbers into a combination
/// that paints nothing fails to build, in every profile, not just under `cargo test`.
const _: () = assert!(SCROLLBAR_SIZE - 2.0 * THUMB_INSET > 0.0);

/// How much real right-side clearance row/header content next to this scrollbar needs, in
/// addition to `SCROLLBAR_SIZE` itself - GitHub issue #123 ("Add padding to the file tree right
/// side icons/buttons"). The scrollbar track is painted as an `.absolute()` sibling overlay
/// (this module's own top docs explain why), not reserved flex space, so any row/header content
/// whose own right padding is `<= SCROLLBAR_SIZE` sits exactly where the track begins - flush
/// contact, not a real gap - whenever the list is scrollable. Before this fix
/// `crate::sidebar::render::render_change_row` was the one place in the codebase that already
/// padded further than the bare `SCROLLBAR_SIZE` (`pr(px(10.0))`, i.e. `SCROLLBAR_SIZE + 2.0`),
/// but 2px of real clearance still reads as "touching" once anti-aliasing and hover states are
/// on screen (the collision the issue's screenshot shows). `+ 6.0` triples that margin to a
/// value that's genuinely, unambiguously distinct from the track - still compact enough to match
/// this UI's generally tight chrome (`theme::band::TREE_ROW` is 22px) - and is reused as the one
/// shared constant everywhere right-aligned content sits next to this scrollbar, instead of every
/// call site repeating its own guess at "close enough".
pub(crate) const CONTENT_CLEARANCE: f32 = SCROLLBAR_SIZE + 6.0;

/// One decoration mark on a vertical scrollbar's track (see this module's own docs) - a coloured
/// tick at `fraction` (`0.0` = the very top of the full document, `1.0` = the very bottom),
/// painted over the track underneath the thumb so it stays visible even while scrolled away from
/// it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarMark {
    pub fraction: f32,
    pub color: Rgba,
}

impl ScrollbarMark {
    pub(crate) fn new(fraction: f32, color: Rgba) -> Self {
        Self {
            fraction: fraction.clamp(0.0, 1.0),
            color,
        }
    }
}

/// See this module's own top docs for why this trait exists at all - it is the one real place
/// GPUI's several scrollable regions are treated as interchangeable.
pub(crate) trait ScrollableHandle: Clone + 'static {
    /// The scrolled region's own painted box, in window space.
    fn viewport_bounds(&self) -> gpui::Bounds<Pixels>;
    /// How far the content can scroll past the viewport - `0` when it does not overflow.
    fn max_scroll_offset(&self) -> gpui::Point<Pixels>;
    /// The current scroll offset. `y` is **negative** when scrolled down, matching
    /// `gpui::ScrollHandle`'s own convention.
    fn scroll_offset(&self) -> gpui::Point<Pixels>;
    fn set_scroll_offset(&self, offset: gpui::Point<Pixels>);
}

impl ScrollableHandle for ScrollHandle {
    fn viewport_bounds(&self) -> gpui::Bounds<Pixels> {
        self.bounds()
    }
    fn max_scroll_offset(&self) -> gpui::Point<Pixels> {
        self.max_offset()
    }
    fn scroll_offset(&self) -> gpui::Point<Pixels> {
        self.offset()
    }
    fn set_scroll_offset(&self, offset: gpui::Point<Pixels>) {
        self.set_offset(offset);
    }
}

impl ScrollableHandle for UniformListScrollHandle {
    fn viewport_bounds(&self) -> gpui::Bounds<Pixels> {
        self.0.borrow().base_handle.bounds()
    }
    fn max_scroll_offset(&self) -> gpui::Point<Pixels> {
        self.0.borrow().base_handle.max_offset()
    }
    fn scroll_offset(&self) -> gpui::Point<Pixels> {
        self.0.borrow().base_handle.offset()
    }
    fn set_scroll_offset(&self, offset: gpui::Point<Pixels>) {
        self.0.borrow().base_handle.set_offset(offset);
    }
}

impl ScrollableHandle for gpui::ListState {
    fn viewport_bounds(&self) -> gpui::Bounds<Pixels> {
        gpui::ListState::viewport_bounds(self)
    }
    fn max_scroll_offset(&self) -> gpui::Point<Pixels> {
        self.max_offset_for_scrollbar()
    }
    fn scroll_offset(&self) -> gpui::Point<Pixels> {
        self.scroll_px_offset_for_scrollbar()
    }
    fn set_scroll_offset(&self, offset: gpui::Point<Pixels>) {
        self.set_offset_from_scrollbar(offset);
    }
}

/// The invisible drag-ghost payload a scrollbar thumb's real `on_drag` starts - see this module's
/// own docs on why `id` is load-bearing once more than one scrollbar is mounted at once. Mirrors
/// `crate::root::resize::PaneResizeDrag`'s exact shape/reasoning. Vertical-only (see this module's
/// own docs on why no region in this app currently has real horizontal overflow to scroll) - a
/// second `axis` field would be real dead weight, not future-proofing, until one does.
#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
    id: &'static str,
}

impl gpui::Render for ScrollbarDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A real vertical overlay scrollbar for `handle`'s region - `None` when the region doesn't
/// currently overflow vertically (nothing to draw, nothing to hit-test). `id` must be a
/// literal unique to this call site (see this module's own docs on why `on_drag_move`
/// dispatch needs it). `marks` are the editor's real decoration ticks (empty for every other
/// region - see [`ScrollbarMark`]'s own docs).
pub(crate) fn render_vertical_scrollbar<T: 'static, H: ScrollableHandle>(
    id: &'static str,
    handle: &H,
    marks: &[ScrollbarMark],
    cx: &mut Context<T>,
) -> Option<AnyElement> {
    let bounds = handle.viewport_bounds();
    let viewport = bounds.size.height.as_f32();
    let max_offset = handle.max_scroll_offset().y.as_f32();
    if viewport <= 0.0 || max_offset <= 0.5 {
        return None;
    }
    let scrolled = (-handle.scroll_offset().y.as_f32()).clamp(0.0, max_offset);
    let thumb_len = geometry::thumb_length(viewport, max_offset);
    let thumb_top = geometry::thumb_position(viewport, max_offset, scrolled);

    let jump_handle = handle.clone();
    let drag_handle = handle.clone();

    Some(
        div()
            .id(id)
            // Test-only (a no-op outside test builds, like every other `debug_selector` in
            // this codebase) - lets a real render test read the track's own painted bounds
            // back with `VisualTestContext::debug_bounds` to assert genuine geometric
            // clearance (see `crate::sidebar::render`'s tests), rather than trusting a
            // padding value's *number* changed without proving what it actually clears.
            .debug_selector(move || id.to_string())
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(SCROLLBAR_SIZE))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_this, event: &MouseDownEvent, _window, cx| {
                    let viewport = jump_handle.viewport_bounds().size.height.as_f32();
                    let max_offset = jump_handle.max_scroll_offset().y.as_f32();
                    let track_top = jump_handle.viewport_bounds().origin.y.as_f32();
                    let new_scrolled = geometry::offset_for_pointer(
                        viewport,
                        max_offset,
                        track_top,
                        event.position.y.as_f32(),
                    );
                    let current = jump_handle.scroll_offset();
                    jump_handle.set_scroll_offset(gpui::point(current.x, px(-new_scrolled)));
                    cx.notify();
                }),
            )
            .children(marks.iter().map(|mark| {
                let top = (mark.fraction * viewport).clamp(0.0, (viewport - 2.0).max(0.0));
                div()
                    .absolute()
                    .top(px(top))
                    .right_0()
                    .w(px(SCROLLBAR_SIZE))
                    .h(px(2.0))
                    .bg(mark.color)
            }))
            .child(
                div()
                    .id(format!("{id}-thumb"))
                    .absolute()
                    .top(px(thumb_top + THUMB_INSET))
                    // The thumb carries a 2px transparent border so it floats 2px clear of
                    // the edge. Drawn directly rather than in CSS, that border is simply an
                    // inset on every edge - so the thumb is `WIDTH - 2 * INSET` wide and sits
                    // `INSET` in from the track's own edges. The track keeps its full
                    // `SCROLLBAR_SIZE` width and its own click-to-jump handler, so the region
                    // the pointer can act on is unchanged; only the painted bar is slimmer.
                    .right(theme::scrollbar::THUMB_INSET)
                    .w(px(SCROLLBAR_SIZE - 2.0 * THUMB_INSET))
                    .h(px(thumb_len - 2.0 * THUMB_INSET))
                    .rounded(theme::scrollbar::THUMB_RADIUS)
                    // Painted at full strength: §4p's `#2b3137` is already quieter than the
                    // greys these values replaced, so the old opacity multiplier would land
                    // somewhere the spec did not ask for.
                    .bg(theme::scrollbar::THUMB)
                    .hover(|el| el.bg(theme::scrollbar::THUMB_HOVER))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this, _event: &MouseDownEvent, _window, cx| {
                            // Swallows the mouse-down so it never also bubbles to the
                            // track's own handler above and jumps the view right before
                            // the drag below takes over - the same idiom
                            // `crate::root::resize::AdeApp::render_resize_handle` uses for
                            // its splitter.
                            cx.stop_propagation();
                        }),
                    )
                    .on_drag(ScrollbarDrag { id }, move |drag, _offset, _window, cx| {
                        cx.new(|_| *drag)
                    })
                    .on_drag_move(cx.listener(
                        move |_this, event: &DragMoveEvent<ScrollbarDrag>, _window, cx| {
                            let drag = event.drag(cx);
                            if drag.id != id {
                                return;
                            }
                            let viewport = drag_handle.viewport_bounds().size.height.as_f32();
                            let max_offset = drag_handle.max_scroll_offset().y.as_f32();
                            let track_top = drag_handle.viewport_bounds().origin.y.as_f32();
                            let new_scrolled = geometry::offset_for_pointer(
                                viewport,
                                max_offset,
                                track_top,
                                event.event.position.y.as_f32(),
                            );
                            let current = drag_handle.scroll_offset();
                            drag_handle
                                .set_scroll_offset(gpui::point(current.x, px(-new_scrolled)));
                            cx.notify();
                        },
                    )),
            )
            .into_any_element(),
    )
}

/// Pins this module's plain-`f32` geometry constants to the design tokens they mirror.
#[cfg(test)]
mod scrollbar_spec_tests {
    use super::*;

    #[test]
    fn the_local_size_constants_match_the_design_tokens() {
        assert_eq!(
            px(SCROLLBAR_SIZE),
            theme::scrollbar::WIDTH,
            "SCROLLBAR_SIZE has drifted from theme::scrollbar::WIDTH - §4p's scrollbar width is \
             defined once, in the theme layer"
        );
        assert_eq!(
            px(THUMB_INSET),
            theme::scrollbar::THUMB_INSET,
            "THUMB_INSET has drifted from theme::scrollbar::THUMB_INSET - §4p's 2px content-box \
             float is defined once, in the theme layer"
        );
    }
}
