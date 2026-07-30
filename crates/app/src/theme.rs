//! Jerry's design tokens, ported from `design_handoff_jerry_ade/tokens.rs` (colour/size
//! constants transcribed from the reviewed mockup `Jerry.dc.html`).
//!
//! ## Runtime-swappable colour tokens ([`ColorToken`])
//!
//! Every colour constant below (`surface::WINDOW`, `text::BODY`, `syntax::KEYWORD`, ...) is a
//! [`ColorToken`], not a plain [`Rgba`] - a real, compile-time `const` (so it can still appear
//! inside another `const`, e.g. `crate::language::EXTENSIONS`'s `chip_colors` field - see that
//! module's own docs) that carries Jerry Dark's own original [`Rgba`] value plus, at the point
//! something actually asks for a real colour (`ColorToken::resolve`, or the `Into<Hsla>`/
//! `Into<Rgba>` impls every GPUI builder method already accepts), applies whichever theme is
//! currently selected via [`current_theme_index`] - see that function's own docs for why a
//! global atomic index, not a value threaded through every render call, is the real mechanism.
//! Jerry Dark itself (`current_theme_index() == 0`) is the identity case: `resolve()` returns
//! the token's own original value completely unchanged - not even a lossy HSL round-trip - so
//! every existing exact-hex test (`lang_token_tests`, etc.) keeps passing bit-for-bit.
//!
//! The other five themes' full palettes are *derived*, not hand-authored - see
//! [`derive_shift`]'s own docs for the real, systematic HSL transform, computed from each
//! theme's own five `crate::settings::THEME_DEFS` swatches compared against Jerry Dark's.
//!
//! [`hex`] still reimplements `gpui::rgb`'s byte-extraction formula as a real `const fn`
//! (GPUI's own `rgb()`/`Into<Hsla>` conversions aren't `const fn` -
//! `vendor/zed/crates/gpui/src/color.rs:14,677` - so a literal `const Hsla`/`const Rgba` token
//! wouldn't compile), now wrapped straight into a [`ColorToken`].
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

use gpui::{px, Hsla, Pixels, Rgba};

/// Reimplements `gpui::rgb`'s byte-extraction formula (see the module docs) as a real `const
/// fn`, so every token below is a compile-time constant.
const fn hex(v: u32) -> ColorToken {
    ColorToken(Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a: 1.0,
    })
}

/// A design token's real Jerry Dark colour, resolved against whichever theme is currently active
/// only at the point something actually renders it - see the module docs' "Runtime-swappable
/// colour tokens" section. `Copy`/`const`-constructible so it's a drop-in replacement for the
/// plain `Rgba` every existing token used to be: a bare `theme::surface::WINDOW` still works
/// unchanged at every GPUI builder call site (`.bg(...)`, `.text_color(...)`, ...) via
/// [`Into<Hsla>`]/[`Into<Rgba>`] below, and still works inside another `const` definition (it's
/// itself real `const`-evaluable, unlike a function call would be).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorToken(pub Rgba);

impl ColorToken {
    /// Resolves this token against [`current_theme_index`]'s real, live-selected theme. Jerry
    /// Dark (`index == 0`) returns [`Self`]'s own original value completely unchanged, not even
    /// a lossy `Rgba -> Hsla -> Rgba` round trip - see the module docs for why this matters for
    /// existing exact-hex tests.
    pub fn resolve(self) -> Rgba {
        let index = current_theme_index();
        if index == 0 {
            return self.0;
        }
        apply_shift(self.0, theme_shift(index))
    }
}

impl From<ColorToken> for Rgba {
    fn from(token: ColorToken) -> Rgba {
        token.resolve()
    }
}

impl From<ColorToken> for Hsla {
    fn from(token: ColorToken) -> Hsla {
        token.resolve().into()
    }
}

/// `Styled::bg`/`Styled::border_color`-style GPUI builder methods take `impl Into<gpui::Fill>`,
/// not `impl Into<Hsla>` directly (`vendor/zed/crates/gpui/src/styled.rs:492`) - `Fill` has its
/// own real `From<Hsla>`/`From<Rgba>` impls (`vendor/zed/crates/gpui/src/style.rs:871,877`) that
/// [`From<ColorToken> for Hsla`] above doesn't automatically chain into (`Into` isn't
/// transitive), so every `.bg(theme::surface::WINDOW)`-style call site across the app needs this
/// real, direct impl too.
impl From<ColorToken> for gpui::Fill {
    fn from(token: ColorToken) -> gpui::Fill {
        token.resolve().into()
    }
}

/// `Window::fill` (real, low-level `gpui::PaintQuad` painting, `vendor/zed/crates/gpui/src/
/// window.rs:6644` - used by this app's own `canvas`-based custom-drawn elements, e.g. the File
/// view's real caret) takes `impl Into<gpui::Background>`, a third distinct GPUI colour-sink
/// type alongside `Hsla`/`Fill` above, for the same "`Into` isn't transitive" reason.
impl From<ColorToken> for gpui::Background {
    fn from(token: ColorToken) -> gpui::Background {
        token.resolve().into()
    }
}

