//! Pure width-clamping logic for Zone 1/Zone 3's drag-to-resize splitters
//! (`design_handoff_jerry_ade/README.md`'s Layout table: rail "276 (range 240–340)", files/
//! changes panel "320 (260 in empty states)").

/// The rail's default width and adjustable range (README Layout table: "276 (range 240–340)").
pub const RAIL_DEFAULT: f32 = 276.0;
pub const RAIL_MIN: f32 = 240.0;
pub const RAIL_MAX: f32 = 340.0;

/// The files/changes panel's default width and adjustable range. The README gives two *fixed*
/// widths for two UI states (320 normally, 260 "in empty states") rather than an explicit
/// min/max - judgment call: the smaller, already-documented 260 is used as the floor, and the
/// ceiling is set generously while still leaving the centre a usable minimum width on the app's
/// 1440px design canvas.
pub const PANEL_DEFAULT: f32 = 320.0;
pub const PANEL_MIN: f32 = 260.0;
pub const PANEL_MAX: f32 = 480.0;

/// Clamps `width` to `[min, max]` - a named wrapper so call sites below read as "clamp a pane
/// width".
fn clamp_width(width: f32, min: f32, max: f32) -> f32 {
    width.clamp(min, max)
}

/// The rail's new width given the drag's absolute cursor x and the window body's left edge x -
/// the rail sits flush against the body's left edge, so its width is how far right of that edge
/// the cursor is, clamped to `[RAIL_MIN, RAIL_MAX]`.
pub fn rail_width_for_cursor(body_left: f32, cursor_x: f32) -> f32 {
    clamp_width(cursor_x - body_left, RAIL_MIN, RAIL_MAX)
}

/// The files/changes panel's new width given the drag's absolute cursor x and the window body's
/// right edge x - the panel sits flush against the body's right edge, so its width is how far
/// left of that edge the cursor is, clamped to `[PANEL_MIN, PANEL_MAX]`.
pub fn panel_width_for_cursor(body_right: f32, cursor_x: f32) -> f32 {
    clamp_width(body_right - cursor_x, PANEL_MIN, PANEL_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rail's width is how far right of the body's *own* left edge the cursor is, clamped to
    /// its documented range. The body deliberately does not start at window x=0 in the middle
    /// case, so a dropped `body_left` term cannot pass.
    #[test]
    fn rail_width_measures_from_the_bodys_left_edge_and_clamps_to_its_range() {
        for (body_left, cursor_x, expected, why) in [
            (
                0.0,
                300.0,
                300.0,
                "the plain distance from the body's left edge",
            ),
            (
                50.0,
                350.0,
                300.0,
                "measured from the body, not the window origin",
            ),
            (
                0.0,
                10_000.0,
                RAIL_MAX,
                "never wider than its documented maximum",
            ),
            (
                0.0,
                -10_000.0,
                RAIL_MIN,
                "never narrower than its documented minimum",
            ),
        ] {
            assert_eq!(
                rail_width_for_cursor(body_left, cursor_x),
                expected,
                "{why}"
            );
        }
    }

    /// The panel's handle sits on its *left* edge and the panel is flush against the body's right
    /// edge, so it grows as the cursor moves left - the mirror image of the rail's measurement,
    /// clamped to its own documented range.
    #[test]
    fn panel_width_measures_back_from_the_bodys_right_edge_and_clamps_to_its_range() {
        for (cursor_x, expected, why) in [
            (
                900.0,
                300.0,
                "the plain distance back from the body's right edge",
            ),
            (-10_000.0, PANEL_MAX, "never wider than its maximum"),
            (
                10_000.0,
                PANEL_MIN,
                "never narrower than its documented floor",
            ),
        ] {
            assert_eq!(panel_width_for_cursor(1200.0, cursor_x), expected, "{why}");
        }
    }
}
