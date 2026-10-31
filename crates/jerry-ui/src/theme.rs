//! The token tier every component in [`crate::components`] draws from: a handful of semantic
//! colours plus the non-colour scales (spacing, radius, motion, dimensions). No global state -
//! every component takes a `Theme` by value or reference from its caller, matching this crate's
//! "no globals" rule (`docs/architecture/decisions.md` §27).
//!
//! [`Theme::default`] is Jerry Dark, hand-copied from `crates/jerry-app/src/theme.rs`'s own
//! literal `ColorToken` defaults so the migration in that crate is visually neutral - each field
//! below cites the exact token it mirrors. `jerry-app` is expected to construct its own `Theme`
//! from the *live* resolved palette (`crate::theme::ColorToken::resolve`) rather than reach for
//! this default directly, so a custom theme file still repaints every migrated component; the
//! default only matters for this crate's own tests and for a caller with no live palette.

use std::time::Duration;

use gpui::{px, Hsla, Pixels, Rgba};

/// `0xrrggbb` -> a real, opaque [`Hsla`] - the same bit-for-bit conversion
/// `crates/jerry-app/src/theme.rs`'s own `hex_rgba` performs before every `ColorToken` default,
/// kept local so this crate never needs an `Rgba` field of its own at rest.
const fn hex(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// The semantic colour tier - what every component reads instead of a raw hex literal. Each
/// field's doc comment names the exact `crates/jerry-app/src/theme.rs` token its [`Theme::default`]
/// value mirrors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    /// Mirrors `theme::surface::CARD` (`0x161a1d`) - an ordinary elevated content surface.
    pub surface: Hsla,
    /// Mirrors `theme::surface::POPOVER` (`0x181c20`) - a surface raised a step further above
    /// [`Self::surface`] (a popover, or a card sitting on a card).
    pub surface_raised: Hsla,
    /// Mirrors `theme::surface::ROW_HOVER_ALT` (`0x1b1f22`) - the hover fill chrome
    /// buttons/rows use.
    pub surface_hover: Hsla,
    /// Mirrors `theme::surface::ROW_SELECTED` (`0x1a1e21`).
    pub surface_selected: Hsla,
    /// Mirrors `theme::border::BUTTON` (`0x2a2f34`) - an outline button's resting border.
    pub border: Hsla,
    /// Mirrors `theme::border::BUTTON_DISABLED` (`0x1f2327`).
    pub border_disabled: Hsla,
    /// Mirrors `theme::text::PRIMARY` (`0xd3d8dd`).
    pub text: Hsla,
    /// Mirrors `theme::text::MUTED` (`0x9aa1a8`).
    pub text_muted: Hsla,
    /// Mirrors `theme::text::GHOSTER` (`0x454b51`) - the disabled "Open file" label's colour.
    pub text_disabled: Hsla,
    /// Mirrors `theme::text::SELECTED` (`0xdde2e7`) - a light neutral readable on [`Self::accent`].
    pub text_on_accent: Hsla,
    /// Mirrors `theme::SEED_REFERENCE_ACCENT`/`theme::syntax::FUNCTION` (`0x74ade8`), documented
    /// there as "Jerry Dark's own real accent blue".
    pub accent: Hsla,
    /// Mirrors `theme::button::BLUE_BG_HOVER` (`0x2c4a63`).
    pub accent_hover: Hsla,
    pub status: StatusColors,
}

/// Mirrors `crates/jerry-app/src/theme.rs`'s own `status` module - the only place colour carries
/// meaning beyond decoration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusColors {
    /// Mirrors `theme::status::REVIEW` (`0x5cb87f`).
    pub ok: Hsla,
    /// Mirrors `theme::status::REVIEW_BG` (`0x1e3b2a`).
    pub ok_bg: Hsla,
    /// Mirrors `theme::status::ASK` (`0xe2a336`).
    pub warn: Hsla,
    /// Mirrors `theme::status::ASK_BG` (`0x3a2c14`).
    pub warn_bg: Hsla,
    /// Mirrors `theme::status::FAIL` (`0xe0625c`).
    pub fail: Hsla,
    /// Mirrors `theme::status::FAIL_BG` (`0x3a1e1e`).
    pub fail_bg: Hsla,
    /// Mirrors `theme::status::RUN` (`0x5a9ad4`).
    pub info: Hsla,
    /// Mirrors `theme::status::RUN_BG` (`0x1e2f3e`).
    pub info_bg: Hsla,
}