thread_local! {
    /// The live-selected theme's index into `crate::settings::THEME_DEFS` (`0` = Jerry Dark, the
    /// real default and identity case - see [`ColorToken::resolve`]). A [`std::thread_local`],
    /// not a value threaded through every render call's parameters, by deliberate design: every
    /// one of this module's ~200 colour tokens is a bare, freestanding `const` read from dozens
    /// of files across the whole app (`theme::surface::WINDOW`, `theme::text::BODY`, ...), the
    /// overwhelming majority several layers deep inside plain, `gpui`-context-free helper
    /// functions (`crate::changes::stat_segment_color`, `crate::status::Status::color`, ...) that
    /// have no `AdeApp`/`Context`/theme parameter to receive a selection through, and adding one
    /// to every such signature across the codebase (the exact churn `crate::root::AdeApp::
    /// ui_text_size`'s own narrower, opt-in scaling mechanism was deliberately kept away from,
    /// per that function's own docs) would be a materially larger, riskier change than this
    /// app's actual, real architecture needs.
    ///
    /// This started as a plain process-global `AtomicUsize` instead of a `thread_local!`
    /// (reasoning: "nothing in this app ever wants a different selected theme on a different
    /// thread", true for the real, single-foreground-thread production app per `vendor/zed/
    /// CLAUDE.md`'s own "All use of entities and UI rendering occurs on a single foreground
    /// thread" note) - a real audit caught the flaw in that reasoning: `cargo test`'s default
    /// (parallel) mode runs each `#[gpui::test]` on its *own* OS thread, and since essentially
    /// every test in this crate constructs at least one `AdeApp` (whose constructor always calls
    /// `Self::apply_theme_selection`, writing this value), a single shared global meant any test
    /// asserting a non-default resolved colour could be corrupted mid-assertion by a completely
    /// unrelated, concurrently-running test's own `AdeApp` construction resetting the same global
    /// back to `0` - a real, reproduced flake specific to default (not `--test-threads=1`)
    /// parallelism, the exact mode `BUILD-LOG.md`'s own established testing convention (see its
    /// `pty-core` step) expects to run clean. A `thread_local!` fixes this for free: each test
    /// thread gets its own independent copy, so tests can never interfere with each other's theme
    /// selection regardless of scheduling, while production is unaffected (there is still only
    /// the one real foreground thread that ever reads or writes this).
    static CURRENT_THEME_INDEX: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// See [`CURRENT_THEME_INDEX`]'s own docs.
pub fn current_theme_index() -> usize {
    CURRENT_THEME_INDEX.with(|index| index.get())
}

/// Sets the live-selected theme index - the one real place `crate::root::AdeApp`'s theme
/// selection (`crate::settings::THEME_DEFS`'s row click, or a real `follow_system` OS-appearance
/// change) writes through. Callers must also force a real repaint (`App::refresh_windows`,
/// `vendor/zed/crates/gpui/src/app.rs:1025`) afterward - this function only flips the thread-local
/// index; nothing here can reach into GPUI's own render loop to schedule one.
pub fn set_current_theme_index(index: usize) {
    CURRENT_THEME_INDEX.with(|cell| cell.set(index));
}

/// A real, systematic HSL transform - see [`derive_shift`]'s own docs for how one is computed,
/// and [`apply_shift`] for how it's applied to a single token.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HslShift {
    /// Added to hue (wraps via `rem_euclid`), in the same 0.0..=1.0 range `gpui::Hsla::h` uses.
    hue: f32,
    /// Multiplies saturation.
    saturation_scale: f32,
    /// `new_lightness = old_lightness * lightness_scale + lightness_offset` - a linear remap,
    /// not a plain additive shift, so a light theme (`Paper`) can be derived from Jerry Dark's
    /// own near-black baseline without every already-light token clipping at 100%. Clamped to
    /// `0.0..=1.0` in [`apply_shift`].
    lightness_scale: f32,
    lightness_offset: f32,
}

const IDENTITY_SHIFT: HslShift = HslShift {
    hue: 0.0,
    saturation_scale: 1.0,
    lightness_scale: 1.0,
    lightness_offset: 0.0,
};

/// Applies `shift` to `base` via a real `Rgba -> Hsla -> (shift) -> Rgba` round trip, using
/// GPUI's own real, verified conversions (`vendor/zed/crates/gpui/src/color.rs`'s `From<Rgba>
/// for Hsla`/`From<Hsla> for Rgba`) rather than a hand-rolled one.
///
/// Known, honestly-disclosed limitation (an audit found this): for `"Paper"`, the one real light
/// theme, `derive_shift`'s linear lightness fit solves a *negative* `lightness_scale` (Jerry
/// Dark's near-black window background maps to Paper's near-white one) - correct for the two
/// background swatches the fit is solved from, but it also means several of Jerry Dark's already
/// fairly-light `text::*` tokens (`SELECTED`/`PRIMARY`/`HEADING`) get mapped *below* `0.0`
/// lightness before the `.clamp(0.0, 1.0)` above brings them back to pure black - collapsing what
/// were three distinguishable text levels in Jerry Dark into one on Paper. Still a real, applied
/// derived palette (not a fake one - every other token, and every other theme, keeps real
/// relative distinctness), just a known rough edge in exactly this one theme's lightest text
/// tokens; a properly rank-preserving remap (e.g. a quantile-based fit instead of a single linear
/// one) would fix it but is a larger, separate piece of real design work.
fn apply_shift(base: Rgba, shift: HslShift) -> Rgba {
    let mut hsla: Hsla = base.into();
    hsla.h = (hsla.h + shift.hue).rem_euclid(1.0);
    hsla.s = (hsla.s * shift.saturation_scale).clamp(0.0, 1.0);
    hsla.l = (hsla.l * shift.lightness_scale + shift.lightness_offset).clamp(0.0, 1.0);
    hsla.into()
}

