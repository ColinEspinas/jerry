//! Pure width-clamping logic for Zone 1/Zone 3's drag-to-resize splitters
//! (`design_handoff_jerry_ade/README.md`'s Layout table: rail "276 (range 240–340)", files/
//! changes panel "320 (260 in empty states)").
//!
//! Deliberately `gpui`-free (plain `f32`s, not `gpui::Pixels`) so the clamp math is directly
//! unit-testable without a window, mirroring `crate::work_surface::state`/`crate::rail::status`'s own
//! pure-logic split. `crate::root::AdeApp::apply_pane_resize` converts to/from `Pixels` at the
//! one GPUI call site that owns the drag.
//!
//! Widths are computed from the drag's *absolute* cursor position and the window body's bounds
//! on every `on_drag_move` tick, rather than a delta from an "armed" drag-start baseline that
//! would need explicit clearing on mouse-up - matching the equivalent precedent in
//! `vendor/zed/crates/workspace/src/workspace.rs`'s own dock-resize `on_drag_move` handler,
//! which computes `resize_left_dock(e.event.position.x - workspace.bounds.left(), ..)` directly
//! from the current event with no armed-drag baseline at all. This removes the "drag state left
//! dangling if the mouse is released outside the window" leak by construction.

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

    #[test]
    fn rail_width_is_the_cursors_distance_from_the_bodys_left_edge() {
        assert_eq!(rail_width_for_cursor(0.0, 300.0), 300.0);
    }

    #[test]
    fn rail_never_exceeds_its_documented_maximum() {
        assert_eq!(rail_width_for_cursor(0.0, 10_000.0), RAIL_MAX);
    }

    #[test]
    fn rail_never_drops_below_its_documented_minimum() {
        assert_eq!(rail_width_for_cursor(0.0, -10_000.0), RAIL_MIN);
    }

    #[test]
    fn rail_width_still_measures_from_a_body_thats_not_flush_with_the_window_origin() {
        // The body (the div `rail_width_for_cursor` is really measuring against) doesn't
        // itself start at window x=0 - confirm the offset is subtracted, not assumed away.
        assert_eq!(rail_width_for_cursor(50.0, 350.0), 300.0);
    }

    #[test]
    fn panel_width_is_the_cursors_distance_from_the_bodys_right_edge() {
        // The panel's handle sits on its *left* edge, and the panel itself is flush against
        // the body's right edge, so the panel grows as the cursor moves left (away from that
        // right edge) - the mirror image of the rail's left-edge measurement.
        assert_eq!(panel_width_for_cursor(1200.0, 900.0), 300.0);
    }

    #[test]
    fn panel_never_exceeds_its_maximum() {
        assert_eq!(panel_width_for_cursor(1200.0, -10_000.0), PANEL_MAX);
    }

    #[test]
    fn panel_never_drops_below_its_documented_floor() {
        assert_eq!(panel_width_for_cursor(1200.0, 10_000.0), PANEL_MIN);
    }
}
