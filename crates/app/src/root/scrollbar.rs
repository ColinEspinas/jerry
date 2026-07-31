//! Real, themed, overlay scrollbars (GitHub issue #30) for every scrollable region audited in
//! that issue: the file tree and Changes list (`crate::sidebar`), the code editor and merge
//! hand-edit buffer (`crate::code_surface`, `crate::merge`), Settings' nav/content columns
//! (`crate::settings`), the session rail's worktree list (`crate::rail`), the command palette's
//! result list (`crate::palette`), and the read-only diff view (`crate::code_surface::diff_view`).
//!
//! ## Why one shared component, not nine hand-copied ones
//!
//! Every one of those regions already scrolls via one of GPUI's two real scroll-state types -
//! `gpui::ScrollHandle` (a plain `div().overflow_y_scroll().track_scroll(&handle)`) or
//! `gpui::UniformListScrollHandle` (`uniform_list(...).track_scroll(&handle)`, used by every
//! virtualized list in this app - see e.g. `crate::sidebar::render::AdeApp::render_file_tree`'s own
//! docs on why virtualization matters here). Both expose the same real geometry
//! (`ScrollHandle::offset`/`max_offset`/`bounds`/`set_offset` -
//! `vendor/zed/crates/gpui/src/elements/div.rs:3938-4077`; `UniformListScrollHandle` simply wraps
//! one as its own `pub base_handle` field - `vendor/zed/crates/gpui/src/elements/
//! uniform_list.rs:80,116`), so [`ScrollableHandle`] below is the one, real, tiny adapter that
//! lets [`AdeApp::render_vertical_scrollbar`] draw a real overlay thumb against either kind
//! without a second, drifting implementation per region.
//! [`crate::root::scrollbar_geometry`] carries the actual thumb-length/position math, kept
//! `gpui`-free and unit-tested there directly (see that module's own docs) - written
//! axis-generically (`viewport`/`max_offset` along *a* scroll axis, not literally "vertical") even
//! though only the vertical direction has a real caller today, see this module's own "Audited but
//! not wired up" section below for why.
//!
//! ## Overlay, not reflow
//!
//! Every scrollbar this module renders is `.absolute()`, positioned against a `.relative()`
//! ancestor the caller already has (every scrollable region in this app is already the child of
//! a sized flex container) - so it paints *over* the last few pixels of content rather than
//! reserving a lane for itself and shrinking the scrollable area, which is GitHub issue #30's own
//! "overlay style so it doesn't shift layout" requirement. It is invisible whenever the region
//! genuinely doesn't overflow (`ScrollHandle::max_offset() <= 0`) - `None` is returned rather than
//! an empty/zero-size element, so a caller composes it with a plain `.children(...)` and pays
//! nothing extra when there's nothing to scroll.
//!
//! ## Real drag-to-scroll, not a decorative thumb
//!
//! A scrollbar thumb that only ever *displays* the scroll position without being draggable would
//! be exactly the "looks wired up but isn't" case `CONTRIBUTING.md` calls out - so the thumb is a
//! real GPUI drag target (`Interactivity::on_drag`/`on_drag_move`, the same mechanism
//! `crate::root::resize`'s pane-resize splitters already use - see that module's own docs), and the
//! track itself jumps the view to a clicked point. [`ScrollbarDrag`] carries a `&'static str`
//! identity (every real call site already has a literal id, e.g. `"file-tree"`) because
//! `on_drag_move`'s dispatch only matches on the *type* of the active drag
//! (`vendor/zed/crates/gpui/src/elements/div.rs:334-358`'s own real dispatch, verified directly:
//! it checks `drag.value.type_id() == TypeId::of::<T>()`, nothing about which element started
//! it) - with several scrollbars of the same `ScrollbarDrag` type mounted in the same frame (the
//! rail's worktree list and the code editor's own scrollbar are both on screen simultaneously),
//! every mounted scrollbar's `on_drag_move` listener fires for *any* active `ScrollbarDrag`, so
//! each one has to check the id itself before touching its own handle.
//!
//! ## Decoration marks
//!
//! [`ScrollbarMark`] is the editor scrollbar's real "decoration marks" requirement (GitHub issue
//! #30) - `crate::code_surface::file_view` builds one per diagnostic line
//! (`AdeApp::file_view_diagnostics`, real LSP diagnostics) and per git-changed line
//! (`AdeApp::file_view_changed_lines`, a real diff against the file on disk), plus one for the
//! real cursor line (`AdeApp::code_cursor`) - all genuine, already-computed state this app tracks
//! for other reasons (the inline diagnostic gutter, the git-gutter stripe, the blinking caret),
//! not new data invented for the scrollbar. A "search matches" mark is deliberately **not**
//! implemented: this app has no find-in-file feature anywhere yet (`grep -r "SearchMatch\|
//! find_in_file" crates/app/src` turns up nothing), and inventing a fake match set to paint marks
//! for would be exactly the "no simulated output" violation `CONTRIBUTING.md` warns against - see
//! `BUILD-LOG.md`'s entry for this issue for the honestly-documented gap.
//!
//! ## Audited but not wired up (honest gaps, not oversights)
//!
//! GitHub issue #30 also asks for the terminal, popups, and a horizontal scrollbar. Real, audited
//! reasons none of the three get one in this pass:
//! - **Terminal.** `crate::terminal::pane`/`crate::terminal::grid` render `alacritty_terminal`'s
//!   live cursor-addressed grid only (`grep -rn scroll crates/app/src/terminal` turns up nothing) -
//!   there is no scroll-*back* view to attach a scrollbar to at all yet. Building one is a real,
//!   separate feature (surfacing `alacritty_terminal`'s own scrollback buffer for rendering), not
//!   a styling change.
//! - **Popups.** `crate::lsp::completion_popup`'s own docs already say why: "this app has no
//!   virtualized/scrollable popover widget" - it hard-truncates at
//!   `MAX_RENDERED_COMPLETION_ITEMS` (12) instead of scrolling. Turning that into a real scrollable
//!   popup would also mean reworking `Self::move_completions_selection`'s keyboard-nav-into-view
//!   behaviour, not just adding a thumb - left as a follow-up rather than done as a rushed half
//!   measure alongside eight other regions in the same change.
//! - **Horizontal.** No region in this app has real horizontal content overflow today (`grep -rn
//!   overflow_x crates/app/src` matches nothing anywhere). The code editor's own rows are
//!   deliberately `.w_full()` (see `crate::code_surface::editing`'s own docs on why - a real,
//!   already-fixed click-to-position bug depends on it), so lines currently wrap/clip rather than
//!   overflow; giving it real horizontal scroll would mean reworking that width contract (GPUI does
//!   have the real primitive for it - `gpui::ListHorizontalSizingBehavior::Unconstrained`,
//!   verified directly against `vendor/zed/crates/gpui/src/elements/uniform_list.rs:634-650` - but
//!   plumbing it in safely is a separate change, not a scrollbar-styling one). A vertical-only
//!   component now is honest; a horizontal one with no real overflowing content anywhere to
//!   exercise it would just be untested, unreachable code.

