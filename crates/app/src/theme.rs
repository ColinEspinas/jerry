//! Jerry's design tokens, ported from `design_handoff_jerry_ade/tokens.rs` (colour/size
//! constants transcribed from the reviewed mockup `Jerry.dc.html`).
//!
//! Tokens are typed as [`Rgba`], not [`gpui::Hsla`]: GPUI's `rgb()` and its `From<Rgba> for
//! Hsla` impl are not `const fn` (`vendor/zed/crates/gpui/src/color.rs:14,677`), so a `const
//! Hsla` token wouldn't compile. [`hex`] reimplements `rgb()`'s byte-extraction formula as a
//! real `const fn` instead; GPUI's own `Into<Hsla>` conversions apply automatically wherever a
//! token is used.
//!
//! Module names (`surface`, `border`, `text`, `status`, `diff`, `syntax`, `term`, `agent`,
//! `lang`, `button`, `toggle`, `tag`, `radius`, `band`, `zone`, `shadow`, ...) match `tokens.rs`
//! so call sites can reference e.g. `theme::status::ASK` unchanged. `radius`/`band`/`zone` are
//! [`gpui::Pixels`] (via `gpui::px`, `vendor/zed/crates/gpui/src/geometry.rs:3736`) since GPUI's
//! sizing methods consume `Pixels` directly; `shadow` is `(Pixels, Pixels, Pixels)` for
//! `(x-offset, y-offset, blur-radius)`.
//!
//! `font` (not present in `tokens.rs`) carries the two bundled font family names - see
//! `crate::fonts`.

use gpui::{px, Pixels, Rgba};

/// Reimplements `gpui::rgb`'s byte-extraction formula (see the module docs) as a real `const
/// fn`, so every token below is a compile-time constant.
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
    /// The hint-size keycap's own background - distinct from [`KEYCAP`]'s standard-size
    /// `#181c1f` (`Jerry.dc.html`: `background:#15181a;border:1px solid #23272b`).
    pub const KEYCAP_HINT: Rgba = hex(0x15181a);
    pub const CHIP_NEUTRAL: Rgba = hex(0x23272b);
    pub const CURRENT_LINE: Rgba = hex(0x181c20);
    /// The Windows/Linux title bar's close caption button's hover fill (`Jerry.dc.html`:
    /// `style-hover="background:#8c3a38"`).
    pub const TITLE_BAR_CLOSE_HOVER: Rgba = hex(0x8c3a38);
    /// The tab strip's `+` menu popover row hover fill - distinct from [`ROW_HOVER`]/
    /// [`ROW_HOVER_ALT`].
    pub const PLUS_MENU_ROW_HOVER: Rgba = hex(0x1d2226);
    /// A file tab's close-affordance hover fill - one hex step off [`CHIP_NEUTRAL`]
    /// (`#23272b`), kept as its own token.
    pub const TAB_CLOSE_HOVER: Rgba = hex(0x23282c);
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
    /// The hint-size keycap's own border - see [`super::surface::KEYCAP_HINT`].
    pub const KEYCAP_HINT: Rgba = hex(0x23272b);
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
    /// The context bar's worktree path text (`README.md`: "worktree path 10.5px mono
    /// `#4a5057`") - one hex step off [`GHOST`]; not in `tokens.rs`'s `text` module, added
    /// here directly.
    pub const PATH: Rgba = hex(0x4a5057);
    /// The file tree row's `▾`/`▸` caret - same hex as [`PATH`] but a distinct token for a
    /// distinct element.
    pub const TREE_CARET: Rgba = hex(0x4a5057);
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

    /// The File view's Diagnostic-state row tint (`README.md`: "row tinted `#191416`") -
    /// distinct from [`super::surface::CURRENT_LINE`].
    pub const DIAGNOSTIC_ROW_BG: Rgba = hex(0x191416);
    /// The Diagnostic state's dim, end-of-line inline message text (`README.md`: `#6b4a48`).
    pub const DIAGNOSTIC_INLINE_MESSAGE: Rgba = hex(0x6b4a48);
    /// The Diagnostic state's card message text (`README.md`: `#e3908b`). Same hex as
    /// [`super::button::DANGER_FG_HOVER`], kept as its own token - unrelated elements that
    /// happen to share a designed red.
    pub const DIAGNOSTIC_CARD_MESSAGE: Rgba = hex(0xe3908b);
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
    /// A clickable path/`path:line` link inside terminal output (`Jerry.dc.html`:
    /// `color:#7fb4e3;border-bottom:1px dotted #3d6a91`).
    pub const LINK: Rgba = hex(0x7fb4e3);
    pub const LINK_UNDERLINE: Rgba = hex(0x3d6a91);
    /// The link's hover state (`Jerry.dc.html`: `style-hover="color:#a5cdf0;border-bottom:1px
    /// solid #78a8d0"`). Same value as [`super::button::BLUE_FG`], kept as its own token for a
    /// distinct element.
    pub const LINK_HOVER: Rgba = hex(0xa5cdf0);
    pub const LINK_UNDERLINE_HOVER: Rgba = hex(0x78a8d0);
}