/// Derives a real, systematic [`HslShift`] for a non-Jerry-Dark theme from the two themes' own
/// five `crate::settings::THEME_DEFS` swatches (`[background, panel, green-ish, amber-ish,
/// blue-ish]`, `crate::settings::ThemeDef::swatches`'s own real, transcribed values) - not a
/// hand-picked palette per theme. This is the whole mechanism the module docs' "derived, not
/// hand-authored" claim rests on:
///
/// - **Lightness** is a linear fit (`scale`/`offset`, not a plain additive shift) solved exactly
///   from the two background-ish swatches (index 0, the window background; index 1, the panel
///   background) - two points exactly determine a line. This is what lets a light theme (real
///   example: `Paper`, whose swatches are genuinely light hex values) be derived correctly from
///   Jerry Dark's own near-black tokens without every already-fairly-light token clipping at
///   100%: a plain `lightness + delta` shift would either undershoot on Jerry Dark's darkest
///   tokens or blow straight through 1.0 on its lighter ones: dividing this same linear fit by
///   *background* lightness specifically (not averaged across all five swatches, which would
///   blend in the differently-behaved chromatic ones below) keeps the fit meaningful for a
///   theme that inverts light/dark entirely.
/// - **Hue** is the circular mean (via `atan2` over the swatches' real `(cos, sin)` hue vectors,
///   not a plain arithmetic average, which breaks across the 0.0/1.0 wraparound a hue near red
///   sits on) of the *chromatic* swatches only (index 2/3/4 - green/amber/blue-ish accents) -
///   the two background swatches are excluded since a near-desaturated colour's hue is numerically
///   unstable (dividing by a `delta` close to zero in `From<Rgba> for Hsla`) and not
///   representative of what a real accent-colour hue shift should be.
/// - **Saturation** is the mean ratio (`target.s / base.s`) across the same three chromatic
///   swatches, clamped so a theme with an unusually low-saturation swatch pair can't produce a
///   negative or wildly inflated scale.
///
/// Every one of Jerry Dark's ~200 real tokens then goes through the exact same [`apply_shift`],
/// deriving a *whole plausible palette* rather than only re-tinting the five swatches themselves
/// - the real, load-bearing distinction from "only the preview cards look different."
fn derive_shift(base_swatches: [u32; 5], target_swatches: [u32; 5]) -> HslShift {
    fn hsla_of(hex_value: u32) -> Hsla {
        Rgba {
            r: ((hex_value >> 16) & 0xff) as f32 / 255.0,
            g: ((hex_value >> 8) & 0xff) as f32 / 255.0,
            b: (hex_value & 0xff) as f32 / 255.0,
            a: 1.0,
        }
        .into()
    }

    let base: Vec<Hsla> = base_swatches.into_iter().map(hsla_of).collect();
    let target: Vec<Hsla> = target_swatches.into_iter().map(hsla_of).collect();

    // Lightness: an exact linear fit through the two background-ish swatches (index 0, 1).
    let (base_bg, base_panel) = (base[0].l, base[1].l);
    let (target_bg, target_panel) = (target[0].l, target[1].l);
    let denominator = base_panel - base_bg;
    let lightness_scale = if denominator.abs() > 0.001 {
        (target_panel - target_bg) / denominator
    } else {
        1.0
    };
    let lightness_offset = target_bg - lightness_scale * base_bg;

    // Hue: circular mean of the three chromatic swatches (index 2, 3, 4) only.
    let (mut sin_sum, mut cos_sum) = (0.0f32, 0.0f32);
    for index in 2..5 {
        let delta = (target[index].h - base[index].h) * std::f32::consts::TAU;
        sin_sum += delta.sin();
        cos_sum += delta.cos();
    }
    let hue = (sin_sum.atan2(cos_sum) / std::f32::consts::TAU).rem_euclid(1.0);

    // Saturation: mean ratio across the same three chromatic swatches.
    let mut ratio_sum = 0.0f32;
    let mut ratio_count = 0.0f32;
    for index in 2..5 {
        if base[index].s > 0.001 {
            ratio_sum += target[index].s / base[index].s;
            ratio_count += 1.0;
        }
    }
    let saturation_scale = if ratio_count > 0.0 {
        (ratio_sum / ratio_count).clamp(0.0, 3.0)
    } else {
        1.0
    };

    HslShift {
        hue,
        saturation_scale,
        lightness_scale,
        lightness_offset,
    }
}

/// The real, once-computed shift table for `crate::settings::THEME_DEFS`' six themes - index 0
/// (Jerry Dark) is always [`IDENTITY_SHIFT`] ([`ColorToken::resolve`] special-cases it anyway,
/// never actually calling through here for it, but a real identity entry keeps this table
/// honestly total over every real theme index rather than silently relying on that short
/// circuit). Computed once, lazily, from the live `THEME_DEFS` table itself (not a second,
/// hand-copied set of swatch literals that could drift from it).
fn theme_shift(index: usize) -> HslShift {
    static SHIFTS: std::sync::OnceLock<[HslShift; 6]> = std::sync::OnceLock::new();
    let shifts = SHIFTS.get_or_init(|| {
        let defs = crate::settings::THEME_DEFS;
        let base = defs[0].swatches;
        std::array::from_fn(|i| {
            if i == 0 {
                IDENTITY_SHIFT
            } else {
                derive_shift(base, defs[i].swatches)
            }
        })
    });
    shifts.get(index).copied().unwrap_or(IDENTITY_SHIFT)
}

pub mod surface {
    use super::{hex, ColorToken};