use gpui::{AnyElement, Rgba, ScrollHandle};

use super::*;
use crate::root::scrollbar_geometry as geometry;

/// The scrollbar's own thickness (track + thumb width/height) - the hit target as well as the
/// visual width, kept slim to match this UI's generally compact chrome (`theme::band::TREE_ROW`
/// is 22px; a 12px `list_example.rs`-style scrollbar would visually dominate a row that short).
const SCROLLBAR_SIZE: f32 = 8.0;

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
/// `gpui::ScrollHandle` and `gpui::UniformListScrollHandle` are treated as interchangeable.
pub(crate) trait ScrollableHandle: Clone + 'static {
    fn base_handle(&self) -> ScrollHandle;
}

impl ScrollableHandle for ScrollHandle {
    fn base_handle(&self) -> ScrollHandle {
        self.clone()
    }
}

impl ScrollableHandle for UniformListScrollHandle {
    fn base_handle(&self) -> ScrollHandle {
        self.0.borrow().base_handle.clone()
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

impl AdeApp {
    /// A real vertical overlay scrollbar for `handle`'s region - `None` when the region doesn't
    /// currently overflow vertically (nothing to draw, nothing to hit-test). `id` must be a
    /// literal unique to this call site (see this module's own docs on why `on_drag_move`
    /// dispatch needs it). `marks` are the editor's real decoration ticks (empty for every other
    /// region - see [`ScrollbarMark`]'s own docs).
    pub(crate) fn render_vertical_scrollbar<H: ScrollableHandle>(
        &self,
        id: &'static str,
        handle: &H,
        marks: &[ScrollbarMark],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let base = handle.base_handle();
        let bounds = base.bounds();
        let viewport = bounds.size.height.as_f32();
        let max_offset = base.max_offset().y.as_f32();
        if viewport <= 0.0 || max_offset <= 0.5 {
            return None;
        }
        let scrolled = (-base.offset().y.as_f32()).clamp(0.0, max_offset);
        let thumb_len = geometry::thumb_length(viewport, max_offset);
        let thumb_top = geometry::thumb_position(viewport, max_offset, scrolled);

        let jump_handle = base.clone();
        let drag_handle = base.clone();

        Some(
            div()
                .id(id)
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(SCROLLBAR_SIZE))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, event: &MouseDownEvent, _window, cx| {
                        let viewport = jump_handle.bounds().size.height.as_f32();
                        let max_offset = jump_handle.max_offset().y.as_f32();
                        let track_top = jump_handle.bounds().origin.y.as_f32();
                        let new_scrolled = geometry::offset_for_pointer(
                            viewport,
                            max_offset,
                            track_top,
                            event.position.y.as_f32(),
                        );
                        let current = jump_handle.offset();
                        jump_handle.set_offset(gpui::point(current.x, px(-new_scrolled)));
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
                        .top(px(thumb_top))
                        .right_0()
                        .w(px(SCROLLBAR_SIZE))
                        .h(px(thumb_len))
                        .rounded(px(SCROLLBAR_SIZE / 2.0))
                        .bg(theme::scrollbar::THUMB.resolve().opacity(0.55))
                        .hover(|el| el.bg(theme::scrollbar::THUMB_HOVER.resolve().opacity(0.75)))
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
                                let viewport = drag_handle.bounds().size.height.as_f32();
                                let max_offset = drag_handle.max_offset().y.as_f32();
                                let track_top = drag_handle.bounds().origin.y.as_f32();
                                let new_scrolled = geometry::offset_for_pointer(
                                    viewport,
                                    max_offset,
                                    track_top,
                                    event.event.position.y.as_f32(),
                                );
                                let current = drag_handle.offset();
                                drag_handle.set_offset(gpui::point(current.x, px(-new_scrolled)));
                                cx.notify();
                            },
                        )),
                )
                .into_any_element(),
        )
    }
}