/// The environment (WSL) chip's tokens - shown in the terminal footer, the status bar, and
/// Settings' `Default environment` row.
pub mod env {
    use super::{hex, Rgba};

    /// Same value as [`super::term::PROMPT`], reused directly (`Jerry.dc.html`'s
    /// `footRemoteFg` for `plat === 'windows'`).
    pub const WSL_FG: Rgba = super::term::PROMPT;
    pub const WSL_BG: Rgba = hex(0x16222c);
    pub const WSL_BORDER: Rgba = hex(0x24384a);
    /// Same value as [`super::text::FAINT`], reused directly.
    pub const LOCAL_FG: Rgba = super::text::FAINT;
    /// Same value as [`super::border::DIVIDER`], reused directly.
    pub const LOCAL_BORDER: Rgba = super::border::DIVIDER;
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
                                                                  // Verified directly against `design_handoff_jerry_ade/revision/tokens.rs:149-160`'s real
                                                                  // hex values, not paraphrased.
    pub const TS: (Rgba, Rgba) = (hex(0x6b9bd1), hex(0x1b2838)); // "ts"
    pub const VUE: (Rgba, Rgba) = (hex(0x5cb87f), hex(0x16261e)); // "vue"
    pub const PY: (Rgba, Rgba) = (hex(0xc9b04a), hex(0x2a2612)); // "py"
    pub const GO: (Rgba, Rgba) = (hex(0x5fa8c4), hex(0x152730)); // "go"
    pub const UNKNOWN: (Rgba, Rgba) = (hex(0x6b7178), hex(0x23272b)); // "."
}

pub mod button {
    use super::{hex, Rgba};

    pub const GREEN_BG: Rgba = hex(0x24503a);
    pub const GREEN_BG_HOVER: Rgba = hex(0x2c6045);
    pub const GREEN_FG: Rgba = hex(0x9fdcb6);
    pub const GREEN_KEYCAP: Rgba = hex(0x376b4d);
    /// The keycap glyph colour inside a green primary button (`README.md`/`Jerry.dc.html`:
    /// `#8ac9a4`) - not in `tokens.rs`'s `button` module (only [`GREEN_KEYCAP`], the border, is
    /// transcribed there), added here directly.
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

/// Settings-surface-only colours read directly from `Jerry.dc.html`'s inline literals for the
/// `settingsOpen` block - real values present in the mockup but missing from `tokens.rs`'s
/// transcription (predates the Settings section). Every other Settings colour reuses an
/// existing token from another module - see `crate::root`'s Settings render methods.
pub mod settings {
    use super::{hex, Rgba};

