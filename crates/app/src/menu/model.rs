//! The app's one menu, as pure data: what a menu row *is*, how rows group into separated
//! sections, and where the popover has to be drawn so it stays inside the window.

use crate::icons::Icon;

/// One rendered menu row: an action payload plus everything the popover paints for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry<A> {
    /// What running this row does - handed straight back to the surface's own dispatcher.
    pub action: A,
    /// The row's label.
    pub label: String,
    /// An optional leading glyph. Only the `⋯` overflow uses one today (§4u: History and Settings
    /// keep "the glyphs they had in the strip ... so the move out of the strip does not cost
    /// their recognisability"); a right-click row set is all text.
    pub glyph: Option<Icon>,
    /// The keystroke this row *also* has a real registered binding for, in
    /// `crate::keymap::resolve_combo`'s spec syntax (`"mod+shift+N"`) - `None` for the rows that
    /// are menu-only. Deliberately not a hard-coded keycap string: the keycap goes through the
    /// same per-platform resolution every other keycap in this app does, so a Ctrl/⌘ mismatch
    /// can't be introduced here, and a keycap can only name a binding that really exists.
    pub keystroke_spec: Option<&'static str>,
    /// The row's hint, shown as a real tooltip. `None` for a row whose label says everything.
    pub tooltip: Option<String>,
    /// Whether this row can be run right now.
    pub enabled: bool,
    /// Why a disabled row is disabled - `None` when it's enabled.
    pub disabled_reason: Option<String>,
    /// Whether this row mutates or destroys something - the rows the menu tints with the failure
    /// tint and hovers with [`crate::theme::surface::MENU_ROW_HOVER_DESTRUCTIVE`], so a
    /// destructive click is never visually identical to `Copy path`.
    pub destructive: bool,
}

impl<A> MenuEntry<A> {
    /// A plain, enabled row: an action and its label, nothing else.
    pub fn new(action: A, label: impl Into<String>) -> Self {
        MenuEntry {
            action,
            label: label.into(),
            glyph: None,
            keystroke_spec: None,
            tooltip: None,
            enabled: true,
            disabled_reason: None,
            destructive: false,
        }
    }

    /// Names the real, registered binding this row duplicates - see [`Self::keystroke_spec`].
    pub fn keys(mut self, spec: &'static str) -> Self {
        self.keystroke_spec = Some(spec);
        self
    }

    /// Attaches the row's hint.
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Attaches a leading glyph - see [`Self::glyph`].
    pub fn glyph(mut self, icon: Icon) -> Self {
        self.glyph = Some(icon);
        self
    }

    /// Marks the row destructive - see [`Self::destructive`].
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Gates the row on a real precondition, carrying the reason it is unavailable. A row that
    /// silently does nothing is worse than a visibly disabled one with its reason attached.
    pub fn gated(mut self, enabled: bool, reason: impl Into<String>) -> Self {
        self.enabled = enabled;
        self.disabled_reason = (!enabled).then(|| reason.into());
        self
    }
}

/// One thing the popover paints, top to bottom: a real action row, or the divider between two
/// groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuRow<A> {
    Item(MenuEntry<A>),
    Separator,
}

/// `groups` flattened with a [`MenuRow::Separator`] between each pair of adjacent groups - never a
/// leading or trailing one, and never one around a group that came out empty.
pub fn menu_rows<A>(groups: Vec<Vec<MenuEntry<A>>>) -> Vec<MenuRow<A>> {
    let mut rows = Vec::new();
    for group in groups {
        if group.is_empty() {
            continue;
        }
        if !rows.is_empty() {
            rows.push(MenuRow::Separator);
        }
        rows.extend(group.into_iter().map(MenuRow::Item));
    }
    rows
}

/// The popover's fixed width, in px. `REVISION-2026-08-14.md` §4, verbatim: "206 wide, flip above
/// when they would overflow the rail's bottom."
pub const MENU_WIDTH: f32 = 206.0;

/// One menu row's height, in px - §4t's "24px rows".
pub const MENU_ROW_HEIGHT: f32 = 24.0;

/// One group separator's whole vertical footprint, in px - a restatement of
/// `crate::root::widgets::MENU_GROUP_DIVIDER_HEIGHT` (its 1px rule plus its 4px margins top and
/// bottom), as a plain `f32` because this module is deliberately GPUI-free and that constant is
/// typed in `gpui::Pixels`. `crate::menu::render`'s
/// `the_shared_menu_paints_exactly_the_height_it_measures` is the real guard on the restatement:
/// it compares [`menu_height`] against the popover's *actually painted* bounds, so neither this
/// nor any other term here can drift from the element without failing.
pub const MENU_SEPARATOR_HEIGHT: f32 = 9.0;

/// The popover's own vertical padding (top + bottom together), in px.
pub const MENU_VERTICAL_PADDING: f32 = 8.0;