/// A four-step spacing scale (padding/gap), in `Pixels`. Values are the most frequently repeated
/// gap/padding literals already in `crates/jerry-app`'s render code (`px(4.0)`, `px(6.0)`,
/// `px(8.0)`/`px(10.0)`, `px(16.0)`) - a real, observed scale, not an invented one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    pub xs: Pixels,
    pub sm: Pixels,
    pub md: Pixels,
    pub lg: Pixels,
    pub xl: Pixels,
}

/// Mirrors `crates/jerry-app/src/theme.rs`'s own `radius` module exactly (same four literal
/// values), so a component built here and a hand-rolled `div()` still round the same amount.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radius {
    /// Mirrors `theme::radius::CHIP` (`3px`).
    pub sm: Pixels,
    /// Mirrors `theme::radius::BUTTON` (`4px`).
    pub md: Pixels,
    /// Mirrors `theme::radius::CARD` (`6px`).
    pub lg: Pixels,
    /// Mirrors `theme::radius::PILL` (`8px`).
    pub pill: Pixels,
}

/// Transition durations for hover/selection state changes. `jerry-app` renders every state
/// change as an instant repaint today (no call site animates a style transition), so nothing yet
/// reads these - they exist so a component built against this tier has a real, shared duration
/// to reach for the day one does, rather than each component picking its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    pub fast: Duration,
    pub normal: Duration,
    pub slow: Duration,
}

/// Shared layout dimensions a component needs to know to lay itself out consistently with the
/// rest of the window chrome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dimensions {
    /// Mirrors `theme::zone::RAIL_WIDTH` (`276px`).
    pub rail_width: Pixels,
    /// Mirrors `theme::band::CHROME_HEADER` (`36px`) - shared by the work-surface tab strip, the
    /// rail's sidebar strip, and the files/changes panel header.
    pub tab_height: Pixels,
}

/// The full token tier a component renders against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub colors: Colors,
    pub spacing: Spacing,
    pub radius: Radius,
    pub motion: Motion,
    pub dimensions: Dimensions,
}

impl Default for Theme {
    /// Jerry Dark - see this module's own docs for why every value here is a citation, not a
    /// guess.
    fn default() -> Self {
        Theme {
            colors: Colors {
                surface: hex(0x161a1d).into(),
                surface_raised: hex(0x181c20).into(),
                surface_hover: hex(0x1b1f22).into(),
                surface_selected: hex(0x1a1e21).into(),
                border: hex(0x2a2f34).into(),
                border_disabled: hex(0x1f2327).into(),
                text: hex(0xd3d8dd).into(),
                text_muted: hex(0x9aa1a8).into(),
                text_disabled: hex(0x454b51).into(),
                text_on_accent: hex(0xdde2e7).into(),
                accent: hex(0x74ade8).into(),
                accent_hover: hex(0x2c4a63).into(),
                status: StatusColors {
                    ok: hex(0x5cb87f).into(),
                    ok_bg: hex(0x1e3b2a).into(),
                    warn: hex(0xe2a336).into(),
                    warn_bg: hex(0x3a2c14).into(),
                    fail: hex(0xe0625c).into(),
                    fail_bg: hex(0x3a1e1e).into(),
                    info: hex(0x5a9ad4).into(),
                    info_bg: hex(0x1e2f3e).into(),
                },
            },
            spacing: Spacing {
                xs: px(4.0),
                sm: px(6.0),
                md: px(8.0),
                lg: px(10.0),
                xl: px(16.0),
            },
            radius: Radius {
                sm: px(3.0),
                md: px(4.0),
                lg: px(6.0),
                pill: px(8.0),
            },
            motion: Motion {
                fast: Duration::from_millis(100),
                normal: Duration::from_millis(150),
                slow: Duration::from_millis(250),
            },
            dimensions: Dimensions {
                rail_width: px(276.0),
                tab_height: px(36.0),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Theme;

    #[test]
    fn the_spacing_scale_is_strictly_increasing() {
        let spacing = Theme::default().spacing;
        let steps = [spacing.xs, spacing.sm, spacing.md, spacing.lg, spacing.xl];
        for pair in steps.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} should be less than {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_radius_scale_is_strictly_increasing() {
        let radius = Theme::default().radius;
        let steps = [radius.sm, radius.md, radius.lg, radius.pill];
        for pair in steps.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} should be less than {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_motion_scale_is_strictly_increasing() {
        let motion = Theme::default().motion;
        assert!(motion.fast < motion.normal);
        assert!(motion.normal < motion.slow);
    }

    /// A theme is `Copy` - every component takes it by value without forcing its caller to
    /// clone or hold a lock, matching the "no globals, just pass it in" rule.
    #[test]
    fn theme_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Theme>();
    }
}