    /// A nav row's hover background (`Jerry.dc.html`: `style-hover="background:#17191b"`) -
    /// distinct from [`super::surface::ROW_HOVER`] (`#15181b`).
    pub const NAV_ROW_HOVER: Rgba = hex(0x17191b);
    /// The content column's page-subtitle text (`Jerry.dc.html`: `color:#767d84`) - close to
    /// but distinct from [`super::text::DIM`] (`#8b9197`).
    pub const SUBTITLE: Rgba = hex(0x767d84);
    /// A card row's own bottom separator (`Jerry.dc.html`: `border-bottom:1px solid #1f2327`) -
    /// distinct from [`super::border::CARD_FIELD`] (`#22272b`).
    pub const CARD_ROW_SEP: Rgba = hex(0x1f2327);
    /// A binary-found status dot on the Agents page. Same hex as [`super::status::REVIEW`],
    /// kept as its own token: the session `Status` palette is reserved for session urgency
    /// (`README.md`'s "Status vocabulary — use nowhere else"), and "this binary resolved on
    /// `$PATH`" is a different fact that just happens to want the same green.
    pub const AGENT_READY: Rgba = hex(0x5cb87f);
    /// A binary-not-found status dot on the Agents page - same reasoning as [`AGENT_READY`],
    /// same hex as [`super::status::FAIL`].
    pub const AGENT_NOT_FOUND: Rgba = hex(0xe0625c);
    /// The Worktrees page's "merged and prunable" row dot - distinct from
    /// [`super::status::IDLE`] (`#565d64`, used for the main checkout's own dot).
    pub const WORKTREE_PRUNABLE_DOT: Rgba = hex(0x3f454b);
    /// A selected Appearance-preview-card's / Theme-card's background - see
    /// [`CARD_UNSELECTED_BG`] for the unselected counterpart.
    pub const CARD_SELECTED_BG: Rgba = hex(0x161b1f);
    pub const CARD_UNSELECTED_BG: Rgba = hex(0x131619);
    /// A Theme card's hover border (`Jerry.dc.html`: `style-hover="border-color:#3a4148"`).
    pub const THEME_CARD_HOVER_BORDER: Rgba = hex(0x3a4148);
    /// The config snippet block's section-header line colour (`Jerry.dc.html`'s `CSFG.s`:
    /// `#c294e0`).
    pub const SNIPPET_SECTION: Rgba = hex(0xc294e0);
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
    /// The terminal pane's info footer band (`pid` · grid dimensions · environment chip ·
    /// right-aligned static copy) - distinct from [`SURFACE_FOOTER`] (the session-level
    /// Interrupt/Retry/Archive action footer, rendered separately below it).
    pub const PTY_INFO_FOOTER: Pixels = px(26.0);
    pub const BREADCRUMB: Pixels = px(26.0);
    /// 26 -> 28 (`CHANGELOG.md`'s change 7: "Height 26 -> 28").
    pub const STATUS_BAR: Pixels = px(28.0);
    pub const PALETTE_INPUT: Pixels = px(44.0);
    pub const PALETTE_ROW: Pixels = px(30.0);
    pub const CHANGE_ROW: Pixels = px(27.0);
    pub const TREE_ROW: Pixels = px(22.0);
    pub const KEYCAP: Pixels = px(15.0);
    /// The hint-size keycap's height.
    pub const KEYCAP_HINT: Pixels = px(14.0);
    /// The Windows/Linux title bar's menu row item height.
    pub const TITLE_BAR_MENU_ITEM: Pixels = px(22.0);
    /// One Windows/Linux caption button's width (minimise/maximise/close), pinned to the
    /// title bar's right edge.
    pub const TITLE_BAR_CAPTION_BUTTON: Pixels = px(44.0);
    /// The tab strip's `+` menu popover's row height.
    pub const PLUS_MENU_ROW: Pixels = px(29.0);
}

pub mod zone {
    use super::{px, Pixels};

    pub const RAIL_WIDTH: Pixels = px(276.0); // adjustable 240..=340
    pub const PANEL_WIDTH: Pixels = px(320.0);
    pub const PANEL_WIDTH_EMPTY: Pixels = px(260.0);
    pub const SETTINGS_NAV_WIDTH: Pixels = px(212.0);
    /// The Settings content column's cap - both the header block and the scrollable body share
    /// this `max_w`.
    pub const SETTINGS_CONTENT_MAX_WIDTH: Pixels = px(700.0);
    pub const PALETTE_WIDTH: Pixels = px(684.0);
    pub const COMPOSER_WIDTH: Pixels = px(560.0);
    /// The tab strip's `+` menu popover's width.
    pub const PLUS_MENU_WIDTH: Pixels = px(326.0);
}

/// The only shadows in the product. Drop them if GPUI makes them awkward - the borders
/// carry the elevation on their own.
pub mod shadow {
    use super::{px, Pixels};