    pub const WINDOW: ColorToken = hex(0x0e0f11); // window body
    pub const WINDOW_BORDER: ColorToken = hex(0x262a2e);
    pub const TITLE_BAR: ColorToken = hex(0x101214);
    pub const RAIL: ColorToken = hex(0x101113); // left rail + right panel
    pub const CENTER: ColorToken = hex(0x131518); // work surface
    pub const PTY: ColorToken = hex(0x0d0f11); // agent CLI + terminal
    pub const HEADER: ColorToken = hex(0x121417); // context bar, panel headers
    pub const FOOTER: ColorToken = hex(0x111316); // surface footers, status strips
    pub const CARD: ColorToken = hex(0x161a1d); // composer, settings cards
    pub const CARD_SUNK: ColorToken = hex(0x131619); // card footers
    pub const POPOVER: ColorToken = hex(0x181c20); // completion popup, hover card
    pub const PALETTE: ColorToken = hex(0x15181b);
    pub const SCRIM: ColorToken = hex(0x060708); // at 62% alpha behind the palette
    pub const ROW_HOVER: ColorToken = hex(0x15181b);
    pub const ROW_HOVER_ALT: ColorToken = hex(0x1b1f22); // hover on chrome buttons
    pub const ROW_SELECTED: ColorToken = hex(0x1a1e21);
    pub const SEGMENT_TRACK: ColorToken = hex(0x171a1d);
    pub const SEGMENT_ACTIVE: ColorToken = hex(0x242a2f);
    pub const KEYCAP: ColorToken = hex(0x181c1f);
    /// The hint-size keycap's own background - distinct from [`KEYCAP`]'s standard-size
    /// `#181c1f` (`Jerry.dc.html`: `background:#15181a;border:1px solid #23272b`).
    pub const KEYCAP_HINT: ColorToken = hex(0x15181a);
    pub const CHIP_NEUTRAL: ColorToken = hex(0x23272b);
    pub const CURRENT_LINE: ColorToken = hex(0x181c20);
    /// The Windows/Linux title bar's close caption button's hover fill (`Jerry.dc.html`:
    /// `style-hover="background:#8c3a38"`).
    pub const TITLE_BAR_CLOSE_HOVER: ColorToken = hex(0x8c3a38);
    /// The tab strip's `+` menu popover row hover fill - distinct from [`ROW_HOVER`]/
    /// [`ROW_HOVER_ALT`].
    pub const PLUS_MENU_ROW_HOVER: ColorToken = hex(0x1d2226);
    /// A file tab's close-affordance hover fill - one hex step off [`CHIP_NEUTRAL`]
    /// (`#23272b`), kept as its own token.
    pub const TAB_CLOSE_HOVER: ColorToken = hex(0x23282c);
}

pub mod border {
    use super::{hex, ColorToken};

    pub const ZONE: ColorToken = hex(0x1e2225); // between the three zones
    pub const INNER: ColorToken = hex(0x1c2023); // between bands inside a zone
    pub const RAIL_INNER: ColorToken = hex(0x191c1f);
    pub const ROW: ColorToken = hex(0x171a1c); // change-list row separators
    pub const DIVIDER: ColorToken = hex(0x22262a); // 1px vertical rules
    pub const CARD: ColorToken = hex(0x23282c);
    pub const CARD_FIELD: ColorToken = hex(0x22272b);
    pub const COMPOSER: ColorToken = hex(0x24292e);
    pub const POPOVER: ColorToken = hex(0x2b3238);
    pub const BUTTON: ColorToken = hex(0x2a2f34); // outline button
    pub const BUTTON_DISABLED: ColorToken = hex(0x1f2327);
    pub const KEYCAP: ColorToken = hex(0x272c31);
    /// The hint-size keycap's own border - see [`super::surface::KEYCAP_HINT`].
    pub const KEYCAP_HINT: ColorToken = hex(0x23272b);
    pub const SELECTED_EDGE: ColorToken = hex(0x3f5b74); // 2px left edge on a selected row
}

pub mod text {
    use super::{hex, ColorToken};

    pub const SELECTED: ColorToken = hex(0xdde2e7);
    pub const PRIMARY: ColorToken = hex(0xd3d8dd);
    pub const HEADING: ColorToken = hex(0xc8cdd2);
    pub const STRONG: ColorToken = hex(0xc2c7cc);
    pub const BODY: ColorToken = hex(0xb8bfc6);
    pub const SECONDARY: ColorToken = hex(0xa9b0b7);
    pub const MUTED: ColorToken = hex(0x9aa1a8);
    pub const DIM: ColorToken = hex(0x8b9197);
    pub const DIMMER: ColorToken = hex(0x7d848b);
    pub const FAINT: ColorToken = hex(0x6b7178);
    pub const FAINTER: ColorToken = hex(0x5e646a);
    pub const GHOST: ColorToken = hex(0x4e545a);
    pub const GHOSTER: ColorToken = hex(0x454b51);
    pub const HINT: ColorToken = hex(0x41464b);
    pub const GUTTER: ColorToken = hex(0x3a3f44);
    pub const DISABLED: ColorToken = hex(0x3d4248);
    /// The context bar's worktree path text (`README.md`: "worktree path 10.5px mono
    /// `#4a5057`") - one hex step off [`GHOST`]; not in `tokens.rs`'s `text` module, added
    /// here directly.
    pub const PATH: ColorToken = hex(0x4a5057);
    /// The file tree row's `▾`/`▸` caret - same hex as [`PATH`] but a distinct token for a
    /// distinct element.
    pub const TREE_CARET: ColorToken = hex(0x4a5057);
}

/// Status is the only place colour carries meaning in the rail.
pub mod status {
    use super::{hex, ColorToken};

    pub const ASK: ColorToken = hex(0xe2a336); // needs input
    pub const ASK_BG: ColorToken = hex(0x3a2c14);
    pub const FAIL: ColorToken = hex(0xe0625c);
    pub const FAIL_BG: ColorToken = hex(0x3a1e1e);
    pub const REVIEW: ColorToken = hex(0x5cb87f);
    pub const REVIEW_BG: ColorToken = hex(0x1e3b2a);
    pub const RUN: ColorToken = hex(0x5a9ad4);
    pub const RUN_BG: ColorToken = hex(0x1e2f3e);
    pub const IDLE: ColorToken = hex(0x565d64);
    pub const IDLE_BG: ColorToken = hex(0x22262a);
    // waiting-question preview inside a rail row
    pub const ASK_CARD_BG: ColorToken = hex(0x1c1710);
    pub const ASK_CARD_EDGE: ColorToken = hex(0x8a6420);
    pub const ASK_CARD_FG: ColorToken = hex(0xc99b4e);
    // conflict banner
    pub const BANNER_BG: ColorToken = hex(0x1b1610);
    pub const BANNER_BORDER: ColorToken = hex(0x33291a);
}

pub mod diff {
    use super::{hex, ColorToken};

