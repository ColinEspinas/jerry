//! Pure thumb-geometry math for [`super::scrollbar`]'s real overlay scrollbars (GitHub issue
//! #30). Deliberately `gpui`-free (plain `f32`s, not `gpui::Pixels`), mirroring
//! `crate::root::layout`'s own split between pure clamp math and the GPUI call sites that
//! convert to/from `Pixels` - see that module's docs for the precedent this follows.

/// The smallest a thumb is ever allowed to shrink to, in pixels - without a floor, a very long
/// document (thousands of lines) would shrink the thumb into an unclickable sliver.
/// `vendor/zed/crates/gpui/examples/list_example.rs:40` uses the same real floor (`px(30.)`) for
/// the same reason; kept as its own named constant here (not `super::scrollbar`'s, to keep this
/// module callable without any GPUI import at all).
pub const MIN_THUMB_LENGTH: f32 = 28.0;

/// The thumb's length along the scroll axis, proportional to how much of the full document the
/// viewport currently shows (`viewport / content`), floored at [`MIN_THUMB_LENGTH`] and capped at
/// the viewport's own length (a document that fits without scrolling never reaches this function
/// at all - see [`super::scrollbar`]'s "not scrollable" gate - but the cap keeps this function
/// itself total for any input rather than relying on that caller-side guard).
pub fn thumb_length(viewport: f32, max_offset: f32) -> f32 {
    let viewport = viewport.max(0.0);
    let max_offset = max_offset.max(0.0);
    let content = viewport + max_offset;
    if content <= 0.0 {
        return viewport;
    }
    let raw = viewport * viewport / content;
    raw.max(MIN_THUMB_LENGTH.min(viewport)).min(viewport)
}

/// The thumb's top/left offset within the track, given the track's own length (equal to the
/// viewport length - the track spans the full visible region) and the current scroll amount
/// (`0.0` at the very top/left, `max_offset` at the very bottom/right - the *positive*-going
/// convention `scrollbar` itself uses, the sign-flipped mirror of `ScrollHandle::offset()`'s own
/// "more negative as you scroll down" convention - see that method's doc comment).
pub fn thumb_position(viewport: f32, max_offset: f32, scrolled: f32) -> f32 {
    if max_offset <= 0.0 {
        return 0.0;
    }
    let thumb = thumb_length(viewport, max_offset);
    let track = (viewport - thumb).max(0.0);
    let fraction = (scrolled / max_offset).clamp(0.0, 1.0);
    track * fraction
}

/// The new scroll amount (same positive-going `0.0..=max_offset` convention as
/// [`thumb_position`]) a click/drag at `pointer` (an absolute coordinate along the scroll axis)
/// should jump to, given the track's own start coordinate (`track_start`, e.g. the viewport's
/// `bounds.origin.y`) and length. Centers the thumb under the pointer (standard direct-manipulation
/// behaviour - "grab and drag" as well as "click anywhere on the track to jump there"), not a
/// VS-Code-style page-up/page-down step.
pub fn offset_for_pointer(viewport: f32, max_offset: f32, track_start: f32, pointer: f32) -> f32 {
    if max_offset <= 0.0 {
        return 0.0;
    }
    let thumb = thumb_length(viewport, max_offset);
    let track = (viewport - thumb).max(0.0);
    if track <= 0.0 {
        return 0.0;
    }
    let local = (pointer - track_start - thumb / 2.0).clamp(0.0, track);
    let fraction = local / track;
    (fraction * max_offset).clamp(0.0, max_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_fills_the_track_when_content_fits_without_scrolling() {
        assert_eq!(thumb_length(400.0, 0.0), 400.0);
    }

    #[test]
    fn thumb_shrinks_proportionally_to_how_much_of_the_document_is_visible() {
        // Viewport 400, document 1600 total (400 visible + 1200 more to scroll) -> the visible
        // fraction is 1/4, so the thumb should be 1/4 of the 400px track (100px) - well above the
        // floor, so the floor doesn't mask the proportional math here.
        assert_eq!(thumb_length(400.0, 1200.0), 100.0);
    }

    #[test]
    fn thumb_never_shrinks_below_the_documented_floor() {
        // An enormous document (100,000px more to scroll) against a 400px viewport would compute
        // a sub-pixel raw thumb length without the floor.
        assert_eq!(thumb_length(400.0, 100_000.0), MIN_THUMB_LENGTH);
    }

    #[test]
    fn thumb_never_exceeds_the_viewport_even_for_a_viewport_smaller_than_the_floor() {
        assert_eq!(thumb_length(10.0, 5.0), 10.0);
    }

    #[test]
    fn thumb_sits_at_the_track_top_when_unscrolled() {
        assert_eq!(thumb_position(400.0, 1200.0, 0.0), 0.0);
    }

    #[test]
    fn thumb_sits_at_the_track_bottom_when_scrolled_to_the_end() {
        let viewport = 400.0;
        let max_offset = 1200.0;
        let thumb = thumb_length(viewport, max_offset);
        let expected_track = viewport - thumb;
        assert_eq!(
            thumb_position(viewport, max_offset, max_offset),
            expected_track
        );
    }

    #[test]
    fn thumb_position_is_never_negative_or_past_the_track_even_for_out_of_range_input() {
        // A stale/overshot `scrolled` value (e.g. a frame where content just shrank) must not
        // send the thumb off-track - the same real bug class `list_example.rs`'s own
        // `bug_detected` assertion exists to catch, guarded against here instead by clamping.
        assert_eq!(thumb_position(400.0, 1200.0, -50.0), 0.0);
        let viewport = 400.0;
        let max_offset = 1200.0;
        let thumb = thumb_length(viewport, max_offset);
        assert_eq!(
            thumb_position(viewport, max_offset, 5_000.0),
            viewport - thumb
        );
    }

    #[test]
    fn an_unscrollable_region_has_no_meaningful_thumb_position() {
        assert_eq!(thumb_position(400.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn clicking_the_very_top_of_the_track_jumps_to_the_start() {
        let viewport = 400.0;
        let max_offset = 1200.0;
        let thumb = thumb_length(viewport, max_offset);
        assert_eq!(
            offset_for_pointer(viewport, max_offset, 0.0, thumb / 2.0),
            0.0
        );
    }

    #[test]
    fn clicking_the_very_bottom_of_the_track_jumps_to_the_end() {
        let viewport = 400.0;
        let max_offset = 1200.0;
        let thumb = thumb_length(viewport, max_offset);
        let track_start = 50.0; // a viewport that doesn't start at window y=0
        let pointer_at_bottom = track_start + viewport - thumb / 2.0;
        assert_eq!(
            offset_for_pointer(viewport, max_offset, track_start, pointer_at_bottom),
            max_offset
        );
    }

    #[test]
    fn clicking_above_or_below_the_track_clamps_rather_than_over_or_under_shooting() {
        let viewport = 400.0;
        let max_offset = 1200.0;
        assert_eq!(offset_for_pointer(viewport, max_offset, 0.0, -1_000.0), 0.0);
        assert_eq!(
            offset_for_pointer(viewport, max_offset, 0.0, 10_000.0),
            max_offset
        );
    }

    #[test]
    fn an_unscrollable_region_always_jumps_to_zero() {
        assert_eq!(offset_for_pointer(400.0, 0.0, 0.0, 200.0), 0.0);
    }
}