/// The smallest gap the menu keeps from a window edge, in px - so a menu pushed back inside the
/// window doesn't sit flush against the frame.
pub const MENU_EDGE_MARGIN: f32 = 4.0;

/// The popover's own 1px border, top and bottom. Part of the painted height because GPUI sizes
/// are border-box only for an *explicit* size; this panel's height comes from its children plus
/// its own padding and border, so a `menu_height` that omitted this was 2px short of what really
/// paints - which is exactly enough for a menu opened at the very bottom edge to overhang.
pub const MENU_BORDER: f32 = 2.0;

/// The gap the `⋯` overflow menu leaves between the button it hangs off and its own top edge, in
/// px.
pub const MENU_ANCHOR_GAP: f32 = 4.0;

/// The popover's real painted height for exactly the `rows` it is about to paint - action rows
/// and group dividers both, since [`crate::menu::render`] paints both and an edge flip computed
/// from the row count alone would be short by [`MENU_SEPARATOR_HEIGHT`] per group boundary.
pub fn menu_height<A>(rows: &[MenuRow<A>]) -> f32 {
    let painted: f32 = rows
        .iter()
        .map(|row| match row {
            MenuRow::Item(_) => MENU_ROW_HEIGHT,
            MenuRow::Separator => MENU_SEPARATOR_HEIGHT,
        })
        .sum();
    painted + MENU_VERTICAL_PADDING + MENU_BORDER
}