    pub const ADD_BG: ColorToken = hex(0x12211a);
    pub const ADD_FG: ColorToken = hex(0x9fd0b2);
    pub const ADD_SIGN: ColorToken = hex(0x4e8c68);
    pub const DEL_BG: ColorToken = hex(0x211517);
    pub const DEL_FG: ColorToken = hex(0xd6a4a0);
    pub const DEL_SIGN: ColorToken = hex(0xa35f5b);
    pub const CTX_FG: ColorToken = hex(0x868d94);
    pub const HUNK_BG: ColorToken = hex(0x15181c);
    pub const HUNK_FG: ColorToken = hex(0x5f666e);
    pub const FOLD_BG: ColorToken = hex(0x121417);
    pub const FOLD_FG: ColorToken = hex(0x4a5057);
    pub const STAT_ADD: ColorToken = hex(0x5f9c78); // "+142" label
    pub const STAT_DEL: ColorToken = hex(0xb06a66); // "-8" label
    pub const STAT_EMPTY: ColorToken = hex(0x22262a); // unused segment of the 5-bar
    pub const GIT_GUTTER: ColorToken = hex(0x2c6244); // 3px agent-touched marker
}

pub mod syntax {
    use super::{hex, ColorToken};

    pub const TEXT: ColorToken = hex(0xacb2be);
    pub const KEYWORD: ColorToken = hex(0xb477cf);
    pub const FUNCTION: ColorToken = hex(0x74ade8);
    pub const TYPE: ColorToken = hex(0xdfc184);
    pub const LITERAL: ColorToken = hex(0xbf956a); // numbers, `self`
    pub const COMMENT: ColorToken = hex(0x5d636f);
    pub const CARET: ColorToken = hex(0x5a9ad4);
    pub const ERROR_UNDERLINE: ColorToken = hex(0xe0625c); // 2px dotted
    pub const HOVER_UNDERLINE: ColorToken = hex(0x4d7ba8); // 1px solid

    /// The File view's Diagnostic-state row tint (`README.md`: "row tinted `#191416`") -
    /// distinct from [`super::surface::CURRENT_LINE`].
    pub const DIAGNOSTIC_ROW_BG: ColorToken = hex(0x191416);
    /// The Diagnostic state's dim, end-of-line inline message text (`README.md`: `#6b4a48`).
    pub const DIAGNOSTIC_INLINE_MESSAGE: ColorToken = hex(0x6b4a48);
    /// The Diagnostic state's card message text (`README.md`: `#e3908b`). Same hex as
    /// [`super::button::DANGER_FG_HOVER`], kept as its own token - unrelated elements that
    /// happen to share a designed red.
    pub const DIAGNOSTIC_CARD_MESSAGE: ColorToken = hex(0xe3908b);
}

pub mod term {
    use super::{hex, ColorToken};

    pub const PROMPT: ColorToken = hex(0x8fbde6);
    pub const TEXT: ColorToken = hex(0xa7adb4);
    pub const DIM: ColorToken = hex(0x6b7178);
    pub const OK: ColorToken = hex(0x6ab97f);
    pub const ERR: ColorToken = hex(0xe0625c);
    pub const WARN: ColorToken = hex(0xd8a94a);
    pub const HEADING: ColorToken = hex(0xced4da);
    pub const ACTIVITY: ColorToken = hex(0x5a9ad4); // spinner / progress line
    pub const MENU_SEL_FG: ColorToken = hex(0xe0b263);
    pub const MENU_SEL_BG: ColorToken = hex(0x1f1a10);
    pub const CURSOR: ColorToken = hex(0x5a9ad4);
    /// A clickable path/`path:line` link inside terminal output (`Jerry.dc.html`:
    /// `color:#7fb4e3;border-bottom:1px dotted #3d6a91`).
    pub const LINK: ColorToken = hex(0x7fb4e3);
    pub const LINK_UNDERLINE: ColorToken = hex(0x3d6a91);
    /// The link's hover state (`Jerry.dc.html`: `style-hover="color:#a5cdf0;border-bottom:1px
    /// solid #78a8d0"`). Same value as [`super::button::BLUE_FG`], kept as its own token for a
    /// distinct element.
    pub const LINK_HOVER: ColorToken = hex(0xa5cdf0);
    pub const LINK_UNDERLINE_HOVER: ColorToken = hex(0x78a8d0);
}

/// The environment (WSL) chip's tokens - shown in the terminal footer, the status bar, and
/// Settings' `Default environment` row.
pub mod env {
    use super::{hex, ColorToken};

    /// Same value as [`super::term::PROMPT`], reused directly (`Jerry.dc.html`'s
    /// `footRemoteFg` for `plat === 'windows'`).
    pub const WSL_FG: ColorToken = super::term::PROMPT;
    pub const WSL_BG: ColorToken = hex(0x16222c);
    pub const WSL_BORDER: ColorToken = hex(0x24384a);
    /// Same value as [`super::text::FAINT`], reused directly.
    pub const LOCAL_FG: ColorToken = super::text::FAINT;
    /// Same value as [`super::border::DIVIDER`], reused directly.
    pub const LOCAL_BORDER: ColorToken = super::border::DIVIDER;
}

/// One tint per agent. Used on the rail badge, the CLI tab chip and the
/// conflict side headers, so a colour always means the same agent.
pub mod agent {
    use super::{hex, ColorToken};

    pub const SONNET: (ColorToken, ColorToken) = (hex(0xd8a94a), hex(0x33280f)); // (fg, bg)
    pub const CODEX: (ColorToken, ColorToken) = (hex(0x6ab97f), hex(0x1e3327));
    pub const HAIKU: (ColorToken, ColorToken) = (hex(0xc98fbf), hex(0x332030));
    pub const LOCAL: (ColorToken, ColorToken) = (hex(0x7f9ad4), hex(0x1f2941));
}

/// Language chips, shared by the file tree, the code tab and the palette.
pub mod lang {
    use super::{hex, ColorToken};

