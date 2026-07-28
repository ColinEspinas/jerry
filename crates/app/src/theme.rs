//! Jerry's design tokens, ported from `design_handoff_jerry_ade/tokens.rs` (the
//! design-review-approved colour/size constants transcribed from `Jerry.dc.html`, the
//! authoritative mockup - see `design_handoff_jerry_ade/README.md`).
//!
//! ## Why `Rgba`, not `Hsla`, is the constant's type
//!
//! GPUI's real color type for `.bg()`/`.text_color()`/`.border_color()` is `Hsla`
//! (`vendor/zed/crates/gpui/src/color.rs:334`), and `gpui::rgb(u32) -> Rgba`
//! (`color.rs:14`) is the real hex-to-color entry point, converted to `Hsla` via a real
//! `impl From<Rgba> for Hsla` (`color.rs:677`). Neither `rgb()` nor that `From` impl is
//! `const fn` (the former calls `u32::to_be_bytes`, the latter needs `f32::max`/`min`), so
//! `pub const ASK: Hsla = gpui::rgb(0x...).into();` does not compile. [`hex`] below
//! reimplements `rgb()`'s exact byte-extraction formula (no HSL math, so no `const`-unsafe
//! float comparisons needed) as a real `const fn`, producing a real compile-time `Rgba`
//! constant for every token; GPUI's own `Into<Hsla>`/`Into<Fill>` conversions (verified
//! against `.bg()`'s `impl Styled` bound at `vendor/zed/crates/gpui/src/styled.rs` and
//! `.text_color()`'s `impl Into<Hsla>` bound) then apply automatically, using GPUI's real
//! conversion math, at whichever call site actually renders a token - not a reimplementation
//! of RGB-to-HSL here.
//!
//! `Rgba` is `Copy` and has no `Drop`, so a struct literal of it is itself a valid `const`
//! expression - no `unsafe`, no runtime initialization (`once_cell`/`lazy_static`) needed.
//! This matches the shape `vendor/zed/crates/theme/src/styles/default_colors.rs` uses for
//! its own color constants (plain `const` values built from literal fields), just with an
//! `Rgba` byte triple standing in for that crate's direct `Hsla { h, s, l, a }` literals
//! (which come pre-computed from a design tool; these came from the mockup as hex).
//!
//! ## Module grouping
//!
//! Grouping and names are unchanged from `tokens.rs` (`surface`, `border`, `text`,
//! `status`, `diff`, `syntax`, `term`, `agent`, `lang`, `button`, `toggle`, `tag`,
//! `radius`, `band`, `zone`, `shadow`) so later phases can reference e.g.
//! `theme::status::ASK` exactly as `design_handoff_jerry_ade/README.md` does. Every
//! numeric literal below is copied unchanged from `tokens.rs` - this module only adapts
//! the *type*, never the *value*.
//!
//! `radius`/`band`/`zone` are typed as [`gpui::Pixels`] (via the real `const fn
//! gpui::px` at `vendor/zed/crates/gpui/src/geometry.rs:3736`) rather than bare `f32`,
//! since every one of their tokens is a pixel size GPUI's own sizing methods
//! (`.w()`, `.h()`, `.rounded()`, ...) consume directly as `Pixels`. `shadow` is typed as
//! `(Pixels, Pixels, Pixels)` for the same reason (`(x-offset, y-offset, blur-radius)`,
//! matching the CSS `box-shadow: <x> <y> <blur>` order the original comments describe).
//!
//! An added `font` module (not present in `tokens.rs`, which only covers colour) carries
//! the two bundled family names (see `crate::fonts`), so later phases have one place to
//! reference them from, matching this module's own `theme::font::SANS` shape.

use gpui::{px, Pixels, Rgba};