    pub const POPOVER: (Pixels, Pixels, Pixels) = (px(0.0), px(8.0), px(20.0)); // rgba(0,0,0,0.50)
    pub const PALETTE: (Pixels, Pixels, Pixels) = (px(0.0), px(12.0), px(34.0));
    // rgba(0,0,0,0.55)
    /// The `+` menu popover's own shadow - distinct from [`PALETTE`]'s `0 12 34`.
    pub const PLUS_MENU: (Pixels, Pixels, Pixels) = (px(0.0), px(14.0), px(30.0));
    // rgba(0,0,0,0.55)
}

/// Honestly-scoped application of `Settings.appearance.interface_scale_percent` - text-size
/// scaling only, deliberately not padding/spacing/icon/fixed-chrome dimensions (retrofitting
/// every literal `Pixels` constant in this module to scale is out of scope). See
/// `crate::root::AdeApp::ui_text_size` for the render-side application, which chooses whether to
/// call [`scaled_px`] at each call site.
///
/// ## Which real surfaces read this
///
/// Scaled: the session rail (`crate::root::rail_render`); the title bar/status bar
/// (`crate::root::status_bar`); the command palette's row labels/hints
/// (`crate::root::palette_render`); the Files/Changes sidebar's row labels, footer hint, and
/// tree caret (`crate::root::sidebar_render`); the file/session tab strip's tab labels
/// (`crate::root::work_surface_render`); and every Settings row's label/hint *and* control
/// (stepper value, choice-segment labels, config banner text, snippet block text - all in
/// `crate::root::settings_widgets`).
///
/// Deliberately not scaled, each for its own reason: the code surface and terminal panes have
/// their own dedicated font-size mechanisms (`AdeApp::effective_code_rem_px`,
/// `Settings.appearance.terminal_font_size`) that a second multiplier would compound with;
/// chips/badges/keycaps/close-tab glyphs app-wide are small, fixed-size shapes the design treats
/// as part of a component rather than running text; and the rest of `work_surface_render`'s own
/// chrome (session context bar, toolbar buttons, `+` menu, footer action buttons) is real,
/// currently out of scope.
pub mod ui_scale {
    use super::px;
    use gpui::Pixels;

    /// Scales `base_px` by `scale_percent` (`100` = unchanged, `125` = 25% larger). Pure and
    /// `gpui::Context`-free so it's directly unit-testable without a live window.
    pub fn scaled_px(base_px: f32, scale_percent: u16) -> Pixels {
        px(base_px * (scale_percent as f32 / 100.0))
    }
}

/// The two bundled font families (see `crate::fonts`): IBM Plex Sans (UI) and IBM Plex Mono
/// (branches, paths, diffs, terminal, code).
pub mod font {
    pub const SANS: &str = "IBM Plex Sans";
    pub const MONO: &str = "IBM Plex Mono";
}

/// Palette-only (⌘K) colours read directly from `Jerry.dc.html`'s inline literals for the
/// `paletteOpen` block - real values missing from `tokens.rs`'s transcription (predates the
/// palette section).
pub mod palette {
    use super::{hex, Rgba};