    pub const RS: (ColorToken, ColorToken) = (hex(0xc0824a), hex(0x2e2113)); // "rs"
    pub const TOML: (ColorToken, ColorToken) = (hex(0x8b9197), hex(0x23272b)); // "to"
    pub const MD: (ColorToken, ColorToken) = (hex(0x7f9ad4), hex(0x1d2532)); // "md"
    pub const SQL: (ColorToken, ColorToken) = (hex(0x6ab97f), hex(0x1b2a20)); // "sq"
                                                                              // Verified directly against `design_handoff_jerry_ade/revision/tokens.rs:149-160`'s real
                                                                              // hex values, not paraphrased.
    pub const TS: (ColorToken, ColorToken) = (hex(0x6b9bd1), hex(0x1b2838)); // "ts"
    pub const VUE: (ColorToken, ColorToken) = (hex(0x5cb87f), hex(0x16261e)); // "vue"
    pub const PY: (ColorToken, ColorToken) = (hex(0xc9b04a), hex(0x2a2612)); // "py"
    pub const GO: (ColorToken, ColorToken) = (hex(0x5fa8c4), hex(0x152730)); // "go"
    pub const UNKNOWN: (ColorToken, ColorToken) = (hex(0x6b7178), hex(0x23272b));
    // "."
}

pub mod button {
    use super::{hex, ColorToken};

    pub const GREEN_BG: ColorToken = hex(0x24503a);
    pub const GREEN_BG_HOVER: ColorToken = hex(0x2c6045);
    pub const GREEN_FG: ColorToken = hex(0x9fdcb6);
    pub const GREEN_KEYCAP: ColorToken = hex(0x376b4d);
    /// The keycap glyph colour inside a green primary button (`README.md`/`Jerry.dc.html`:
    /// `#8ac9a4`) - not in `tokens.rs`'s `button` module (only [`GREEN_KEYCAP`], the border, is
    /// transcribed there), added here directly.
    pub const GREEN_KEYCAP_FG: ColorToken = hex(0x8ac9a4);
    // The equivalent blue keycap glyph colour (`#8fbde6`) needs no separate constant here -
    // it's the exact same value already ported as `term::PROMPT`.
    pub const BLUE_BG: ColorToken = hex(0x243c50);
    pub const BLUE_BG_HOVER: ColorToken = hex(0x2c4a63);
    pub const BLUE_FG: ColorToken = hex(0xa5cdf0);
    pub const BLUE_KEYCAP: ColorToken = hex(0x365b78);
    pub const AMBER_BG: ColorToken = hex(0x3a2c14);
    pub const AMBER_BG_HOVER: ColorToken = hex(0x4a3818);
    pub const AMBER_FG: ColorToken = hex(0xe0b263);
    pub const DANGER_FG: ColorToken = hex(0xc4726d);
    pub const DANGER_FG_HOVER: ColorToken = hex(0xe3908b);
}

pub mod toggle {
    use super::{hex, ColorToken};

    pub const TRACK_ON: ColorToken = hex(0x2f6d4b);
    pub const TRACK_OFF: ColorToken = hex(0x23272b);
    pub const KNOB_ON: ColorToken = hex(0xc8ecd6);
    pub const KNOB_OFF: ColorToken = hex(0x6b7178);
}

pub mod tag {
    use super::{hex, ColorToken};

    pub const NEW: (ColorToken, ColorToken) = (hex(0x7fc79a), hex(0x1e3b2a));
    pub const DELETED: (ColorToken, ColorToken) = (hex(0xd18b86), hex(0x3a1e1e));
    pub const CONFLICT: (ColorToken, ColorToken) = (hex(0xe0b263), hex(0x3a2c14));
    pub const TREE_ADDED: ColorToken = hex(0x5f9c78); // "A" mark
    pub const TREE_MODIFIED: ColorToken = hex(0xa3873f); // "M" mark
}

/// Settings-surface-only colours read directly from `Jerry.dc.html`'s inline literals for the
/// `settingsOpen` block - real values present in the mockup but missing from `tokens.rs`'s
/// transcription (predates the Settings section). Every other Settings colour reuses an
/// existing token from another module - see `crate::root`'s Settings render methods.
pub mod settings {
    use super::{hex, ColorToken};

    /// A nav row's hover background (`Jerry.dc.html`: `style-hover="background:#17191b"`) -
    /// distinct from [`super::surface::ROW_HOVER`] (`#15181b`).
    pub const NAV_ROW_HOVER: ColorToken = hex(0x17191b);
    /// The content column's page-subtitle text (`Jerry.dc.html`: `color:#767d84`) - close to
    /// but distinct from [`super::text::DIM`] (`#8b9197`).
    pub const SUBTITLE: ColorToken = hex(0x767d84);
    /// A card row's own bottom separator (`Jerry.dc.html`: `border-bottom:1px solid #1f2327`) -
    /// distinct from [`super::border::CARD_FIELD`] (`#22272b`).
    pub const CARD_ROW_SEP: ColorToken = hex(0x1f2327);
    /// A binary-found status dot on the Agents page. Same hex as [`super::status::REVIEW`],
    /// kept as its own token: the session `Status` palette is reserved for session urgency
    /// (`README.md`'s "Status vocabulary — use nowhere else"), and "this binary resolved on
    /// `$PATH`" is a different fact that just happens to want the same green.
    pub const AGENT_READY: ColorToken = hex(0x5cb87f);
    /// A binary-not-found status dot on the Agents page - same reasoning as [`AGENT_READY`],
    /// same hex as [`super::status::FAIL`].
    pub const AGENT_NOT_FOUND: ColorToken = hex(0xe0625c);
    /// The Worktrees page's "merged and prunable" row dot - distinct from
    /// [`super::status::IDLE`] (`#565d64`, used for the main checkout's own dot).
    pub const WORKTREE_PRUNABLE_DOT: ColorToken = hex(0x3f454b);
    /// A selected Appearance-preview-card's / Theme-card's background - see
    /// [`CARD_UNSELECTED_BG`] for the unselected counterpart.
    pub const CARD_SELECTED_BG: ColorToken = hex(0x161b1f);
    pub const CARD_UNSELECTED_BG: ColorToken = hex(0x131619);
    /// A Theme card's hover border (`Jerry.dc.html`: `style-hover="border-color:#3a4148"`).
    pub const THEME_CARD_HOVER_BORDER: ColorToken = hex(0x3a4148);
    /// The config snippet block's section-header line colour (`Jerry.dc.html`'s `CSFG.s`:
    /// `#c294e0`).
    pub const SNIPPET_SECTION: ColorToken = hex(0xc294e0);
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
    use super::{hex, ColorToken};