/// Reimplements `gpui::rgb`'s byte-extraction formula (see the module docs) as a real
/// `const fn`, so every token below is a genuine compile-time constant rather than a
/// runtime-initialized value.
const fn hex(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

pub mod surface {
    use super::{hex, Rgba};

    pub const WINDOW: Rgba = hex(0x0e0f11); // window body
    pub const WINDOW_BORDER: Rgba = hex(0x262a2e);
    pub const TITLE_BAR: Rgba = hex(0x101214);
    pub const RAIL: Rgba = hex(0x101113); // left rail + right panel
    pub const CENTER: Rgba = hex(0x131518); // work surface
    pub const PTY: Rgba = hex(0x0d0f11); // agent CLI + terminal
    pub const HEADER: Rgba = hex(0x121417); // context bar, panel headers
    pub const FOOTER: Rgba = hex(0x111316); // surface footers, status strips
    pub const CARD: Rgba = hex(0x161a1d); // composer, settings cards
    pub const CARD_SUNK: Rgba = hex(0x131619); // card footers
    pub const POPOVER: Rgba = hex(0x181c20); // completion popup, hover card
    pub const PALETTE: Rgba = hex(0x15181b);
    pub const SCRIM: Rgba = hex(0x060708); // at 62% alpha behind the palette
    pub const ROW_HOVER: Rgba = hex(0x15181b);
    pub const ROW_HOVER_ALT: Rgba = hex(0x1b1f22); // hover on chrome buttons
    pub const ROW_SELECTED: Rgba = hex(0x1a1e21);
    pub const SEGMENT_TRACK: Rgba = hex(0x171a1d);
    pub const SEGMENT_ACTIVE: Rgba = hex(0x242a2f);
    pub const KEYCAP: Rgba = hex(0x181c1f);
    pub const CHIP_NEUTRAL: Rgba = hex(0x23272b);
    pub const CURRENT_LINE: Rgba = hex(0x181c20);
}

pub mod border {
    use super::{hex, Rgba};

    pub const ZONE: Rgba = hex(0x1e2225); // between the three zones
    pub const INNER: Rgba = hex(0x1c2023); // between bands inside a zone
    pub const RAIL_INNER: Rgba = hex(0x191c1f);
    pub const ROW: Rgba = hex(0x171a1c); // change-list row separators
    pub const DIVIDER: Rgba = hex(0x22262a); // 1px vertical rules
    pub const CARD: Rgba = hex(0x23282c);
    pub const CARD_FIELD: Rgba = hex(0x22272b);
    pub const COMPOSER: Rgba = hex(0x24292e);
    pub const POPOVER: Rgba = hex(0x2b3238);
    pub const BUTTON: Rgba = hex(0x2a2f34); // outline button
    pub const BUTTON_DISABLED: Rgba = hex(0x1f2327);
    pub const KEYCAP: Rgba = hex(0x272c31);
    pub const SELECTED_EDGE: Rgba = hex(0x3f5b74); // 2px left edge on a selected row
}

pub mod text {
    use super::{hex, Rgba};

    pub const SELECTED: Rgba = hex(0xdde2e7);
    pub const PRIMARY: Rgba = hex(0xd3d8dd);
    pub const HEADING: Rgba = hex(0xc8cdd2);
    pub const STRONG: Rgba = hex(0xc2c7cc);
    pub const BODY: Rgba = hex(0xb8bfc6);
    pub const SECONDARY: Rgba = hex(0xa9b0b7);
    pub const MUTED: Rgba = hex(0x9aa1a8);
    pub const DIM: Rgba = hex(0x8b9197);
    pub const DIMMER: Rgba = hex(0x7d848b);
    pub const FAINT: Rgba = hex(0x6b7178);
    pub const FAINTER: Rgba = hex(0x5e646a);
    pub const GHOST: Rgba = hex(0x4e545a);
    pub const GHOSTER: Rgba = hex(0x454b51);
    pub const HINT: Rgba = hex(0x41464b);
    pub const GUTTER: Rgba = hex(0x3a3f44);
    pub const DISABLED: Rgba = hex(0x3d4248);
    /// The context bar's worktree path text specifically - `design_handoff_jerry_ade/
    /// README.md`'s "branch 11px mono `#8b9197` · worktree path 10.5px mono `#4a5057`" (its
    /// own distinct value, one hex step off [`GHOST`]'s `#4e545a`) - `tokens.rs`'s `text`
    /// module omits it (the same real gap [`super::button::GREEN_KEYCAP_FG`]'s docs describe
    /// for a different module: present in the HTML/README, missing from the transcribed
    /// token list), so it's added here directly rather than reusing the nearby-but-different
    /// [`GHOST`] or an unrelated module's identically-valued constant (`diff::FOLD_FG`).
    pub const PATH: Rgba = hex(0x4a5057);
}

/// Status is the only place colour carries meaning in the rail.
pub mod status {
    use super::{hex, Rgba};

    pub const ASK: Rgba = hex(0xe2a336); // needs input
    pub const ASK_BG: Rgba = hex(0x3a2c14);
    pub const FAIL: Rgba = hex(0xe0625c);
    pub const FAIL_BG: Rgba = hex(0x3a1e1e);
    pub const REVIEW: Rgba = hex(0x5cb87f);
    pub const REVIEW_BG: Rgba = hex(0x1e3b2a);
    pub const RUN: Rgba = hex(0x5a9ad4);
    pub const RUN_BG: Rgba = hex(0x1e2f3e);
    pub const IDLE: Rgba = hex(0x565d64);
    pub const IDLE_BG: Rgba = hex(0x22262a);
    // waiting-question preview inside a rail row
    pub const ASK_CARD_BG: Rgba = hex(0x1c1710);
    pub const ASK_CARD_EDGE: Rgba = hex(0x8a6420);
    pub const ASK_CARD_FG: Rgba = hex(0xc99b4e);
    // conflict banner
    pub const BANNER_BG: Rgba = hex(0x1b1610);
    pub const BANNER_BORDER: Rgba = hex(0x33291a);
}

pub mod diff {
    use super::{hex, Rgba};

    pub const ADD_BG: Rgba = hex(0x12211a);
    pub const ADD_FG: Rgba = hex(0x9fd0b2);
    pub const ADD_SIGN: Rgba = hex(0x4e8c68);
    pub const DEL_BG: Rgba = hex(0x211517);
    pub const DEL_FG: Rgba = hex(0xd6a4a0);
    pub const DEL_SIGN: Rgba = hex(0xa35f5b);
    pub const CTX_FG: Rgba = hex(0x868d94);
    pub const HUNK_BG: Rgba = hex(0x15181c);
    pub const HUNK_FG: Rgba = hex(0x5f666e);
    pub const FOLD_BG: Rgba = hex(0x121417);
    pub const FOLD_FG: Rgba = hex(0x4a5057);
    pub const STAT_ADD: Rgba = hex(0x5f9c78); // "+142" label
    pub const STAT_DEL: Rgba = hex(0xb06a66); // "-8" label
    pub const STAT_EMPTY: Rgba = hex(0x22262a); // unused segment of the 5-bar
    pub const GIT_GUTTER: Rgba = hex(0x2c6244); // 3px agent-touched marker
}

pub mod syntax {
    use super::{hex, Rgba};

    pub const TEXT: Rgba = hex(0xacb2be);
    pub const KEYWORD: Rgba = hex(0xb477cf);
    pub const FUNCTION: Rgba = hex(0x74ade8);
    pub const TYPE: Rgba = hex(0xdfc184);
    pub const LITERAL: Rgba = hex(0xbf956a); // numbers, `self`
    pub const COMMENT: Rgba = hex(0x5d636f);
    pub const CARET: Rgba = hex(0x5a9ad4);
    pub const ERROR_UNDERLINE: Rgba = hex(0xe0625c); // 2px dotted
    pub const HOVER_UNDERLINE: Rgba = hex(0x4d7ba8); // 1px solid
}

pub mod term {
    use super::{hex, Rgba};

    pub const PROMPT: Rgba = hex(0x8fbde6);
    pub const TEXT: Rgba = hex(0xa7adb4);
    pub const DIM: Rgba = hex(0x6b7178);
    pub const OK: Rgba = hex(0x6ab97f);
    pub const ERR: Rgba = hex(0xe0625c);
    pub const WARN: Rgba = hex(0xd8a94a);
    pub const HEADING: Rgba = hex(0xced4da);
    pub const ACTIVITY: Rgba = hex(0x5a9ad4); // spinner / progress line
    pub const MENU_SEL_FG: Rgba = hex(0xe0b263);
    pub const MENU_SEL_BG: Rgba = hex(0x1f1a10);
    pub const CURSOR: Rgba = hex(0x5a9ad4);
}

/// One tint per agent. Used on the rail badge, the CLI tab chip and the
/// conflict side headers, so a colour always means the same agent.
pub mod agent {
    use super::{hex, Rgba};

    pub const SONNET: (Rgba, Rgba) = (hex(0xd8a94a), hex(0x33280f)); // (fg, bg)
    pub const CODEX: (Rgba, Rgba) = (hex(0x6ab97f), hex(0x1e3327));
    pub const HAIKU: (Rgba, Rgba) = (hex(0xc98fbf), hex(0x332030));
    pub const LOCAL: (Rgba, Rgba) = (hex(0x7f9ad4), hex(0x1f2941));
}

/// Language chips, shared by the file tree, the code tab and the palette.
pub mod lang {
    use super::{hex, Rgba};

    pub const RS: (Rgba, Rgba) = (hex(0xc0824a), hex(0x2e2113)); // "rs"
    pub const TOML: (Rgba, Rgba) = (hex(0x8b9197), hex(0x23272b)); // "to"
    pub const MD: (Rgba, Rgba) = (hex(0x7f9ad4), hex(0x1d2532)); // "md"
    pub const SQL: (Rgba, Rgba) = (hex(0x6ab97f), hex(0x1b2a20)); // "sq"
    pub const UNKNOWN: (Rgba, Rgba) = (hex(0x6b7178), hex(0x23272b)); // "."
}

pub mod button {
    use super::{hex, Rgba};

    pub const GREEN_BG: Rgba = hex(0x24503a);
    pub const GREEN_BG_HOVER: Rgba = hex(0x2c6045);
    pub const GREEN_FG: Rgba = hex(0x9fdcb6);
    pub const GREEN_KEYCAP: Rgba = hex(0x376b4d);
    /// The keycap *glyph* colour inside a green primary button - `design_handoff_jerry_ade/
    /// README.md`'s "Keyboard affordances" section states this explicitly ("green
    /// `#376b4d`/`#8ac9a4`") and `Jerry.dc.html`'s own `AB.primaryG.keyFg` inline literal
    /// confirms it, but `design_handoff_jerry_ade/tokens.rs`'s `button` module omits it (only
    /// [`GREEN_KEYCAP`], the keycap *border*, is transcribed there) - added here directly from
    /// the HTML/README rather than left as an inline magic number at each Phase C call site.
    pub const GREEN_KEYCAP_FG: Rgba = hex(0x8ac9a4);
    // The equivalent blue keycap glyph colour (`#8fbde6`) needs no separate constant here -
    // it's the exact same value already ported as `term::PROMPT`.
    pub const BLUE_BG: Rgba = hex(0x243c50);
    pub const BLUE_BG_HOVER: Rgba = hex(0x2c4a63);
    pub const BLUE_FG: Rgba = hex(0xa5cdf0);
    pub const BLUE_KEYCAP: Rgba = hex(0x365b78);
    pub const AMBER_BG: Rgba = hex(0x3a2c14);
    pub const AMBER_BG_HOVER: Rgba = hex(0x4a3818);
    pub const AMBER_FG: Rgba = hex(0xe0b263);
    pub const DANGER_FG: Rgba = hex(0xc4726d);
    pub const DANGER_FG_HOVER: Rgba = hex(0xe3908b);
}

pub mod toggle {
    use super::{hex, Rgba};

    pub const TRACK_ON: Rgba = hex(0x2f6d4b);
    pub const TRACK_OFF: Rgba = hex(0x23272b);
    pub const KNOB_ON: Rgba = hex(0xc8ecd6);
    pub const KNOB_OFF: Rgba = hex(0x6b7178);
}

pub mod tag {
    use super::{hex, Rgba};

    pub const NEW: (Rgba, Rgba) = (hex(0x7fc79a), hex(0x1e3b2a));
    pub const DELETED: (Rgba, Rgba) = (hex(0xd18b86), hex(0x3a1e1e));
    pub const CONFLICT: (Rgba, Rgba) = (hex(0xe0b263), hex(0x3a2c14));
    pub const TREE_ADDED: Rgba = hex(0x5f9c78); // "A" mark
    pub const TREE_MODIFIED: Rgba = hex(0xa3873f); // "M" mark
}

pub mod radius {
    use super::{px, Pixels};

    pub const WINDOW: Pixels = px(10.0);
    pub const PANEL: Pixels = px(8.0); // palette
    pub const CARD: Pixels = px(6.0);
    pub const CARD_SM: Pixels = px(5.0);
    pub const BUTTON: Pixels = px(4.0);
    pub const CHIP: Pixels = px(3.0); // chips, keycaps, segments
    pub const MARK: Pixels = px(2.0); // stat bars, small squares
    pub const PILL: Pixels = px(8.0); // toggle track (26x15)
}

pub mod band {
    use super::{px, Pixels};

    pub const TITLE_BAR: Pixels = px(38.0);
    pub const TAB_STRIP: Pixels = px(34.0);
    pub const RAIL_HEADER: Pixels = px(36.0);
    pub const PANEL_HEADER: Pixels = px(36.0);
    pub const CONTEXT_BAR: Pixels = px(32.0);
    pub const DIFF_TOOLBAR: Pixels = px(31.0);
    pub const FILTER_ROW: Pixels = px(30.0);
    pub const SURFACE_FOOTER: Pixels = px(28.0);
    pub const PTY_HEADER: Pixels = px(27.0);
    pub const BREADCRUMB: Pixels = px(26.0);
    pub const STATUS_BAR: Pixels = px(26.0);
    pub const PALETTE_INPUT: Pixels = px(44.0);
    pub const PALETTE_ROW: Pixels = px(30.0);
    pub const CHANGE_ROW: Pixels = px(27.0);
    pub const TREE_ROW: Pixels = px(22.0);
    pub const KEYCAP: Pixels = px(15.0);
}

pub mod zone {
    use super::{px, Pixels};

    pub const RAIL_WIDTH: Pixels = px(276.0); // adjustable 240..=340
    pub const PANEL_WIDTH: Pixels = px(320.0);
    pub const PANEL_WIDTH_EMPTY: Pixels = px(260.0);
    pub const SETTINGS_NAV_WIDTH: Pixels = px(212.0);
    pub const PALETTE_WIDTH: Pixels = px(684.0);
    pub const COMPOSER_WIDTH: Pixels = px(560.0);
}

/// The only shadows in the product. Drop them if GPUI makes them awkward - the borders
/// carry the elevation on their own.
pub mod shadow {
    use super::{px, Pixels};

    pub const POPOVER: (Pixels, Pixels, Pixels) = (px(0.0), px(8.0), px(20.0)); // rgba(0,0,0,0.50)
    pub const PALETTE: (Pixels, Pixels, Pixels) = (px(0.0), px(12.0), px(34.0));
    // rgba(0,0,0,0.55)
}

/// The two bundled font families (see `crate::fonts`) - `design_handoff_jerry_ade/README.md`'s
/// "Design tokens" section: "Fonts: IBM Plex Sans (UI ...) and IBM Plex Mono (branches,
/// paths, diffs, terminal, code ...). Nothing else."
pub mod font {
    pub const SANS: &str = "IBM Plex Sans";
    pub const MONO: &str = "IBM Plex Mono";
}