    /// The input row's scope-prefix glyph (`Jerry.dc.html`: `color:#5f7f9e`).
    pub const PREFIX: Rgba = hex(0x5f7f9e);
    /// A result group's uppercase header label (`Jerry.dc.html`: `color:#5b6167`) - close to
    /// but distinct from [`super::text::FAINT`] (`#6b7178`).
    pub const GROUP_HEADER: Rgba = hex(0x5b6167);
    /// An unselected result row's hover background (`Jerry.dc.html`: `style-hover`:
    /// `background:#191d20`) - distinct from [`super::surface::ROW_HOVER`] (`#15181b`, which
    /// happens to equal the palette panel's own background, [`super::surface::PALETTE`]).
    pub const ROW_HOVER: Rgba = hex(0x191d20);
    /// The selected/first row's label colour (`Jerry.dc.html`: `fg: first ? '#e3e8ed' :
    /// '#c2c7cc'`) - one hex step brighter than [`super::text::SELECTED`] (`#dde2e7`).
    pub const LABEL_SELECTED: Rgba = hex(0xe3e8ed);
    /// A command result's kind chip `(fg, bg)` (`Jerry.dc.html`: `background:#1d2532` /
    /// `color:#7f9ad4`) - the same hex pair as [`super::lang::MD`], kept as its own token since
    /// a command chip and a Markdown-file chip are unrelated concepts.
    pub const COMMAND_CHIP: (Rgba, Rgba) = (hex(0x7f9ad4), hex(0x1d2532));
}

/// Revision R8's new `lang` chip tokens (item 6) - verified against the exact hex values in
/// `design_handoff_jerry_ade/revision/tokens.rs:149-160`, independently reconstructed from the
/// raw `u32` here rather than reusing [`hex`] (the same function under test), so a transcription
/// error in [`lang::TS`]/[`lang::VUE`]/[`lang::PY`]/[`lang::GO`] would actually be caught rather
/// than tautologically confirmed.
#[cfg(test)]
mod lang_token_tests {
    use super::{lang, Rgba};

    fn rgba_from_u32(v: u32) -> Rgba {
        Rgba {
            r: ((v >> 16) & 0xff) as f32 / 255.0,
            g: ((v >> 8) & 0xff) as f32 / 255.0,
            b: (v & 0xff) as f32 / 255.0,
            a: 1.0,
        }
    }

    // `Rgba` derives `PartialEq` but not `Debug` (`vendor/zed/crates/gpui/src/color.rs:37`), so
    // `assert_eq!`/`assert_ne!` can't be used directly - same reason
    // `crate::file_tree::tests::same` exists.
    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }
    fn same_pair(a: (Rgba, Rgba), b: (Rgba, Rgba)) -> bool {
        same(a.0, b.0) && same(a.1, b.1)
    }

    #[test]
    fn ts_matches_the_real_spec_d_hex_pair() {
        assert!(same_pair(
            lang::TS,
            (rgba_from_u32(0x6b9bd1), rgba_from_u32(0x1b2838))
        ));
    }

    #[test]
    fn vue_matches_the_real_spec_d_hex_pair() {
        assert!(same_pair(
            lang::VUE,
            (rgba_from_u32(0x5cb87f), rgba_from_u32(0x16261e))
        ));
    }

    #[test]
    fn py_matches_the_real_spec_d_hex_pair() {
        assert!(same_pair(
            lang::PY,
            (rgba_from_u32(0xc9b04a), rgba_from_u32(0x2a2612))
        ));
    }

    #[test]
    fn go_matches_the_real_spec_d_hex_pair() {
        assert!(same_pair(
            lang::GO,
            (rgba_from_u32(0x5fa8c4), rgba_from_u32(0x152730))
        ));
    }

    #[test]
    fn every_lang_chip_color_is_distinct_from_every_other() {
        let all = [
            ("rs", lang::RS),
            ("toml", lang::TOML),
            ("md", lang::MD),
            ("sql", lang::SQL),
            ("ts", lang::TS),
            ("vue", lang::VUE),
            ("py", lang::PY),
            ("go", lang::GO),
            ("unknown", lang::UNKNOWN),
        ];
        for (i, (name_a, color_a)) in all.iter().enumerate() {
            for (name_b, color_b) in all.iter().skip(i + 1) {
                assert!(
                    !same_pair(*color_a, *color_b),
                    "{name_a} and {name_b} should not share an identical (fg, bg) chip color"
                );
            }
        }
    }
}

#[cfg(test)]
mod ui_scale_tests {
    use super::px;
    use super::ui_scale::scaled_px;

    #[test]
    fn one_hundred_percent_is_a_real_no_op() {
        assert_eq!(scaled_px(12.0, 100), px(12.0));
    }

    #[test]
    fn scales_up_and_down_proportionally() {
        // `125`/`50` (not e.g. `90`) so the expected value is exactly representable in `f32`
        // and this stays an exact-equality check rather than needing an epsilon comparison.
        assert_eq!(scaled_px(12.0, 125), px(15.0));
        assert_eq!(scaled_px(12.0, 50), px(6.0));
    }
}