    /// The input row's scope-prefix glyph (`Jerry.dc.html`: `color:#5f7f9e`).
    pub const PREFIX: ColorToken = hex(0x5f7f9e);
    /// A result group's uppercase header label (`Jerry.dc.html`: `color:#5b6167`) - close to
    /// but distinct from [`super::text::FAINT`] (`#6b7178`).
    pub const GROUP_HEADER: ColorToken = hex(0x5b6167);
    /// An unselected result row's hover background (`Jerry.dc.html`: `style-hover`:
    /// `background:#191d20`) - distinct from [`super::surface::ROW_HOVER`] (`#15181b`, which
    /// happens to equal the palette panel's own background, [`super::surface::PALETTE`]).
    pub const ROW_HOVER: ColorToken = hex(0x191d20);
    /// The selected/first row's label colour (`Jerry.dc.html`: `fg: first ? '#e3e8ed' :
    /// '#c2c7cc'`) - one hex step brighter than [`super::text::SELECTED`] (`#dde2e7`).
    pub const LABEL_SELECTED: ColorToken = hex(0xe3e8ed);
    /// A command result's kind chip `(fg, bg)` (`Jerry.dc.html`: `background:#1d2532` /
    /// `color:#7f9ad4`) - the same hex pair as [`super::lang::MD`], kept as its own token since
    /// a command chip and a Markdown-file chip are unrelated concepts.
    pub const COMMAND_CHIP: (ColorToken, ColorToken) = (hex(0x7f9ad4), hex(0x1d2532));
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
    // `a` is real, live-registered `ColorToken`s (e.g. `lang::TS`); `b` is a literal hex
    // reconstruction. Resolved via `ColorToken::resolve` - a no-op at the real default index 0
    // every test in this binary starts at (see `theme_runtime_tests`'s own docs on why that
    // global must always be restored), so this stays an exact-hex comparison, not a lossy one.
    fn same_pair(a: (super::ColorToken, super::ColorToken), b: (Rgba, Rgba)) -> bool {
        same(a.0.resolve(), b.0) && same(a.1.resolve(), b.1)
    }

