//! Real, themed, overlay scrollbars (GitHub issue #30) for every scrollable region audited in
//! that issue: the file tree and Changes list (`crate::sidebar`), the code editor and merge
//! hand-edit buffer (`crate::code_surface`, `crate::merge`), Settings' nav/content columns
//! (`crate::settings`), the agent rail's worktree list (`crate::rail`), the command palette's
//! result list (`crate::palette`), and the read-only diff view (`crate::code_surface::diff_view`).
//!
//! ## Why one shared component, not nine hand-copied ones
//!
//! Every one of those regions already scrolls via one of GPUI's three real scroll-state types -
//! `gpui::ScrollHandle` (a plain `div().overflow_y_scroll().track_scroll(&handle)`),
//! `gpui::UniformListScrollHandle` (`uniform_list(...).track_scroll(&handle)`, used by every
//! fixed-row-height virtualized list in this app - see e.g.
//! `crate::sidebar::render::AdeApp::render_file_tree`'s own docs on why virtualization matters
//! here), or `gpui::ListState` (`gpui::list(...)`, GPUI's own variable-row-height virtualized list -
//! used by the Changes panel's four sections, whose row heights genuinely differ, see
//! `crate::sidebar::sections`' own docs). All three expose the same real geometry - viewport
//! bounds, a maximum offset, a current offset and a setter - so [`ScrollableHandle`] below is the
//! one, real, tiny adapter that lets [`AdeApp::render_vertical_scrollbar`] draw a real overlay
//! thumb against any of them without a second, drifting implementation per region.
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
//! for would be exactly the "no simulated output" violation `CLAUDE.md` warns against, so it
//! stays an honestly-documented gap instead.
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
//! - **Popups.** Done as of GitHub issue #185, no longer a gap: the Completions popup's item list
//!   is a real virtualized `uniform_list` tracked by `AdeApp::completions_scroll_handle`, and it
//!   renders its own overlay scrollbar through [`AdeApp::render_vertical_scrollbar`] below
//!   (`"completions-scrollbar"`) exactly like every other region here. Its keyboard nav
//!   (`crate::lsp::completion_popup::AdeApp::move_completions_selection`) scrolls the viewport to
//!   follow the selection, which is what the old `MAX_RENDERED_COMPLETION_ITEMS` (12) render cap
//!   was standing in for. The Hover card and the Completions popup's own detail pane both have
//!   one now too, for the same real reason: a genuinely multi-line signature (a pretty-printed
//!   TypeScript utility/generic type) or a long doc comment can overflow their signature+doc
//!   region (`"hover-card-scrollbar"`/`AdeApp::hover_card_scroll_handle` and
//!   `"completions-detail-scrollbar"`/`AdeApp::completions_detail_scroll_handle` respectively) -
//!   before that region could overflow at all, nothing in either card
//!   could, which is why it had none. The remaining popovers (the plus menu, dropdown menus)
//!   still have no scroll region of their own - none of them has content that can overflow: each
//!   sizes to a fixed, bounded body.
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
/// visual width.
///
/// This used to be a local `8.0` chosen to match this UI's compact chrome, because there was no
/// scrollbar spec at all. There is one now, so the number tracks [`theme::scrollbar::WIDTH`]
/// along with the rest of that spec's table:
/// [`theme::scrollbar::THUMB_RADIUS`], [`theme::scrollbar::THUMB_INSET`], and a *transparent*
/// track. The thumb is inset on each side, so what is actually painted is `10 - 2*2 = 6px` wide,
/// visually slimmer than the old flush 8px bar, on a larger hit target.
///
/// This is a plain `f32` rather than the [`gpui::Pixels`] token itself only because
/// [`CONTENT_CLEARANCE`] below has to be a `const` and `Pixels`' inner field is crate-private to
/// GPUI, so it cannot be unwrapped in a const context.
/// `scrollbar_spec_tests::the_local_size_constants_match_the_design_tokens` asserts the two are
/// the same number, so they cannot drift.
const SCROLLBAR_SIZE: f32 = 10.0;

/// How far clear of the track's edges the painted thumb floats - STAGE-A-CHANGELOG.md §4p's "2px
/// transparent border". A plain `f32` mirror of [`theme::scrollbar::THUMB_INSET`], for the same
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
///
/// It used to be one method (`base_handle() -> ScrollHandle`), which worked while every scrollable
/// region in the app was either a plain `overflow_y_scroll` div or a `gpui::uniform_list` - both of
/// which really are a `gpui::ScrollHandle` underneath. `gpui::ListState` (GitHub issue #285: the
/// Changes panel's four sections are one scroller holding genuinely different row heights, which
/// `uniform_list` cannot represent - it sizes every slot from item 0) is **not**: it owns a
/// `SumTree` of measured item heights and exposes its scroll position through its own
/// `*_for_scrollbar` API, with no `ScrollHandle` to hand back.
///
/// So the trait is now the four operations the scrollbar actually performs, which all three can
/// answer honestly, rather than a concrete type two of them happen to share. Nothing about the
/// scrollbar itself changed - it is the same one control on all three regions.
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
///
/// A free function generic over `Context<T>` rather than an `AdeApp` method (GitHub issue
/// #331): every call site until that issue happened to be a method on `AdeApp` itself, so it
/// was originally written as one, taking an unused `&self` purely to be callable as
/// `self.render_vertical_scrollbar(...)` from those sites. `crate::terminal::pane::
/// TerminalPane` is a genuinely separate `Entity<TerminalPane>` with its own
/// `Context<TerminalPane>`, with no `AdeApp` to call this through, and the body never actually
/// touched `self` (both `cx.listener` closures below take `_this`, unused), so generalizing the
/// signature is a real behavior-preserving mechanical change, not a rework: every existing
/// `AdeApp` call site still type-checks unchanged (`T` is inferred from `cx`'s own type), just
/// spelled as a plain function call (`scrollbar::render_vertical_scrollbar(...)`) instead of a
/// method call.
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
                    // STAGE-A-CHANGELOG.md §4p: the thumb carries "a 2px transparent border
                    // and `background-clip:content-box` so it floats 2px clear of the edge".
                    // Drawn directly rather than in CSS, that transparent border is simply an
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
///
/// [`SCROLLBAR_SIZE`] and [`THUMB_INSET`] exist as bare `f32`s only because [`CONTENT_CLEARANCE`]
/// must be a `const` and `gpui::Pixels`' inner field is crate-private to GPUI, so the token cannot
/// be unwrapped in a const context. That is a real duplication, and the point of these assertions
/// is that it can never become a real *divergence*: `theme::scrollbar` stays the single authority
/// for STAGE-A-CHANGELOG.md §4p's numbers, and editing it there without editing it here fails.
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