/// Where the popover's top-left corner must go so the whole menu stays inside a
/// `viewport_width` x `viewport_height` window, given the **pointer** it was opened from.
pub fn clamp_menu_origin(
    click_x: f32,
    click_y: f32,
    width: f32,
    height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> (f32, f32) {
    (
        place_axis(click_x, width, viewport_width),
        place_axis(click_y, height, viewport_height),
    )
}

fn place_axis(click: f32, size: f32, viewport: f32) -> f32 {
    let far_limit = viewport - size - MENU_EDGE_MARGIN;
    let mut origin = click;
    if origin > far_limit {
        // Flip to the other side of the click.
        origin = click - size;
    }
    // `far_limit` can be smaller than `MENU_EDGE_MARGIN` when the menu is larger than the
    // window; `max` last so the near edge always wins in that case (showing the menu's top-left,
    // which is where its first rows are, rather than its bottom-right).
    origin.min(far_limit).max(MENU_EDGE_MARGIN)
}

/// The three edges of the control a button-anchored menu hangs off, in window-space pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnchorRect {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Where a **button-anchored** menu's top-left corner goes: under the button it hangs off, with
/// their right edges aligned (`STAGE-A-CHANGELOG.md` §4w: "the overflow menu off the ⋯ button's
/// own rect with right edges aligned").
pub fn anchor_menu_below_button(
    button: AnchorRect,
    width: f32,
    height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> (f32, f32) {
    let x = (button.right - width)
        .min(viewport_width - width - MENU_EDGE_MARGIN)
        .max(MENU_EDGE_MARGIN);
    let below = button.bottom + MENU_ANCHOR_GAP;
    let y = if below + height > viewport_height - MENU_EDGE_MARGIN {
        button.top - MENU_ANCHOR_GAP - height
    } else {
        below
    };
    (
        x,
        y.min(viewport_height - height - MENU_EDGE_MARGIN)
            .max(MENU_EDGE_MARGIN),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A menu of `count` plain rows and no dividers - the sizing fixture the geometry tests use,
    /// so they stay about placement rather than about grouping.
    fn plain_rows(count: usize) -> Vec<MenuRow<u8>> {
        (0..count)
            .map(|i| MenuRow::Item(MenuEntry::new(i as u8, "row")))
            .collect()
    }

    #[test]
    fn a_menu_that_fits_opens_exactly_at_the_pointer() {
        let height = menu_height(&plain_rows(10));
        assert_eq!(
            clamp_menu_origin(100.0, 50.0, MENU_WIDTH, height, 1200.0, 800.0),
            (100.0, 50.0)
        );
    }

    #[test]
    fn a_click_near_the_right_or_bottom_edge_flips_the_menu_back_inside() {
        let height = menu_height(&plain_rows(10));
        let (x, y) = clamp_menu_origin(1190.0, 790.0, MENU_WIDTH, height, 1200.0, 800.0);
        assert_eq!(x, 1190.0 - MENU_WIDTH, "flips leftwards off the click");
        assert_eq!(y, 790.0 - height, "flips upwards off the click");
        assert!(x >= MENU_EDGE_MARGIN && x + MENU_WIDTH <= 1200.0);
        assert!(y >= MENU_EDGE_MARGIN && y + height <= 800.0);
    }

    #[test]
    fn a_menu_larger_than_the_window_clamps_to_the_near_edge() {
        let height = menu_height(&plain_rows(10));
        let (x, y) = clamp_menu_origin(20.0, 20.0, MENU_WIDTH, height, 100.0, 60.0);
        assert_eq!((x, y), (MENU_EDGE_MARGIN, MENU_EDGE_MARGIN));
    }

    #[test]
    fn a_flip_never_produces_a_negative_origin() {
        let height = menu_height(&plain_rows(4));
        let (x, y) = clamp_menu_origin(2.0, 2.0, MENU_WIDTH, height, 240.0, 60.0);
        assert!(x >= MENU_EDGE_MARGIN, "got {x}");
        assert!(y >= MENU_EDGE_MARGIN, "got {y}");
    }

    #[test]
    fn menu_height_is_the_panels_whole_painted_height() {
        assert_eq!(
            menu_height(&plain_rows(4)),
            4.0 * MENU_ROW_HEIGHT + MENU_VERTICAL_PADDING + MENU_BORDER,
            "every term `crate::menu::render` actually paints - rows, the panel's own py, and \
             its 1px border top and bottom"
        );
        assert_eq!(
            menu_height(&plain_rows(5)) - menu_height(&plain_rows(4)),
            MENU_ROW_HEIGHT,
            "and it must still track the real row count"
        );
        let mut with_one_divider = plain_rows(4);
        with_one_divider.insert(2, MenuRow::Separator);
        assert_eq!(
            menu_height(&with_one_divider) - menu_height(&plain_rows(4)),
            MENU_SEPARATOR_HEIGHT,
            "a divider is painted height too - a menu measured by row count alone would overhang \
             a window edge by 9px per group boundary"
        );
    }

    #[test]
    fn dividers_only_ever_sit_between_two_non_empty_groups() {
        let rows = menu_rows(vec![
            vec![MenuEntry::new(0u8, "a"), MenuEntry::new(1, "b")],
            vec![],
            vec![MenuEntry::new(2, "c")],
        ]);
        assert_eq!(
            rows,
            vec![
                MenuRow::Item(MenuEntry::new(0, "a")),
                MenuRow::Item(MenuEntry::new(1, "b")),
                MenuRow::Separator,
                MenuRow::Item(MenuEntry::new(2, "c")),
            ],
            "an empty group must not paint a rule with nothing under it, and the divider must \
             never lead or trail"
        );
        assert!(menu_rows::<u8>(vec![vec![], vec![]]).is_empty());
    }

    #[test]
    fn an_overflow_menu_hangs_under_its_button_with_right_edges_aligned() {
        let height = menu_height(&plain_rows(2));
        let button = AnchorRect {
            top: 10.0,
            right: 218.0,
            bottom: 38.0,
        };
        let (x, y) = anchor_menu_below_button(button, MENU_WIDTH, height, 1440.0, 900.0);
        assert_eq!(
            x + MENU_WIDTH,
            218.0,
            "the menu's right edge must land on the button's own right edge"
        );
        assert_eq!(y, 38.0 + MENU_ANCHOR_GAP, "and hang just under it");

        let low = AnchorRect {
            top: 860.0,
            right: 218.0,
            bottom: 888.0,
        };
        let (_, flipped) = anchor_menu_below_button(low, MENU_WIDTH, height, 1440.0, 900.0);
        assert_eq!(
            flipped + height,
            860.0 - MENU_ANCHOR_GAP,
            "near the foot it opens upwards from the button's top edge, never over the button"
        );
    }

    #[test]
    fn an_overflow_menu_stays_inside_the_window_at_either_edge() {
        let height = menu_height(&plain_rows(2));
        let at_right = AnchorRect {
            top: 10.0,
            right: 1440.0,
            bottom: 38.0,
        };
        let (x, _) = anchor_menu_below_button(at_right, MENU_WIDTH, height, 1440.0, 900.0);
        assert!(
            x + MENU_WIDTH <= 1440.0 - MENU_EDGE_MARGIN,
            "got {x}, which overhangs the right edge"
        );
        let at_left = AnchorRect {
            top: 10.0,
            right: 20.0,
            bottom: 38.0,
        };
        let (x, _) = anchor_menu_below_button(at_left, MENU_WIDTH, height, 1440.0, 900.0);
        assert!(x >= MENU_EDGE_MARGIN, "got {x}");
    }

    #[test]
    fn a_gated_row_carries_its_reason_and_only_when_disabled() {
        let open = MenuEntry::new(0u8, "Archive 2 agents").gated(true, "nothing to archive");
        assert!(open.enabled && open.disabled_reason.is_none());
        let shut = MenuEntry::new(0u8, "Archive 0 agents").gated(false, "nothing to archive");
        assert!(!shut.enabled);
        assert_eq!(shut.disabled_reason.as_deref(), Some("nothing to archive"));
    }
}