    fn same_color_token_pair(
        a: (super::ColorToken, super::ColorToken),
        b: (super::ColorToken, super::ColorToken),
    ) -> bool {
        same(a.0.resolve(), b.0.resolve()) && same(a.1.resolve(), b.1.resolve())
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
                    !same_color_token_pair(*color_a, *color_b),
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

/// Real regression coverage for the runtime theme-swap mechanism itself -
/// [`CURRENT_THEME_INDEX`] is real, process-global, mutable state, so every test here uses
/// [`with_theme_index`] to both set it up and (via `Drop`, so it still runs if the test body
/// panics) restore it to `0` afterward - a test in this module leaking a non-default index would
/// silently corrupt every *other* test in this binary that reads a colour token, including ones
/// in completely unrelated modules, since `cargo test`'s default single-process-many-threads
/// model shares this one real global across the whole run.
#[cfg(test)]
mod theme_runtime_tests {
    use super::*;

    struct ResetThemeIndexOnDrop;

    impl Drop for ResetThemeIndexOnDrop {
        fn drop(&mut self) {
            set_current_theme_index(0);
        }
    }

    fn with_theme_index(index: usize) -> ResetThemeIndexOnDrop {
        set_current_theme_index(index);
        ResetThemeIndexOnDrop
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    #[test]
    fn current_theme_index_defaults_to_jerry_dark_and_round_trips_through_a_real_set() {
        assert_eq!(
            current_theme_index(),
            0,
            "the real process default before any test touches it"
        );
        let _guard = with_theme_index(3);
        assert_eq!(current_theme_index(), 3);
    }

    /// The real identity case: at index `0`, `resolve()` must return the token's own original
    /// value completely unchanged - not even a lossy `Rgba -> Hsla -> Rgba` round trip (see
    /// [`ColorToken::resolve`]'s own docs for why this matters for every other exact-hex test in
    /// this crate).
    #[test]
    fn jerry_dark_resolve_is_bit_exact_with_no_hsl_round_trip() {
        let _guard = with_theme_index(0);
        let original = surface::WINDOW.0;
        assert!(same(surface::WINDOW.resolve(), original));
    }

    /// The real, load-bearing proof a theme swap actually changes what gets rendered - not just
    /// that the setting persisted. `surface::WINDOW` stands in for "a representative render
    /// call": every real surface in this app reads this exact token for its window background
    /// (`crate::root::AdeApp`'s own top-level `.bg(theme::surface::WINDOW)`).
    #[test]
    fn a_real_theme_swap_changes_what_a_representative_token_resolves_to() {
        let jerry_dark = surface::WINDOW.resolve();
        let _guard = with_theme_index(2); // "Slate"
        let slate = surface::WINDOW.resolve();
        assert!(
            !same(jerry_dark, slate),
            "selecting a different real theme must actually change the resolved colour, not \
             just the persisted setting"
        );
    }

    /// Real, systematic proof this is a *palette* swap, not a two-or-three-token cosmetic
    /// re-tint: several tokens from genuinely different original `theme` modules (surface,
    /// text, syntax, status, diff) must all differ under a non-Jerry-Dark theme - the exact
    /// "half-faked" failure mode (only the settings-page preview cards changing while the rest
    /// of the app stayed on Jerry Dark) this mechanism exists to avoid.
    #[test]
    fn every_non_jerry_dark_theme_changes_tokens_across_multiple_unrelated_modules() {
        let jerry_dark = (
            surface::WINDOW.0,
            text::BODY.0,
            syntax::KEYWORD.0,
            status::ASK.0,
            diff::ADD_BG.0,
        );
        for index in 1..6 {
            let _guard = with_theme_index(index);
            let resolved = (
                surface::WINDOW.resolve(),
                text::BODY.resolve(),
                syntax::KEYWORD.resolve(),
                status::ASK.resolve(),
                diff::ADD_BG.resolve(),
            );
            let mut changed = 0;
            if !same(resolved.0, jerry_dark.0) {
                changed += 1;
            }
            if !same(resolved.1, jerry_dark.1) {
                changed += 1;
            }
            if !same(resolved.2, jerry_dark.2) {
                changed += 1;
            }
            if !same(resolved.3, jerry_dark.3) {
                changed += 1;
            }
            if !same(resolved.4, jerry_dark.4) {
                changed += 1;
            }
            assert!(
                changed >= 4,
                "theme index {index} ({}) only changed {changed}/5 real tokens spanning \
                 surface/text/syntax/status/diff - a genuine full-palette derivation should move \
                 nearly all of them, not leave most still reading Jerry Dark's own values",
                crate::settings::THEME_DEFS[index].name
            );
        }
    }

    /// "Paper" (`crate::settings::THEME_DEFS[5]`) is the one real light theme - its own swatches
    /// are genuinely light hex values, so the derived lightness fit
    /// ([`derive_shift`]'s own docs) must actually produce a *lighter* window background than
    /// Jerry Dark's near-black original, not just a differently-hued dark colour.
    #[test]
    fn paper_theme_derives_a_genuinely_lighter_window_background_than_jerry_dark() {
        let jerry_dark_lightness: Hsla = surface::WINDOW.0.into();
        let _guard = with_theme_index(5); // "Paper"
        let paper_lightness: Hsla = surface::WINDOW.resolve().into();
        assert!(
            paper_lightness.l > jerry_dark_lightness.l + 0.3,
            "Paper's derived window background (lightness {}) should be substantially lighter \
             than Jerry Dark's own (lightness {})",
            paper_lightness.l,
            jerry_dark_lightness.l
        );
    }

    /// Switching back to Jerry Dark after visiting another theme must restore the exact
    /// original value, not some residue of the shift that was applied - the real round-trip
    /// safety a global mutable index needs.
    #[test]
    fn switching_back_to_jerry_dark_restores_the_exact_original_value() {
        let original = surface::WINDOW.0;
        {
            let _guard = with_theme_index(4);
            assert!(!same(surface::WINDOW.resolve(), original));
        }
        // The guard above already reset to 0 on drop - confirm the real effect.
        assert_eq!(current_theme_index(), 0);
        assert!(same(surface::WINDOW.resolve(), original));
    }

    /// `derive_shift`'s lightness fit is solved from the two background-ish swatches specifically
    /// (index 0 and 1) - a real, direct unit test of the pure function itself, independent of
    /// the full token-resolution machinery above.
    #[test]
    fn derive_shift_solves_an_exact_linear_fit_through_the_two_background_swatches() {
        // A synthetic "base" theme (window bg lightness ~10%, panel ~20%) and "target" theme
        // (window bg ~50%, panel ~70%) - the fit should map base 0.10 -> target 0.50 and base
        // 0.20 -> target 0.70 exactly.
        let base = [0x1a1a1a, 0x333333, 0x808080, 0x808080, 0x808080];
        let target = [0x808080, 0xb3b3b3, 0x808080, 0x808080, 0x808080];
        let shift = derive_shift(base, target);

        let remap = |hex_value: u32| -> f32 {
            let hsla: Hsla = Rgba {
                r: ((hex_value >> 16) & 0xff) as f32 / 255.0,
                g: ((hex_value >> 8) & 0xff) as f32 / 255.0,
                b: (hex_value & 0xff) as f32 / 255.0,
                a: 1.0,
            }
            .into();
            (hsla.l * shift.lightness_scale + shift.lightness_offset).clamp(0.0, 1.0)
        };

        let target_bg_l: Hsla = Rgba {
            r: 0x80 as f32 / 255.0,
            g: 0x80 as f32 / 255.0,
            b: 0x80 as f32 / 255.0,
            a: 1.0,
        }
        .into();
        let target_panel_l: Hsla = Rgba {
            r: 0xb3 as f32 / 255.0,
            g: 0xb3 as f32 / 255.0,
            b: 0xb3 as f32 / 255.0,
            a: 1.0,
        }
        .into();

        assert!((remap(0x1a1a1a) - target_bg_l.l).abs() < 0.01);
        assert!((remap(0x333333) - target_panel_l.l).abs() < 0.01);
    }

    /// A degenerate `base` (identical window/panel lightness - a real divide-by-near-zero case
    /// in [`derive_shift`]'s lightness fit) must fall back to an identity scale rather than
    /// producing `NaN`/`inf` and corrupting every token's lightness.
    #[test]
    fn derive_shift_never_produces_nan_when_the_base_swatches_have_equal_lightness() {
        let base = [0x404040, 0x404040, 0x808080, 0x808080, 0x808080];
        let target = [0x202020, 0x606060, 0x101010, 0x505050, 0x909090];
        let shift = derive_shift(base, target);
        assert!(shift.lightness_scale.is_finite());
        assert!(shift.lightness_offset.is_finite());
        assert!(shift.hue.is_finite());
        assert!(shift.saturation_scale.is_finite());
    }
}
