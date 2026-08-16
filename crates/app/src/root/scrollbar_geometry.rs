//! Pure thumb-geometry math for [`super::scrollbar`]'s real overlay scrollbars (GitHub issue
//! #30). Deliberately `gpui`-free (plain `f32`s, not `gpui::Pixels`), mirroring
//! `crate::root::layout`'s own split between pure clamp math and the GPUI call sites that
//! convert to/from `Pixels` - see that module's docs for the precedent this follows.
//!
//! The three functions below are the same real relationship
//! `vendor/zed/crates/gpui/examples/list_example.rs` demonstrates against a live
//! `gpui::ListState` (`max_offset_for_scrollbar`/`scroll_px_offset_for_scrollbar`/
//! `viewport_bounds`), just factored out so it can be verified once, directly, without a live
//! GPUI window - and reused identically for both axes (vertical/horizontal) and both of GPUI's
//! real scroll-handle kinds (`gpui::ScrollHandle`, `gpui::UniformListScrollHandle`) rather than
//! four hand-copied variants of the same three formulas.

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
///
/// `content = viewport + max_offset` - GPUI's own `ScrollHandle::max_offset()` is "how much
/// further the view can scroll", not the content's total length, so the total is reconstructed
/// from the two the same way `list_example.rs`'s own `total_height = viewport_height +
/// max_offset` does.
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

    /// The proportional relationship, its floor, and its cap, in one table: the thumb is the
    /// visible fraction of the whole document, never shorter than [`MIN_THUMB_LENGTH`], and never
    /// longer than the viewport it lives in.
    #[test]
    fn thumb_length_is_the_visible_fraction_floored_and_capped() {
        for (viewport, max_offset, expected, why) in [
            (
                400.0,
                0.0,
                400.0,
                "content that fits without scrolling fills the track",
            ),
            (
                400.0,
                1200.0,
                100.0,
                "400 visible of 1600 total is a quarter of the track",
            ),
            (
                400.0,
                100_000.0,
                MIN_THUMB_LENGTH,
                "an enormous document would compute a sub-pixel thumb without the floor",
            ),
            (
                10.0,
                5.0,
                10.0,
                "and the floor never pushes past the viewport itself",
            ),
        ] {
            assert_eq!(thumb_length(viewport, max_offset), expected, "{why}");
        }
    }

    /// The thumb travels the whole track and stops at both ends - including for a stale or
    /// overshot `scrolled` value (e.g. a frame where the content just shrank), the same real bug
    /// class `list_example.rs`'s own `bug_detected` assertion exists to catch.
    #[test]
    fn thumb_position_spans_the_track_and_clamps_at_both_ends() {
        let (viewport, max_offset) = (400.0, 1200.0);
        let track = viewport - thumb_length(viewport, max_offset);

        for (scrolled, expected, why) in [
            (0.0, 0.0, "unscrolled sits at the track top"),
            (max_offset, track, "fully scrolled sits at the track bottom"),
            (
                -50.0,
                0.0,
                "a negative offset must not send the thumb off-track",
            ),
            (5_000.0, track, "and neither must an overshot one"),
        ] {
            assert_eq!(
                thumb_position(viewport, max_offset, scrolled),
                expected,
                "{why}"
            );
        }
        assert_eq!(
            thumb_position(400.0, 0.0, 0.0),
            0.0,
            "an unscrollable region has no meaningful thumb position"
        );
    }

    /// A click centers the thumb under the pointer and clamps to the track's own ends - measured
    /// from a `track_start` that is deliberately not the window origin, so a dropped
    /// `track_start` term cannot pass.
    #[test]
    fn a_click_centers_the_thumb_under_the_pointer_and_clamps_to_the_track() {
        let (viewport, max_offset, track_start) = (400.0, 1200.0, 50.0);
        let thumb = thumb_length(viewport, max_offset);

        for (pointer, expected, why) in [
            (
                track_start + thumb / 2.0,
                0.0,
                "the very top of the track jumps to the start",
            ),
            (
                track_start + viewport - thumb / 2.0,
                max_offset,
                "the very bottom jumps to the end",
            ),
            (
                -1_000.0,
                0.0,
                "a pointer above the track clamps rather than undershooting",
            ),
            (
                10_000.0,
                max_offset,
                "and one below it clamps rather than overshooting",
            ),
        ] {
            assert_eq!(
                offset_for_pointer(viewport, max_offset, track_start, pointer),
                expected,
                "{why}"
            );
        }
        assert_eq!(
            offset_for_pointer(400.0, 0.0, 0.0, 200.0),
            0.0,
            "an unscrollable region always jumps to zero"
        );
    }
}
