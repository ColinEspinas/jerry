//! Jerry's design tokens, ported from `design_handoff_jerry_ade/tokens.rs` (colour/size
//! constants transcribed from the reviewed mockup `Jerry.dc.html`).
//!
//! ## Runtime-swappable colour tokens ([`ColorToken`])
//!
//! Every colour constant below (`surface::WINDOW`, `text::BODY`, `syntax::KEYWORD`, ...) is a
//! [`ColorToken`], not a plain [`Rgba`] - a real, compile-time `const` (so it can still appear
//! inside another `const`, e.g. `crate::language::EXTENSIONS`'s `chip_colors` field - see that
//! module's own docs) carrying two things: a real, stable **key** (`"surface.window"`,
//! `"syntax.keyword"`, ...) and Jerry Dark's own literal [`Rgba`] **default**. At the point
//! something actually asks for a real colour ([`ColorToken::resolve`], or the `Into<Hsla>`/
//! `Into<Rgba>`/`Into<gpui::Fill>`/`Into<gpui::Background>` impls every GPUI builder method
//! already accepts), the token looks its own key up in whichever theme palette is live right now
//! ([`CURRENT_THEME`]) and falls back to its compiled default when that palette doesn't name it.
//!
//! Jerry Dark itself is the identity case - no palette is installed at all, so `resolve()` returns
//! the token's own `default` completely unchanged, not even a lossy `Rgba -> Hsla -> Rgba` round
//! trip, and every existing exact-hex test (`lang_token_tests`, etc.) keeps passing bit-for-bit.
//!
//! ## The key is the whole contract with a theme file
//!
//! A theme file (`assets/themes/*.toml`, or a user's own `~/.config/jerry/themes/*.toml` - see
//! `crate::settings::custom_theme`'s own module docs for the real format) names tokens by exactly
//! these keys, grouped into `[module]` tables:
//!
//! ```toml
//! [surface]
//! window = "#0e0f11"
//!
//! [syntax]
//! keyword = "#b477cf"
//! ```
//!
//! `"{module}.{const name lowercased}"` is the whole naming rule - `surface::WINDOW` is
//! `surface.window`, `syntax::FUNCTION_METHOD` is `syntax.function_method`. A `(fg, bg)` pair
//! token gets two keys (`agent.sonnet.fg`/`agent.sonnet.bg`), an array token one per element
//! (`graph.lanes.0` .. `graph.lanes.5`). Nothing derives one token's colour from another's any
//! more: **every** token here has its own independent key and its own literal default, including
//! the ~37 that used to be plain Rust-level aliases of some other const (`syntax::FUNCTION_METHOD`
//! was `= FUNCTION`, `editor::CARET` was `= syntax::CARET`, ...). Those defaults are unchanged
//! literal values, so Jerry Dark looks exactly as it always did, but a theme file can now move any
//! one of them without dragging its former alias-partner along.
//!
//! [`TOKEN_GROUPS`] is the real, complete registry every one of those tokens is reachable through.
//! See its own docs for what walks it (theme-file key validation, the built-in theme generator,
//! the "generate a theme from one colour" utility) and for the source-parsing test that keeps it
//! honestly total.
//!
//! ## The HSL derivation is still here, just not in the live path any more
//!
//! [`derive_shift`]/[`apply_shift`] - the real, systematic hue/saturation/lightness transform that
//! used to compute *every* non-Jerry-Dark colour on the fly at resolve time - are now an offline
//! *authoring* utility: they generate a full, literal palette (see [`derived_palette`]) that gets
//! written into a real theme file. The five built-in non-Jerry-Dark themes were migrated onto
//! literal files that way (`crate::settings::builtin_themes`), and the Themes page's "Generate
//! from colour" action uses the same code for a user's own seed colour. Live resolution is a plain
//! hash lookup - no HSL maths, no per-token derivation, no `Rgba -> Hsla -> Rgba` round trip.
//!
//! [`token`] reimplements `gpui::rgb`'s byte-extraction formula as a real `const fn` (GPUI's own
//! `rgb()`/`Into<Hsla>` conversions aren't `const fn` - `vendor/zed/crates/gpui/src/color.rs:14,677`
//! - so a literal `const Hsla`/`const Rgba` token wouldn't compile).
//!
//! Module names (`surface`, `border`, `text`, `status`, `diff`, `syntax`, `term`, `agent`,
//! `lang`, `button`, `toggle`, `tag`, `radius`, `band`, `zone`, `shadow`, ...) match `tokens.rs`
//! so call sites can reference e.g. `theme::status::ASK` unchanged. `radius`/`band`/`zone` are
//! [`gpui::Pixels`] (via `gpui::px`, `vendor/zed/crates/gpui/src/geometry.rs:3736`) since GPUI's
//! sizing methods consume `Pixels` directly; `shadow` is `(Pixels, Pixels, Pixels)` for
//! `(x-offset, y-offset, blur-radius)`. None of those are colours, so none of them is a
//! [`ColorToken`] and none is themeable.
//!
//! `font` (not present in `tokens.rs`) carries the two bundled font family names - see
//! `crate::fonts`.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{px, Hsla, Pixels, Rgba};

/// A real, compiled theme palette: every key a theme (and everything up its `base` chain) actually
/// names, mapped to the literal colour it resolves to. Keys are the exact `&'static str`s
/// [`TOKEN_GROUPS`]' tokens carry - a theme file's own text keys are matched against the registry
/// ([`token_for_key`]) while the palette is being compiled, so an unknown key is a real, reported
/// error at that point rather than a silently-ignored entry nothing ever reads.
pub type Palette = HashMap<&'static str, Rgba>;

/// `0xrrggbb` -> a real, opaque [`Rgba`], as a `const fn` (see the module docs).
pub const fn hex_rgba(v: u32) -> Rgba {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.0,
        g: ((v >> 8) & 0xff) as f32 / 255.0,
        b: (v & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// Declares one real design token: its stable theme-file `key` (see the module docs' naming rule)
/// and Jerry Dark's own literal `0xrrggbb` default. The one constructor every token below uses.
pub const fn token(key: &'static str, value: u32) -> ColorToken {
    ColorToken {
        key,
        default: hex_rgba(value),
    }
}

/// A design token: a stable [`key`](Self::key) a theme file can name it by, plus Jerry Dark's own
/// literal [`default`](Self::default) colour - resolved against whichever theme is really live
/// only at the point something actually renders it (see the module docs).
///
/// `Copy`/`const`-constructible so it's a drop-in for the plain `Rgba` these tokens used to be: a
/// bare `theme::surface::WINDOW` still works unchanged at every GPUI builder call site
/// (`.bg(...)`, `.text_color(...)`, ...) via the [`From`] impls below, and still works inside
/// another `const` definition (it's itself real `const`-evaluable, unlike a function call).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorToken {
    /// This token's real, stable theme-file key - `"{module}.{const name lowercased}"`, e.g.
    /// `"surface.window"`. Empty (`""`) only for [`ColorToken::literal`]'s deliberately
    /// unthemeable one-off colours, which no theme file can name.
    pub key: &'static str,
    /// Jerry Dark's own literal colour for this token - what [`ColorToken::resolve`] returns
    /// whenever the live palette doesn't name [`Self::key`] (including the no-palette-at-all
    /// Jerry Dark case itself).
    pub default: Rgba,
}

impl ColorToken {
    /// A one-off, deliberately **unthemeable** colour wearing the [`ColorToken`] type so it can be
    /// passed to the same builder methods a real token is. Its key is `""`, which no registry entry
    /// and therefore no theme file can ever name, so [`Self::resolve`] always returns `color`
    /// unchanged. The real, and only, use for this is `crate::work_surface::TRANSPARENT` - a fully
    /// transparent fill a few call sites pass where a colour is structurally required but nothing
    /// should actually be painted. Re-tinting "nothing" is meaningless, so this is honest rather
    /// than a gap: a transparent quad has no theme.
    pub const fn literal(color: Rgba) -> ColorToken {
        ColorToken {
            key: "",
            default: color,
        }
    }

    /// Resolves this token against whichever theme palette is really live right now - a single
    /// hash lookup of [`Self::key`], falling straight back to [`Self::default`] both when no
    /// palette is installed at all (Jerry Dark, the identity case) and when the installed palette
    /// simply doesn't name this key (a partial theme file - see [`CURRENT_THEME`]'s own docs).
    pub fn resolve(self) -> Rgba {
        // A `literal` token (empty key) is never themeable by construction - short-circuited here
        // rather than relying on `""` being absent from the palette, so it stays true even for a
        // palette built by hand rather than compiled from a real theme file.
        if self.key.is_empty() {
            return self.default;
        }
        CURRENT_THEME.with(|cell| match cell.borrow().as_ref() {
            Some(palette) => palette.get(self.key).copied().unwrap_or(self.default),
            None => self.default,
        })
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
    /// The live-selected theme's own fully compiled [`Palette`], or `None` for Jerry Dark - the
    /// real identity case, where every token's own compiled `default` wins with no lookup at all.
    /// Written by exactly one place, `crate::settings::render::AdeApp::apply_theme_selection`
    /// (via [`set_current_theme`]), which compiles the selection - the theme's own explicit
    /// entries layered on top of everything up its `base` chain - once, at selection time. Live
    /// resolution is then a plain hash lookup per token, not a per-token HSL derivation the way
    /// this module's pre-rewrite mechanism worked.
    ///
    /// A [`std::thread_local`], not a value threaded through every render call's parameters, by
    /// deliberate design: every one of this module's ~270 colour tokens is a bare, freestanding
    /// `const` read from dozens of files across the whole app (`theme::surface::WINDOW`,
    /// `theme::text::BODY`, ...), the overwhelming majority several layers deep inside plain,
    /// `gpui`-context-free helper functions (`crate::sidebar::changes::stat_segment_color`,
    /// `crate::rail::status::Status::color`, ...) that have no `AdeApp`/`Context`/theme parameter
    /// to receive a selection through, and adding one to every such signature across the codebase
    /// (the exact churn `crate::root::AdeApp::ui_text_size`'s own narrower, opt-in scaling
    /// mechanism was deliberately kept away from, per that function's own docs) would be a
    /// materially larger, riskier change than this app's actual, real architecture needs.
    ///
    /// The `thread_local!`-not-process-global part carries over verbatim from the mechanism this
    /// replaced, and for a reason a real audit found the hard way. This started as a plain global
    /// (reasoning: "nothing in this app ever wants a different selected theme on a different
    /// thread", true for the real, single-foreground-thread production app per `vendor/zed/
    /// CLAUDE.md`'s own "All use of entities and UI rendering occurs on a single foreground
    /// thread" note) - the flaw: `cargo test`'s default (parallel) mode runs each `#[gpui::test]`
    /// on its *own* OS thread, and since essentially every test in this crate constructs at least
    /// one `AdeApp` (whose constructor always calls `Self::apply_theme_selection`, writing this
    /// value), a single shared global meant any test asserting a non-default resolved colour could
    /// be corrupted mid-assertion by a completely unrelated, concurrently-running test's own
    /// `AdeApp` construction resetting it - a real, reproduced flake specific to default (not
    /// `--test-threads=1`) parallelism. A `thread_local!` fixes this for free: each test thread
    /// gets its own independent copy, so tests can never interfere with each other's theme
    /// selection regardless of scheduling, while production is unaffected (there is still only the
    /// one real foreground thread that ever reads or writes this).
    ///
    /// `RefCell<Option<Rc<Palette>>>`, not `Cell`: a compiled palette is a `HashMap`, not `Copy`.
    /// The `Rc` means installing a palette (and cloning one out for a caller that wants to keep
    /// it) is a refcount bump, never a rehash of ~270 entries.
    static CURRENT_THEME: std::cell::RefCell<Option<Rc<Palette>>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs (`Some`) or clears (`None`, back to Jerry Dark) the live theme palette - see
/// [`CURRENT_THEME`]'s own docs. Callers must also force a real repaint
/// (`App::refresh_windows`, `vendor/zed/crates/gpui/src/app.rs:1025`) afterward: this function only
/// swaps the thread-local; nothing here can reach into GPUI's own render loop to schedule one.
pub fn set_current_theme(palette: Option<Rc<Palette>>) {
    CURRENT_THEME.with(|cell| *cell.borrow_mut() = palette);
}

/// The live palette, if one is installed - `None` means Jerry Dark (see [`CURRENT_THEME`]).
/// Cheap: an `Rc` clone, not a copy of the map.
pub fn current_theme_palette() -> Option<Rc<Palette>> {
    CURRENT_THEME.with(|cell| cell.borrow().clone())
}

/// Every real [`ColorToken`] in this module, grouped by the module that declares it - the whole-app
/// registry, and the single source of truth for "which keys are real" that
/// `crate::settings::custom_theme`'s key validation, `crate::settings::builtin_themes`' theme-file
/// generator, and the "generate a theme from one colour" action all read.
///
/// Each entry is `(module name, that module's own `TOKENS` slice)`, and each `TOKENS` entry is
/// `(Rust const name, the token itself)`. The const name is carried purely so
/// [`token_registry_tests`] can check it against the token's own key; nothing at runtime needs it.
///
/// Kept honestly total by [`token_registry_tests::every_real_color_token_in_this_file_is_registered`],
/// which parses this file's own source (`include_str!`) and fails if a `pub const ...: ColorToken`
/// declaration exists that no `TOKENS` slice lists, or if a module declaring tokens is missing from
/// this list - so adding a token without registering it is a test failure, not a colour that
/// silently can't be themed.
pub const TOKEN_GROUPS: &[(&str, &[(&str, ColorToken)])] = &[
    ("surface", surface::TOKENS),
    ("border", border::TOKENS),
    ("tree", tree::TOKENS),
    ("text", text::TOKENS),
    ("status", status::TOKENS),
    ("rail", rail::TOKENS),
    ("diff", diff::TOKENS),
    ("syntax", syntax::TOKENS),
    ("editor", editor::TOKENS),
    ("term", term::TOKENS),
    ("env", env::TOKENS),
    ("agent", agent::TOKENS),
    ("lang", lang::TOKENS),
    ("button", button::TOKENS),
    ("toggle", toggle::TOKENS),
    ("tag", tag::TOKENS),
    ("completions_popup", completions_popup::TOKENS),
    ("settings", settings::TOKENS),
    ("scrollbar", scrollbar::TOKENS),
    ("graph", graph::TOKENS),
    ("palette", palette::TOKENS),
];

/// Every real registered [`ColorToken`], flattened across [`TOKEN_GROUPS`] - registry order
/// (module by module, declaration order within a module), which is also the order a generated
/// theme file's tables and keys come out in.
pub fn all_tokens() -> impl Iterator<Item = ColorToken> {
    TOKEN_GROUPS
        .iter()
        .flat_map(|(_, tokens)| tokens.iter().map(|(_, token)| *token))
}

/// The real registered token for a theme file's `"{module}.{key}"` text, or `None` if nothing in
/// [`TOKEN_GROUPS`] declares that key - the one lookup that turns a user-authored (or
/// VSCode-converted) string into a real, `&'static str`-keyed [`Palette`] entry, and the check
/// behind `crate::settings::custom_theme::ThemeFileError::UnknownKey`.
///
/// Linear over ~270 entries, called once per key while *compiling* a theme (never on the live
/// resolve path) - a real, deliberate "simple beats clever at this size" call, not an oversight.
pub fn token_for_key(key: &str) -> Option<ColorToken> {
    all_tokens().find(|token| token.key == key)
}

/// Real, general "is this a light theme" check - the resolved window background's own HSL
/// lightness, `> 0.5`. Generalizes the old hardcoded `name == "Paper"` special case
/// (`crate::settings::render::AdeApp::set_theme_name`'s `last_dark_theme` bookkeeping used to
/// compare literal theme names) so a disk-loaded custom theme's own light/dark status is
/// determined the same real way a built-in one's is: "Paper" compiles `surface.window` to a
/// genuinely light value, every other bundled theme to a near-black one.
pub fn theme_is_light(window_background: Rgba) -> bool {
    let hsla: Hsla = window_background.into();
    hsla.l > 0.5
}

/// A real, systematic HSL transform - see [`derive_shift`]'s own docs for how one is computed,
/// [`apply_shift`] for how it's applied to a single colour, and [`derived_palette`] for the real
/// whole-palette generation both of this module's remaining callers use.
///
/// **Not part of live theme resolution.** Before this module's rewrite, every non-Jerry-Dark
/// colour in the app was computed by running one of these over a token's own default on every
/// single `resolve()` call; now it is strictly an *authoring-time* tool that produces literal
/// colours to be written into a real, hand-editable theme file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HslShift {
    /// Added to hue (wraps via `rem_euclid`), in the same 0.0..=1.0 range `gpui::Hsla::h` uses.
    pub hue: f32,
    /// Multiplies saturation.
    pub saturation_scale: f32,
    /// `new_lightness = old_lightness * lightness_scale + lightness_offset` - a linear remap,
    /// not a plain additive shift, so a light theme (`Paper`) can be derived from Jerry Dark's
    /// own near-black baseline without every already-light token clipping at 100%. Clamped to
    /// `0.0..=1.0` in [`apply_shift`].
    pub lightness_scale: f32,
    pub lightness_offset: f32,
}

/// The no-op shift - what [`derive_shift`] returns for a target identical to its base.
pub const IDENTITY_SHIFT: HslShift = HslShift {
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
/// theme, [`derive_shift`]'s linear lightness fit solves a *negative* `lightness_scale` (Jerry
/// Dark's near-black window background maps to Paper's near-white one) - correct for the two
/// background swatches the fit is solved from, but it also means several of Jerry Dark's already
/// fairly-light `text::*` tokens (`SELECTED`/`PRIMARY`/`HEADING`) get mapped *below* `0.0`
/// lightness before the `.clamp(0.0, 1.0)` below brings them back to pure black - collapsing what
/// were three distinguishable text levels in Jerry Dark into one on Paper. That is exactly as true
/// of the generated `assets/themes/paper.toml` as it was of the old live derivation (the migration
/// was required to be bit-identical), with one real improvement: those values are now literal lines
/// in a file anyone can retune by hand, one token at a time, instead of an emergent property of a
/// transform nobody could override.
pub fn apply_shift(base: Rgba, shift: HslShift) -> Rgba {
    let mut hsla: Hsla = base.into();
    hsla.h = (hsla.h + shift.hue).rem_euclid(1.0);
    hsla.s = (hsla.s * shift.saturation_scale).clamp(0.0, 1.0);
    hsla.l = (hsla.l * shift.lightness_scale + shift.lightness_offset).clamp(0.0, 1.0);
    hsla.into()
}

/// Derives a real, systematic [`HslShift`] from two themes' own five `[background, panel,
/// green-ish, amber-ish, blue-ish]` swatches - the mechanism the five migrated built-in theme
/// files were generated with (`crate::settings::builtin_themes`, which pins each theme's own
/// original swatches) and the one an imported VSCode theme's whole-app chrome still goes through
/// (`crate::settings::vscode_theme`) for the many tokens no VSCode colour key maps onto:
///
/// - **Lightness** is a linear fit (`scale`/`offset`, not a plain additive shift) solved exactly
///   from the two background-ish swatches (index 0, the window background; index 1, the panel
///   background) - two points exactly determine a line. This is what lets a light theme (real
///   example: `Paper`, whose swatches are genuinely light hex values) be derived correctly from
///   Jerry Dark's own near-black tokens without every already-fairly-light token clipping at
///   100%: a plain `lightness + delta` shift would either undershoot on Jerry Dark's darkest
///   tokens or blow straight through 1.0 on its lighter ones; dividing this same linear fit by
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
pub fn derive_shift(base_swatches: [u32; 5], target_swatches: [u32; 5]) -> HslShift {
    fn hsla_of(hex_value: u32) -> Hsla {
        hex_rgba(hex_value).into()
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

/// Jerry Dark's own real accent-blue token ([`syntax::FUNCTION`]/`#74ade8`, the same value the
/// pre-rewrite five-swatch fixture used as its "blue-ish" swatch) - the reference hue
/// [`shift_from_seed`] rotates a user's seed colour against. Pinned here as the one place that
/// choice is made, rather than repeated at each caller.
const SEED_REFERENCE_ACCENT: u32 = 0x74ade8;

/// Derives an [`HslShift`] from a single seed colour - the real maths behind the Themes page's
/// "Generate from colour" action (GitHub issue #141; Colin's own "we can just keep a setting to
/// generate a theme from a colour using it").
///
/// One colour cannot honestly determine all four degrees of freedom [`derive_shift`] solves from
/// five swatches, so this makes a real, documented, deliberately narrow choice: **hue and
/// saturation only**. The whole Jerry Dark palette is rotated so that its own accent blue
/// ([`SEED_REFERENCE_ACCENT`]) lands exactly on the seed's hue, and every token's saturation is
/// scaled by the same ratio the seed has against that accent; lightness is left completely alone
/// (`scale 1.0`, `offset 0.0`).
///
/// That is the honest reading of "generate a theme from a colour": you pick the app's accent, and
/// everything else follows it while keeping Jerry Dark's own carefully-tuned light/dark structure
/// intact. It deliberately does *not* try to guess whether you wanted a light theme from a light
/// seed - inverting lightness needs a real second reference point (which is exactly what
/// [`derive_shift`]'s two background swatches are), and guessing at one would produce the
/// "precise-looking answer that is really a vibe match" this codebase's own conventions reject.
/// The generated file is a full, literal, hand-editable palette, so retuning lightness afterwards
/// is a real, supported next step rather than a dead end.
pub fn shift_from_seed(seed: Rgba) -> HslShift {
    let seed_hsla: Hsla = seed.into();
    let reference: Hsla = hex_rgba(SEED_REFERENCE_ACCENT).into();
    let saturation_scale = if reference.s > 0.001 {
        (seed_hsla.s / reference.s).clamp(0.0, 3.0)
    } else {
        1.0
    };
    HslShift {
        hue: (seed_hsla.h - reference.h).rem_euclid(1.0),
        saturation_scale,
        lightness_scale: 1.0,
        lightness_offset: 0.0,
    }
}

/// Runs `shift` over **every** real registered token ([`TOKEN_GROUPS`]) and hands back the whole
/// resulting palette as real, literal `(key, colour)` pairs in registry order - the one shared
/// generator behind both the built-in theme migration (`crate::settings::builtin_themes`) and the
/// "generate a theme from one colour" action, so those two can never derive palettes differently.
///
/// This is exactly what the pre-rewrite mechanism computed lazily, per token, on every single
/// `resolve()` call; computing it once up front instead is what let those palettes become real
/// files.
pub fn derived_palette(shift: HslShift) -> Vec<(&'static str, Rgba)> {
    all_tokens()
        .map(|token| (token.key, apply_shift(token.default, shift)))
        .collect()
}
/// Backgrounds - every solid fill in the app, from the window itself down to popovers,
/// hover states and keycaps.
pub mod surface {
    use super::{token, ColorToken};

    pub const WINDOW: ColorToken = token("surface.window", 0x0e0f11); // window body
    pub const WINDOW_BORDER: ColorToken = token("surface.window_border", 0x262a2e);
    pub const TITLE_BAR: ColorToken = token("surface.title_bar", 0x101214);
    pub const RAIL: ColorToken = token("surface.rail", 0x101113); // left rail + right panel
    pub const CENTER: ColorToken = token("surface.center", 0x131518); // work surface
    pub const PTY: ColorToken = token("surface.pty", 0x0d0f11); // agent CLI + terminal
    pub const HEADER: ColorToken = token("surface.header", 0x121417); // context bar, panel headers
    pub const FOOTER: ColorToken = token("surface.footer", 0x111316); // surface footers, status strips
    pub const CARD: ColorToken = token("surface.card", 0x161a1d); // composer, settings cards
    pub const CARD_SUNK: ColorToken = token("surface.card_sunk", 0x131619); // card footers
    pub const POPOVER: ColorToken = token("surface.popover", 0x181c20); // completion popup, hover card
    pub const PALETTE: ColorToken = token("surface.palette", 0x15181b);
    pub const SCRIM: ColorToken = token("surface.scrim", 0x060708); // at 62% alpha behind the palette
    pub const ROW_HOVER: ColorToken = token("surface.row_hover", 0x15181b);
    pub const ROW_HOVER_ALT: ColorToken = token("surface.row_hover_alt", 0x1b1f22); // hover on chrome buttons
    pub const ROW_SELECTED: ColorToken = token("surface.row_selected", 0x1a1e21);
    pub const SEGMENT_TRACK: ColorToken = token("surface.segment_track", 0x171a1d);
    pub const SEGMENT_ACTIVE: ColorToken = token("surface.segment_active", 0x242a2f);
    pub const KEYCAP: ColorToken = token("surface.keycap", 0x181c1f);
    /// The hint-size keycap's own background - distinct from [`KEYCAP`]'s standard-size
    /// `#181c1f` (`Jerry.dc.html`: `background:#15181a;border:1px solid #23272b`).
    pub const KEYCAP_HINT: ColorToken = token("surface.keycap_hint", 0x15181a);
    pub const CHIP_NEUTRAL: ColorToken = token("surface.chip_neutral", 0x23272b);
    pub const CURRENT_LINE: ColorToken = token("surface.current_line", 0x181c20);
    /// The Windows/Linux title bar's close caption button's hover fill. The design handoff
    /// (`Jerry.dc.html`: `style-hover="background:#8c3a38"`, unchanged through revision 3) spec'd
    /// a muted maroon; Colin asked for this to be the real Windows Fluent Design close-hover red
    /// (`#E81123`, the same color Windows 10/11's own native title bar uses) instead - a
    /// deliberate override of the handoff, not a stale-spec bug.
    pub const TITLE_BAR_CLOSE_HOVER: ColorToken = token("surface.title_bar_close_hover", 0xe81123);
    /// GitHub issue #129: the shared row-hover fill for every dropdown/context-menu in the app
    /// (`+` menu, title bar menu, tree context menu, git graph's push/row menus) - distinct from
    /// [`ROW_HOVER`]/[`ROW_HOVER_ALT`], which are for plain list rows and chrome buttons, not
    /// menu popovers. Named `MENU_ROW_HOVER`, not `PLUS_MENU_ROW_HOVER` (its name before this
    /// issue) - it was already shared by four menus, not just the `+` one, before this rename;
    /// only the name had drifted from what it actually covers. Deliberately *not* used by the
    /// command palette (`theme::palette::ROW_HOVER`, its own real token - GitHub issue #129 kept
    /// the palette its own thing on purpose) or the LSP completion popup (keyboard-navigated, not
    /// mouse-hover styled at all - a real, mockup-verified design difference, not drift; see
    /// `crate::lsp::completion_popup`'s own module docs).
    pub const MENU_ROW_HOVER: ColorToken = token("surface.menu_row_hover", 0x1d2226);
    /// A file tab's close-affordance hover fill - one hex step off [`CHIP_NEUTRAL`]
    /// (`#23272b`), kept as its own token.
    pub const TAB_CLOSE_HOVER: ColorToken = token("surface.tab_close_hover", 0x23282c);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("WINDOW", WINDOW),
        ("WINDOW_BORDER", WINDOW_BORDER),
        ("TITLE_BAR", TITLE_BAR),
        ("RAIL", RAIL),
        ("CENTER", CENTER),
        ("PTY", PTY),
        ("HEADER", HEADER),
        ("FOOTER", FOOTER),
        ("CARD", CARD),
        ("CARD_SUNK", CARD_SUNK),
        ("POPOVER", POPOVER),
        ("PALETTE", PALETTE),
        ("SCRIM", SCRIM),
        ("ROW_HOVER", ROW_HOVER),
        ("ROW_HOVER_ALT", ROW_HOVER_ALT),
        ("ROW_SELECTED", ROW_SELECTED),
        ("SEGMENT_TRACK", SEGMENT_TRACK),
        ("SEGMENT_ACTIVE", SEGMENT_ACTIVE),
        ("KEYCAP", KEYCAP),
        ("KEYCAP_HINT", KEYCAP_HINT),
        ("CHIP_NEUTRAL", CHIP_NEUTRAL),
        ("CURRENT_LINE", CURRENT_LINE),
        ("TITLE_BAR_CLOSE_HOVER", TITLE_BAR_CLOSE_HOVER),
        ("MENU_ROW_HOVER", MENU_ROW_HOVER),
        ("TAB_CLOSE_HOVER", TAB_CLOSE_HOVER),
    ];
}

/// The 1px rules that separate things - zone edges, card and popover outlines, and the
/// selected-row edge.
pub mod border {
    use super::{token, ColorToken};

    pub const ZONE: ColorToken = token("border.zone", 0x1e2225); // between the three zones
    pub const INNER: ColorToken = token("border.inner", 0x1c2023); // between bands inside a zone
    pub const RAIL_INNER: ColorToken = token("border.rail_inner", 0x191c1f);
    pub const ROW: ColorToken = token("border.row", 0x171a1c); // change-list row separators
    pub const DIVIDER: ColorToken = token("border.divider", 0x22262a); // 1px vertical rules
    pub const CARD: ColorToken = token("border.card", 0x23282c);
    pub const CARD_FIELD: ColorToken = token("border.card_field", 0x22272b);
    pub const COMPOSER: ColorToken = token("border.composer", 0x24292e);
    pub const POPOVER: ColorToken = token("border.popover", 0x2b3238);
    pub const BUTTON: ColorToken = token("border.button", 0x2a2f34); // outline button
    pub const BUTTON_DISABLED: ColorToken = token("border.button_disabled", 0x1f2327);
    pub const KEYCAP: ColorToken = token("border.keycap", 0x272c31);
    /// The hint-size keycap's own border - see [`super::surface::KEYCAP_HINT`].
    pub const KEYCAP_HINT: ColorToken = token("border.keycap_hint", 0x23272b);
    pub const SELECTED_EDGE: ColorToken = token("border.selected_edge", 0x3f5b74); // 2px left edge on a selected row

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("ZONE", ZONE),
        ("INNER", INNER),
        ("RAIL_INNER", RAIL_INNER),
        ("ROW", ROW),
        ("DIVIDER", DIVIDER),
        ("CARD", CARD),
        ("CARD_FIELD", CARD_FIELD),
        ("COMPOSER", COMPOSER),
        ("POPOVER", POPOVER),
        ("BUTTON", BUTTON),
        ("BUTTON_DISABLED", BUTTON_DISABLED),
        ("KEYCAP", KEYCAP),
        ("KEYCAP_HINT", KEYCAP_HINT),
        ("SELECTED_EDGE", SELECTED_EDGE),
    ];
}

/// The Files tree's own structural marks (GitHub issue #18 §3). A scope of its own rather than
/// two more entries in [`border`]: these are painted *inside* rows as 1px quads, not the border
/// of anything, and keeping them together makes the pair's relationship - one resting, one
/// highlighted - obvious. Both are ordinary [`ColorToken`]s, so a theme file can move them exactly
/// like the ~270 tokens around them.
pub mod tree {
    use super::{token, ColorToken};

    /// The resting indent guide *defaults to* [`super::border::DIVIDER`]'s own value (`#22262a`),
    /// this palette's existing "1px vertical rule" colour, so out of the box the guides read as
    /// structure rather than content. Subtle by design: a guide that competes with a filename is
    /// worse than none. Before this module's rewrite this was a literal Rust-level alias of that
    /// const; it is now its own independently-keyed token that merely starts at the same value, so
    /// a theme can retune tree guides without touching every 1px rule in the app.
    pub const INDENT_GUIDE: ColorToken = token("tree.indent_guide", 0x22262a);
    /// The guide for a level in the selected file's ancestor chain - defaults to
    /// [`super::border::SELECTED_EDGE`]'s own value (`#3f5b74`), the same blue the selected-row
    /// edge uses, so "this line leads to what's selected" is the same visual language in both
    /// places by default. Independently keyed for the same reason as above.
    pub const INDENT_GUIDE_ACTIVE: ColorToken = token("tree.indent_guide_active", 0x3f5b74);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("INDENT_GUIDE", INDENT_GUIDE),
        ("INDENT_GUIDE_ACTIVE", INDENT_GUIDE_ACTIVE),
    ];
}

/// The interface text ramp, brightest to dimmest. Not code: that is `syntax`.
pub mod text {
    use super::{token, ColorToken};

    pub const SELECTED: ColorToken = token("text.selected", 0xdde2e7);
    pub const PRIMARY: ColorToken = token("text.primary", 0xd3d8dd);
    pub const HEADING: ColorToken = token("text.heading", 0xc8cdd2);
    pub const STRONG: ColorToken = token("text.strong", 0xc2c7cc);
    pub const BODY: ColorToken = token("text.body", 0xb8bfc6);
    pub const SECONDARY: ColorToken = token("text.secondary", 0xa9b0b7);
    pub const MUTED: ColorToken = token("text.muted", 0x9aa1a8);
    pub const DIM: ColorToken = token("text.dim", 0x8b9197);
    pub const DIMMER: ColorToken = token("text.dimmer", 0x7d848b);
    pub const FAINT: ColorToken = token("text.faint", 0x6b7178);
    pub const FAINTER: ColorToken = token("text.fainter", 0x5e646a);
    pub const GHOST: ColorToken = token("text.ghost", 0x4e545a);
    pub const GHOSTER: ColorToken = token("text.ghoster", 0x454b51);
    pub const HINT: ColorToken = token("text.hint", 0x41464b);
    pub const GUTTER: ColorToken = token("text.gutter", 0x3a3f44);
    pub const DISABLED: ColorToken = token("text.disabled", 0x3d4248);
    /// The context bar's worktree path text (`README.md`: "worktree path 10.5px mono
    /// `#4a5057`") - one hex step off [`GHOST`]; not in `tokens.rs`'s `text` module, added
    /// here directly.
    pub const PATH: ColorToken = token("text.path", 0x4a5057);
    /// The file tree row's `▾`/`▸` caret - same hex as [`PATH`] but a distinct token for a
    /// distinct element.
    pub const TREE_CARET: ColorToken = token("text.tree_caret", 0x4a5057);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("SELECTED", SELECTED),
        ("PRIMARY", PRIMARY),
        ("HEADING", HEADING),
        ("STRONG", STRONG),
        ("BODY", BODY),
        ("SECONDARY", SECONDARY),
        ("MUTED", MUTED),
        ("DIM", DIM),
        ("DIMMER", DIMMER),
        ("FAINT", FAINT),
        ("FAINTER", FAINTER),
        ("GHOST", GHOST),
        ("GHOSTER", GHOSTER),
        ("HINT", HINT),
        ("GUTTER", GUTTER),
        ("DISABLED", DISABLED),
        ("PATH", PATH),
        ("TREE_CARET", TREE_CARET),
    ];
}

/// Status is the only place colour carries meaning in the rail.
pub mod status {
    use super::{token, ColorToken};

    pub const ASK: ColorToken = token("status.ask", 0xe2a336); // needs input
    pub const ASK_BG: ColorToken = token("status.ask_bg", 0x3a2c14);
    pub const FAIL: ColorToken = token("status.fail", 0xe0625c);
    pub const FAIL_BG: ColorToken = token("status.fail_bg", 0x3a1e1e);
    pub const REVIEW: ColorToken = token("status.review", 0x5cb87f);
    pub const REVIEW_BG: ColorToken = token("status.review_bg", 0x1e3b2a);
    pub const RUN: ColorToken = token("status.run", 0x5a9ad4);
    pub const RUN_BG: ColorToken = token("status.run_bg", 0x1e2f3e);
    pub const IDLE: ColorToken = token("status.idle", 0x565d64);
    pub const IDLE_BG: ColorToken = token("status.idle_bg", 0x22262a);
    // waiting-question preview inside a rail row
    pub const ASK_CARD_BG: ColorToken = token("status.ask_card_bg", 0x1c1710);
    pub const ASK_CARD_EDGE: ColorToken = token("status.ask_card_edge", 0x8a6420);
    pub const ASK_CARD_FG: ColorToken = token("status.ask_card_fg", 0xc99b4e);
    // conflict banner
    pub const BANNER_BG: ColorToken = token("status.banner_bg", 0x1b1610);
    pub const BANNER_BORDER: ColorToken = token("status.banner_border", 0x33291a);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("ASK", ASK),
        ("ASK_BG", ASK_BG),
        ("FAIL", FAIL),
        ("FAIL_BG", FAIL_BG),
        ("REVIEW", REVIEW),
        ("REVIEW_BG", REVIEW_BG),
        ("RUN", RUN),
        ("RUN_BG", RUN_BG),
        ("IDLE", IDLE),
        ("IDLE_BG", IDLE_BG),
        ("ASK_CARD_BG", ASK_CARD_BG),
        ("ASK_CARD_EDGE", ASK_CARD_EDGE),
        ("ASK_CARD_FG", ASK_CARD_FG),
        ("BANNER_BG", BANNER_BG),
        ("BANNER_BORDER", BANNER_BORDER),
    ];
}

/// Tokens used only by the Revision R12 rail rewrite (`design_handoff_jerry_ade/revision 3/
/// REVISION-2026-07-31.md` §2) that have no exact match elsewhere in this module - every other
/// colour that section calls for (the branch/note/model/activity greys, the amber flag, the
/// spine/selection edges) already has one, reused directly at the call site rather than
/// duplicated here under a second name.
pub mod rail {
    use super::{token, ColorToken};

    /// Repo group header's uppercase name (§2.1: "name in 9.5px uppercase Plex Sans `#787f86`").
    pub const REPO_HEADER_NAME: ColorToken = token("rail.repo_header_name", 0x787f86);
    /// Active worktree row header background (§2.2: "Active worktree header background
    /// `#181c1f`").
    pub const WORKTREE_ACTIVE_BG: ColorToken = token("rail.worktree_active_bg", 0x181c1f);
    /// Worktree row hover background (§2.2: "hover `#16191c`").
    pub const WORKTREE_HOVER_BG: ColorToken = token("rail.worktree_hover_bg", 0x16191c);
    /// A prunable (merged, clean, agent-less) worktree's 2px left edge (§2.2: "prunable
    /// `#2f353a`"). A bare-but-not-prunable worktree reuses [`super::status::IDLE_BG`]
    /// (`#22262a`), an exact match for the spec's "Bare worktrees `#22262a`".
    pub const PRUNABLE_EDGE: ColorToken = token("rail.prunable_edge", 0x2f353a);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("REPO_HEADER_NAME", REPO_HEADER_NAME),
        ("WORKTREE_ACTIVE_BG", WORKTREE_ACTIVE_BG),
        ("WORKTREE_HOVER_BG", WORKTREE_HOVER_BG),
        ("PRUNABLE_EDGE", PRUNABLE_EDGE),
    ];
}

/// Diff and change colours - added/removed line fills and signs, hunk headers, and the
/// change-list stat bars.
pub mod diff {
    use super::{token, ColorToken};

    pub const ADD_BG: ColorToken = token("diff.add_bg", 0x12211a);
    pub const ADD_FG: ColorToken = token("diff.add_fg", 0x9fd0b2);
    pub const ADD_SIGN: ColorToken = token("diff.add_sign", 0x4e8c68);
    pub const DEL_BG: ColorToken = token("diff.del_bg", 0x211517);
    pub const DEL_FG: ColorToken = token("diff.del_fg", 0xd6a4a0);
    pub const DEL_SIGN: ColorToken = token("diff.del_sign", 0xa35f5b);
    pub const CTX_FG: ColorToken = token("diff.ctx_fg", 0x868d94);
    pub const HUNK_BG: ColorToken = token("diff.hunk_bg", 0x15181c);
    pub const HUNK_FG: ColorToken = token("diff.hunk_fg", 0x5f666e);
    pub const FOLD_BG: ColorToken = token("diff.fold_bg", 0x121417);
    pub const FOLD_FG: ColorToken = token("diff.fold_fg", 0x4a5057);
    pub const STAT_ADD: ColorToken = token("diff.stat_add", 0x5f9c78); // "+142" label
    pub const STAT_DEL: ColorToken = token("diff.stat_del", 0xb06a66); // "-8" label
    pub const STAT_EMPTY: ColorToken = token("diff.stat_empty", 0x22262a); // unused segment of the 5-bar
    pub const GIT_GUTTER: ColorToken = token("diff.git_gutter", 0x2c6244); // 3px agent-touched marker

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("ADD_BG", ADD_BG),
        ("ADD_FG", ADD_FG),
        ("ADD_SIGN", ADD_SIGN),
        ("DEL_BG", DEL_BG),
        ("DEL_FG", DEL_FG),
        ("DEL_SIGN", DEL_SIGN),
        ("CTX_FG", CTX_FG),
        ("HUNK_BG", HUNK_BG),
        ("HUNK_FG", HUNK_FG),
        ("FOLD_BG", FOLD_BG),
        ("FOLD_FG", FOLD_FG),
        ("STAT_ADD", STAT_ADD),
        ("STAT_DEL", STAT_DEL),
        ("STAT_EMPTY", STAT_EMPTY),
        ("GIT_GUTTER", GIT_GUTTER),
    ];
}

/// The editor's per-scope syntax palette - one [`ColorToken`] per `tree-sitter-highlight`
/// capture bucket [`crate::code_surface::code_view::HighlightKind`] classifies a token into. See
/// that type's own docs for the full scope-name -> bucket mapping and the real grammar captures
/// (`tree-sitter-rust`/`-python`/`-javascript`/`-typescript`'s own bundled `queries/highlights.scm`
/// files, read directly off the fetched crates under `~/.cargo/registry/src/`, not guessed) each
/// bucket exists to cover.
///
/// ## The identifier family (the rose/pink and cyan hues)
///
/// [`VARIABLE`], [`VARIABLE_PARAMETER`] and [`PROPERTY`] are real, independently authored hues.
/// They used to default to [`TEXT`]'s near-white `#acb2be`, which was a genuine legibility
/// problem rather than a stylistic preference: plain identifiers, function parameters and field
/// access together are a very large fraction of the tokens in any real source file, so colouring
/// all three as plain text left most of a typical screen reading as one undifferentiated grey.
///
/// The three colours fill hue territory nothing else in this palette had claimed. Measured
/// against every other syntax colour here (a real CIE Lab ΔE sweep, not an eyeball), the closest
/// any of them lands to an existing token is ΔE 16, and the closest two of them land to each
/// other is ΔE 17 - both comfortably above the ~2.3 just-noticeable threshold, so these read as
/// genuinely different colours rather than as tints of something already in use:
///
/// - [`VARIABLE`] `#bd89a5` - a muted dusty rose. Deliberately the quietest of the three (the
///   lowest saturation in the whole palette bar the greys): a plain identifier is the single most
///   common thing this palette ever colours, so it has to be clearly *not* plain text without
///   turning a screen of code into confetti.
/// - [`VARIABLE_PARAMETER`] `#bd566e` - the same rose family, deeper and considerably more
///   saturated. A function's own inputs are worth picking out from the locals around them, and
///   staying in [`VARIABLE`]'s family says "this is still a variable, just a distinguished one"
///   rather than inventing an unrelated hue for a closely related concept.
///
///   It steps *down* in lightness rather than up, which is a real constraint rather than a
///   preference: "Paper", the bundled light theme, is derived by inverting lightness (see
///   [`apply_shift`]'s own docs), so any token much lighter than about 72% clips to near-black
///   there and collapses into the other light tokens. A brighter pink parameter measured a fine
///   ΔE 16 from plain text in Jerry Dark and ΔE 9.5 - i.e. barely distinguishable - in Paper. The
///   deeper tone keeps at least ΔE 16 from both plain text and [`VARIABLE`] in *every* bundled
///   theme, which is what [`syntax_identifier_palette_tests`] now pins.
/// - [`PROPERTY`] `#75b2c7` - a muted cyan-blue, deliberately *outside* the rose family. A field
///   access is not a local binding: it is a name looked up on another object, and giving it a
///   cool counterpart to the warm locals is what makes an `a.b.c` chain legible at a glance.
///
/// Colours the maintainer explicitly kept out of this pass: [`OPERATOR`],
/// [`PUNCTUATION_BRACKET`], [`PUNCTUATION_DELIMITER`] and [`EMBEDDED`] still default to [`TEXT`]'s
/// own `#acb2be`. That is this palette's long-standing, deliberate choice not to colour operators
/// and punctuation (see the historical design note preserved on
/// [`crate::code_surface::code_view::HighlightKind`]); real bracket-pair colouring is a separate,
/// larger feature to be considered on its own terms, not something to smuggle in here.
///
/// ## The bracket-pair depth ring (GitHub issue #168)
///
/// That separate feature has since landed, and it deliberately did **not** change the paragraph
/// above: [`PUNCTUATION_BRACKET`] is still exactly [`TEXT`], pinned by
/// [`syntax_contrast_tests::operators_and_punctuation_deliberately_still_render_as_plain_text`].
/// It is now the *fallback* a bracket keeps when it has no real matching partner, which is what
/// makes malformed/mid-edit code degrade visibly-but-quietly instead of lying about structure.
///
/// A bracket that *is* half of a real matched pair paints one of six ring colours
/// ([`BRACKET_1`] .. [`BRACKET_6`]), chosen by `nesting depth % 6`, both halves alike - so a pair
/// and everything scoped inside it can be traced at a glance. Six, cycling quickly, matches what
/// VSCode and most editors that ship this feature use; a longer ring buys nothing once adjacent
/// depths are already unmistakable. `graph::LANES` is this file's existing precedent for an
/// N-colour rotation, but these are six independent flat consts rather than a `[ColorToken; 6]`
/// array for a real reason: each one's key has to be exactly `syntax.{HighlightKind::name()}`
/// (see [`crate::settings::vscode_theme::tests::every_highlight_kind_maps_onto_a_real_syntax_token`]),
/// and an array token's key carries a dotted index (`graph.lanes.0`) that a `[syntax]` table key
/// is documented never to have.
///
/// The six are **not independently chosen colours**. Each one is derived from a hue this palette
/// already speaks, held at this palette's own chroma and lightness register, so the ring reads as
/// "this palette, cycling" rather than as six new colours nobody else in this file uses:
///
/// | ring slot | hue borrowed from | Lab hue (anchor) |
/// |---|---|---|
/// | [`BRACKET_1`] `#eb7f7b` salmon | [`VARIABLE_PARAMETER`]'s rose-red | 27 (9) |
/// | [`BRACKET_2`] `#7dd7b9` mint | [`ATTRIBUTE`]'s teal | 169 (185) |
/// | [`BRACKET_3`] `#c48648` amber | [`CONSTANT`]'s brown | 68 (71) |
/// | [`BRACKET_4`] `#8f7cbd` violet | [`KEYWORD`]'s purple | 304 (317) |
/// | [`BRACKET_5`] `#6c9052` moss | [`STRING`]'s green | 130 (123) |
/// | [`BRACKET_6`] `#2d96c7` steel blue | [`FUNCTION`]'s blue | 249 (266) |
///
/// Those six anchors are the widest-spread six of this palette's nine real hues (minimum gap 51
/// degrees), so the ring covers the whole wheel without ever leaving the palette's vocabulary.
/// Each slot is then offset from its anchor - in hue by up to ~18 degrees, and more importantly in
/// lightness - so a coloured bracket never impersonates the semantic token it borrows from: the
/// tightest is [`BRACKET_6`] against [`FUNCTION`] at ΔE 14.8, and no ring colour comes within
/// ΔE 14 of any semantic token.
///
/// ## Why this replaced the first version of this ring
///
/// The first ring shipped here (`#39e9d9` turquoise, `#af52ec` violet, `#36d535` green, ...) was
/// produced by maximising pairwise CIE-Lab ΔE in open colour space, and it was wrong in a way the
/// distinctness tests could not see. Maximising ΔE rewards **chroma**, so the optimiser bought
/// separation by cranking saturation, and the result sat completely outside this palette's own
/// register:
///
/// | | mean C* | max C* |
/// |---|---|---|
/// | this palette's non-neutral tokens | 33.7 | 53.4 ([`KEYWORD`]) |
/// | the replaced ring | **66.4** | **93.3** |
/// | this ring | 39.7 | 46.0 |
///
/// Two of the six were nearly twice as saturated as the most saturated colour this palette had
/// ever used. Every distinctness check passed; it still read as a jarring accent dropped on top of
/// a muted palette, which is exactly what it was. The lesson is recorded here because the failure
/// is not obvious from the tests: *a colour set can be perfectly distinguishable and still not
/// belong*, and [`syntax_bracket_ring_tests::the_ring_stays_inside_the_palettes_own_chroma_register`]
/// exists specifically to catch a future change re-introducing it.
///
/// The distinctness floors below are correspondingly lower than the replaced ring's, and
/// deliberately so - they are now set from what a reader actually needs rather than from what an
/// unconstrained optimiser happened to reach. Measured across **every** bundled theme, not just
/// Jerry Dark (the five others are generated from these defaults by [`derive_shift`], so a value
/// that is fine here can still collapse under one of them):
///
/// - **Cyclically adjacent depths** (`n` against `n + 1`, the only comparison a reader actually
///   makes, since those two nest directly inside one another): worst ΔE 34.0, and 63.7 in Jerry
///   Dark itself. That is >14x the ~2.3 just-noticeable difference.
/// - **Any two ring colours**: worst ΔE 26.7.
/// - **Against plain text**, so a matched bracket never reads like an unmatched one (a real,
///   load-bearing distinction here - see [`BRACKET_1`]): worst ΔE 19.0.
/// - **Readable**: every colour clears 2.5:1 against [`super::surface::CENTER`] in every bundled
///   theme. A bracket is one thin glyph, so it is held to the floor
///   [`syntax_contrast_tests::every_syntax_token_clears_a_real_contrast_floor_in_jerry_dark_and_paper`]
///   only demands of Jerry Dark and `Paper`, not the looser 1.5:1 the other four get.
///
/// The narrow lightness band these sit in is load-bearing for one specific reason: `Paper` derives
/// from these defaults through [`derive_shift`]'s *inverting* lightness fit
/// (`l' = -1.286 l + 1.015`), so a source colour much lighter than ~0.68 lands near-black there. An
/// earlier draft had exactly that bug - a `#9b8cff` periwinkle deriving to `#020109`, ΔE 8.8 from
/// `Paper`'s own plain text, i.e. a "coloured" bracket indistinguishable from an uncoloured one.
///
/// **Not verified against a rendered window.** This environment cannot screenshot real GPUI
/// output, so every claim above is measured colour maths and hue-family reasoning, not something
/// anyone has looked at. The register mismatch that motivated this rewrite was caught by a
/// maintainer looking at the real thing, not by any of the numbers here.
///
/// The five generated theme files carry their own derived values for all six (see
/// [`crate::settings::builtin_themes`]), and an imported VSCode theme maps its own
/// `editorBracketHighlight.foreground1..6` family straight onto them - see
/// [`crate::settings::vscode_theme`]'s `COLOR_KEY_MAP`.
///
/// ## The default fallback chain (GitHub issue #31)
///
/// Several scopes here still have no independently *authored* colour of their own: their compiled
/// default is the same literal value as their nearest covered ancestor scope, so a scope this
/// app's palette never designed a hue for reads like its *parent* rather than like plain
/// foreground text:
///
/// - [`FUNCTION_METHOD`] defaults to [`FUNCTION`]'s `#74ade8` (a method is still a function)
/// - [`TYPE_BUILTIN`] to [`TYPE`]'s `#dfc184` (`i32`/`number`/`void` are still types)
/// - [`CONSTANT_BUILTIN`] to [`CONSTANT`]'s `#bf956a` (`true`/`None`/`undefined` are still
///   constants)
/// - [`TAG`] to [`TYPE`]'s (preserves this module's pre-existing, deliberate "a JSX element name
///   is coloured like the type it names" choice - see the historical note on
///   [`crate::code_surface::code_view::HighlightKind::Tag`])
/// - [`OPERATOR`], [`PUNCTUATION_BRACKET`], [`PUNCTUATION_DELIMITER`] and [`EMBEDDED`] to
///   [`TEXT`]'s, per the section above
///
/// Each of those is a real, live-classified bucket (a genuine `tree-sitter-highlight` capture this
/// module's `HIGHLIGHT_NAMES` actually recognizes - see
/// `code_view_tests::every_real_grammar_config_compiles` and its siblings), simply designed to
/// render identically to its parent rather than compete with it.
///
/// Before this module's rewrite each of those was a literal Rust-level `const` alias, so the two
/// could never be told apart by a theme at all. They are now independently keyed tokens that
/// merely *start* at the same value: a theme file (very much including an imported VSCode one) can
/// set one without touching the other. The chain above is still what an importer walks when a
/// theme names the parent scope but not the child - see
/// `crate::settings::vscode_theme::syntax_scope_rule`.
pub mod syntax {
    use super::{token, ColorToken};

    pub const TEXT: ColorToken = token("syntax.text", 0xacb2be);
    pub const KEYWORD: ColorToken = token("syntax.keyword", 0xb477cf);
    pub const FUNCTION: ColorToken = token("syntax.function", 0x74ade8);
    /// `function.method` (`tree-sitter-rust`'s `@function.method`, `-javascript`'s own) - see the
    /// module docs' fallback-chain section.
    pub const FUNCTION_METHOD: ColorToken = token("syntax.function_method", 0x74ade8);
    pub const TYPE: ColorToken = token("syntax.type", 0xdfc184);
    /// `type.builtin` (`tree-sitter-rust`'s `(primitive_type) @type.builtin`, `-typescript`'s
    /// `(predefined_type) @type.builtin`) - see the module docs' fallback-chain section.
    pub const TYPE_BUILTIN: ColorToken = token("syntax.type_builtin", 0xdfc184);
    /// `constant` (an all-caps identifier, per every one of this app's four grammars' own naming
    /// convention heuristic) - the same value [`LITERAL`] used to carry before this module split
    /// the old six-bucket "Literal" classification into its real, individually-scoped captures.
    pub const CONSTANT: ColorToken = token("syntax.constant", 0xbf956a);
    /// `constant.builtin` (`true`/`false`/`None`/`undefined`/an integer or float literal - Rust
    /// and JavaScript/TypeScript both route numeric/boolean literals through this real capture
    /// name rather than a plain `number`) - see the module docs' fallback-chain section.
    pub const CONSTANT_BUILTIN: ColorToken = token("syntax.constant_builtin", 0xbf956a);
    /// `string` (`(string_literal) @string`, `(template_string) @string`, ...) - a real, distinct
    /// hue from [`CONSTANT`] (unlike the replaced six-bucket palette, which lumped every literal
    /// together) so a string reads apart from a number at a glance.
    pub const STRING: ColorToken = token("syntax.string", 0x9dbb6f);
    /// `string.escape` - registered under both this checklist name and the real capture name every
    /// one of this app's grammars that supports escapes actually emits, plain `escape`
    /// (`tree-sitter-rust`'s `(escape_sequence) @escape`, `-python`'s own identical rule; neither
    /// JavaScript's nor TypeScript's own bundled query captures string escapes at all, verified
    /// directly against their real `queries/highlights.scm` - so this bucket is genuinely reachable
    /// for Rust/Python source only). A brighter tint of [`STRING`] rather than a plain alias: an
    /// escape sequence is a real, deliberately-distinct sub-token within a string, not a fallback
    /// case.
    pub const STRING_ESCAPE: ColorToken = token("syntax.string_escape", 0xc3d99a);
    /// `number` (`-python`'s `[(integer)(float)] @number`, `-javascript`'s `(number) @number`;
    /// Rust has no separate `number` capture at all - its own numeric literals arrive as
    /// `@constant.builtin` instead, see [`CONSTANT_BUILTIN`]). Defaults to [`CONSTANT`]'s value:
    /// both are numeric-literal buckets under a different grammar's own naming choice, and keeping
    /// them visually identical is what makes "a number looks like a number" consistent regardless
    /// of which of the four languages produced it.
    pub const NUMBER: ColorToken = token("syntax.number", 0xbf956a);
    pub const COMMENT: ColorToken = token("syntax.comment", 0x5d636f);
    /// `comment.doc` - registered under both this checklist name and the real capture name
    /// `tree-sitter-rust`'s own query actually emits, `comment.documentation`
    /// (`(line_comment (doc_comment)) @comment.documentation`); none of this app's other three
    /// grammars has a doc-comment concept in their bundled query. A brighter tint of [`COMMENT`]
    /// (not a plain alias) so a `///` doc comment reads as more prominent than an ordinary `//`
    /// one, the same real distinction most editors make.
    pub const COMMENT_DOC: ColorToken = token("syntax.comment_doc", 0x7c8290);
    /// `variable` - a real, live-classified bucket (`-python`'s own blanket `(identifier)
    /// @variable`, `-javascript`'s identical blanket rule). A muted dusty rose, and deliberately
    /// the quietest colour in this palette: a plain identifier is the most common token this
    /// module ever colours, so it has to read as clearly *not* plain text without shouting. See
    /// the module docs' "identifier family" section for how this hue was chosen and measured.
    ///
    /// This used to default to [`TEXT`]'s near-white `#acb2be`, which meant every identifier in a
    /// file rendered as plain grey - the single biggest reason code here read as undifferentiated
    /// white.
    pub const VARIABLE: ColorToken = token("syntax.variable", 0xbd89a5);
    /// `variable.parameter` (`tree-sitter-rust`'s `(parameter (identifier) @variable.parameter)`,
    /// `-typescript`'s `required_parameter`/`optional_parameter` rules) - [`VARIABLE`]'s own rose
    /// family, deeper and considerably more saturated. A function's inputs are worth picking out
    /// from the locals around them, and staying inside [`VARIABLE`]'s family says "still a
    /// variable, just a distinguished one" rather than inventing an unrelated hue for a closely
    /// related concept. Deeper rather than brighter for a real reason - see the module docs.
    pub const VARIABLE_PARAMETER: ColorToken = token("syntax.variable_parameter", 0xbd566e);
    /// `variable.builtin` (`self`/`this`/`super`/`cls`) - the bucket the replaced six-colour
    /// design table called "literal/self"; defaults to [`CONSTANT`]'s old `LITERAL` value so this
    /// one real, pre-existing visual choice (self-references read like literals here) survives the
    /// split unchanged.
    pub const VARIABLE_BUILTIN: ColorToken = token("syntax.variable_builtin", 0xbf956a);
    /// `property` (a field/attribute access - `tree-sitter-rust`'s `(field_identifier) @property`,
    /// `-python`'s `(attribute attribute: (identifier) @property)`, `-javascript`'s
    /// `(property_identifier) @property`) - a muted cyan-blue, deliberately outside [`VARIABLE`]'s
    /// warm family: a field access is not a local binding but a name looked up on another object,
    /// and the warm/cool split is what makes an `a.b.c` chain legible at a glance. See the module
    /// docs' "identifier family" section.
    pub const PROPERTY: ColorToken = token("syntax.property", 0x75b2c7);
    /// `operator` (`+`, `==`, `&&`, ...) - a real, live-classified bucket (previously fell
    /// through unmatched); defaults to [`TEXT`]'s value for the same reason [`VARIABLE`] does -
    /// this app's palette has never coloured punctuation/operators.
    pub const OPERATOR: ColorToken = token("syntax.operator", 0xacb2be);
    /// `punctuation.bracket` (`(`/`)`/`[`/`]`/`{`/`}`, and `<`/`>` in a generic-argument position)
    /// - see [`OPERATOR`]'s own docs for why this defaults to [`TEXT`]'s value.
    pub const PUNCTUATION_BRACKET: ColorToken = token("syntax.punctuation_bracket", 0xacb2be);
    /// `punctuation.delimiter` (`,`/`;`/`:`/`.`/`::`) - see [`OPERATOR`]'s own docs.
    pub const PUNCTUATION_DELIMITER: ColorToken = token("syntax.punctuation_delimiter", 0xacb2be);

    /// GitHub issue #168's rotating bracket-pair depth ring, colour 1 of 6 - the colour a real,
    /// *matched* `(`/`[`/`{` pair at nesting depth 0 (and 6, and 12, ...) paints, both halves of
    /// the pair alike. See this module's own "bracket-pair depth ring" section for how these six
    /// were chosen and measured, and
    /// [`crate::code_surface::code_view::colorize_bracket_pairs`] for the real matcher that
    /// decides which brackets reach these buckets at all (an unmatched one keeps
    /// [`PUNCTUATION_BRACKET`]'s plain-text colour, which is exactly why that token stays aliased
    /// to [`TEXT`]).
    pub const BRACKET_1: ColorToken = token("syntax.bracket_1", 0xeb7f7b);
    /// Bracket-pair depth ring, colour 2 of 6 (nesting depth 1, 7, ...) - see [`BRACKET_1`].
    pub const BRACKET_2: ColorToken = token("syntax.bracket_2", 0x7dd7b9);
    /// Bracket-pair depth ring, colour 3 of 6 (nesting depth 2, 8, ...) - see [`BRACKET_1`].
    pub const BRACKET_3: ColorToken = token("syntax.bracket_3", 0xc48648);
    /// Bracket-pair depth ring, colour 4 of 6 (nesting depth 3, 9, ...) - see [`BRACKET_1`].
    pub const BRACKET_4: ColorToken = token("syntax.bracket_4", 0x8f7cbd);
    /// Bracket-pair depth ring, colour 5 of 6 (nesting depth 4, 10, ...) - see [`BRACKET_1`].
    pub const BRACKET_5: ColorToken = token("syntax.bracket_5", 0x6c9052);
    /// Bracket-pair depth ring, colour 6 of 6 (nesting depth 5, 11, ...) - see [`BRACKET_1`].
    pub const BRACKET_6: ColorToken = token("syntax.bracket_6", 0x2d96c7);
    /// `tag` (a lowercase JSX element name, `-javascript`'s own JSX query) - see the module docs'
    /// fallback-chain section for why this defaults to [`TYPE`]'s value rather than its own hue: it
    /// preserves this module's pre-existing "a JSX element name is coloured like the type it
    /// names" choice unchanged, now through a real, dedicated schema slot instead of folding `tag`
    /// and `type` into one [`crate::code_surface::code_view::HighlightKind`] variant.
    pub const TAG: ColorToken = token("syntax.tag", 0xdfc184);
    /// `attribute` (Rust's `#[derive(...)]`/`#![...]`, `-javascript`'s JSX attribute name query) -
    /// a real, distinct hue (not a fallback) since a decorator/attribute is genuinely unlike
    /// anything else in the six-bucket original palette.
    pub const ATTRIBUTE: ColorToken = token("syntax.attribute", 0x7fb8b0);
    /// `embedded` (the interpolated-expression region of a template string/f-string, e.g.
    /// `` `n=${count}` ``'s `${count}` or an f-string's `{value}`) - defaults to [`TEXT`]'s
    /// value. The
    /// interpolated expression's own tokens (identifiers, calls, numbers, ...) already get their
    /// own, more specific captures that win over this one by nesting (see
    /// [`crate::code_surface::code_view`]'s own "`HighlightStart`s nest" docs), so this bucket is
    /// only ever visible for the rare leftover byte inside an interpolation no more specific
    /// capture covers - not worth a colour of its own.
    pub const EMBEDDED: ColorToken = token("syntax.embedded", 0xacb2be);

    /// GitHub issue #104's own real, prose-specific buckets - Markdown's `text.title`/
    /// `text.uri`/`text.reference`/`text.emphasis`/`text.strong` have no reasonable existing
    /// code-highlighting analog (unlike every other capture this app has ever wired, which is
    /// force-fittable onto an existing bucket - see this module's own fallback-chain docs above),
    /// so they get their own honestly-named [`crate::code_surface::code_view::HighlightKind`]
    /// variants and real, distinct hues rather than a confusing reuse of e.g. `KEYWORD` for a
    /// heading. Real, chosen values, not yet visually verified in a running window (this
    /// environment cannot screenshot GPUI output) - see that limitation noted in this repo's own
    /// session history.
    pub const HEADING: ColorToken = token("syntax.heading", 0xdfc184);
    /// `text.uri`/`text.reference` (a link's destination and its visible label/text) - reuses
    /// [`FUNCTION`]'s own blue as its default, the conventional "this is a link" hue in most
    /// editors/themes.
    pub const LINK: ColorToken = token("syntax.link", 0x74ade8);
    /// `text.strong` (`**bold**`) - a real, distinct hue since this app's rendering pipeline has
    /// no per-run font-weight support yet (`RenderedLine::runs` only carries `(SharedString,
    /// HighlightKind)` - no style/weight field), so a colour is the only real signal available
    /// for now; a brighter tint of [`TEXT`] rather than [`TEXT`] itself, so bold prose still reads
    /// as more prominent than plain text even without real bold rendering.
    pub const STRONG: ColorToken = token("syntax.strong", 0xd4dae4);
    /// `text.emphasis` (`*italic*`) - same real font-style limitation as [`STRONG`]; a soft
    /// lavender, distinct from [`super::syntax::KEYWORD`]'s stronger purple, so emphasis reads as
    /// a milder stylistic cue rather than a structural one.
    pub const EMPHASIS: ColorToken = token("syntax.emphasis", 0xc9a8d9);

    pub const CARET: ColorToken = token("syntax.caret", 0x5a9ad4);
    /// The code editor's real selection fill opacity (GitHub issue #27) while genuinely
    /// focused - applied on top of [`CARET`], the same color the solid caret itself paints, so
    /// selection and caret read as one consistent, theme-aware "insertion cursor" family rather
    /// than two independently-chosen colors.
    pub const SELECTION_OPACITY: f32 = 0.28;
    /// The same selection fill, dimmed further while the editor is unfocused (issue #27:
    /// "selection remains visible (dimmed) when the editor loses focus") - still genuinely
    /// visible, just clearly de-emphasized relative to the focused case above.
    pub const SELECTION_UNFOCUSED_OPACITY: f32 = 0.14;
    pub const ERROR_UNDERLINE: ColorToken = token("syntax.error_underline", 0xe0625c); // 2px dotted
    pub const HOVER_UNDERLINE: ColorToken = token("syntax.hover_underline", 0x4d7ba8); // 1px solid

    /// The File view's Diagnostic-state row tint (`README.md`: "row tinted `#191416`") -
    /// distinct from [`super::surface::CURRENT_LINE`].
    pub const DIAGNOSTIC_ROW_BG: ColorToken = token("syntax.diagnostic_row_bg", 0x191416);
    /// The Diagnostic state's dim, end-of-line inline message text (`README.md`: `#6b4a48`).
    pub const DIAGNOSTIC_INLINE_MESSAGE: ColorToken =
        token("syntax.diagnostic_inline_message", 0x6b4a48);
    /// The Diagnostic state's card message text (`README.md`: `#e3908b`). Same hex as
    /// [`super::button::DANGER_FG_HOVER`], kept as its own token - unrelated elements that
    /// happen to share a designed red.
    pub const DIAGNOSTIC_CARD_MESSAGE: ColorToken =
        token("syntax.diagnostic_card_message", 0xe3908b);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("TEXT", TEXT),
        ("KEYWORD", KEYWORD),
        ("FUNCTION", FUNCTION),
        ("FUNCTION_METHOD", FUNCTION_METHOD),
        ("TYPE", TYPE),
        ("TYPE_BUILTIN", TYPE_BUILTIN),
        ("CONSTANT", CONSTANT),
        ("CONSTANT_BUILTIN", CONSTANT_BUILTIN),
        ("STRING", STRING),
        ("STRING_ESCAPE", STRING_ESCAPE),
        ("NUMBER", NUMBER),
        ("COMMENT", COMMENT),
        ("COMMENT_DOC", COMMENT_DOC),
        ("VARIABLE", VARIABLE),
        ("VARIABLE_PARAMETER", VARIABLE_PARAMETER),
        ("VARIABLE_BUILTIN", VARIABLE_BUILTIN),
        ("PROPERTY", PROPERTY),
        ("OPERATOR", OPERATOR),
        ("PUNCTUATION_BRACKET", PUNCTUATION_BRACKET),
        ("PUNCTUATION_DELIMITER", PUNCTUATION_DELIMITER),
        ("BRACKET_1", BRACKET_1),
        ("BRACKET_2", BRACKET_2),
        ("BRACKET_3", BRACKET_3),
        ("BRACKET_4", BRACKET_4),
        ("BRACKET_5", BRACKET_5),
        ("BRACKET_6", BRACKET_6),
        ("TAG", TAG),
        ("ATTRIBUTE", ATTRIBUTE),
        ("EMBEDDED", EMBEDDED),
        ("HEADING", HEADING),
        ("LINK", LINK),
        ("STRONG", STRONG),
        ("EMPHASIS", EMPHASIS),
        ("CARET", CARET),
        ("ERROR_UNDERLINE", ERROR_UNDERLINE),
        ("HOVER_UNDERLINE", HOVER_UNDERLINE),
        ("DIAGNOSTIC_ROW_BG", DIAGNOSTIC_ROW_BG),
        ("DIAGNOSTIC_INLINE_MESSAGE", DIAGNOSTIC_INLINE_MESSAGE),
        ("DIAGNOSTIC_CARD_MESSAGE", DIAGNOSTIC_CARD_MESSAGE),
    ];
}

/// The File view's structural chrome (GitHub issue #31's "editor chrome" checklist item) -
/// selection, the current-line highlight, the caret, and a handful of tokens for editor features
/// that are real schema slots but have **no real renderer yet** in this codebase (matching-bracket
/// highlighting, indent guides inside the code surface itself - distinct from
/// [`tree`]'s file-*tree* indent guides, which are real and already painted -, whitespace marks, a
/// minimap, blame text, and a removed-line gutter marker). Each such token's own doc comment says
/// so explicitly; none is wired into a fabricated render call - see this crate's own
/// `CONTRIBUTING.md` "no fake functionality" rule.
///
/// Most tokens here *default to* the same literal value an existing token elsewhere in this module
/// carries (the same "start from what's already designed" idiom [`syntax`]'s own fallback chain
/// uses) rather than being independently-authored hex literals - consolidated here, under one
/// discoverable name, even where the underlying value already existed under another module's name.
/// Each is nonetheless its own independently-keyed token: before this module's rewrite they were
/// literal Rust-level `const` aliases, so e.g. a theme could not give the code surface's caret a
/// different colour from `syntax::CARET`; now it can.
pub mod editor {
    use super::{token, ColorToken};

    /// The active text selection fill's base colour - the exact value already painted by the real
    /// selection quad in `crate::code_surface::editing::render_editable_file_view_line` (defaults
    /// to [`super::syntax::CARET`]'s value, matching that call site's own pre-existing choice to
    /// paint the selection in the caret's own hue at reduced opacity).
    pub const SELECTION: ColorToken = token("editor.selection", 0x5a9ad4);
    /// [`SELECTION`]'s real render opacity - the exact literal already passed to `Hsla::opacity`
    /// at that same real call site.
    pub const SELECTION_OPACITY: f32 = 0.28;
    /// A dimmer selection fill for an unfocused/inactive editor pane. **Not yet painted by any
    /// real renderer** - this app's File view has no "inactive pane" focus concept today (a
    /// selection currently renders identically regardless of window/pane focus). Added now so
    /// that real feature, if built, has a real token to plug into rather than inventing one then.
    pub const SELECTION_INACTIVE: ColorToken = token("editor.selection_inactive", 0x5a9ad4);
    /// [`SELECTION_INACTIVE`]'s intended opacity, dimmer than [`SELECTION_OPACITY`] - unused for
    /// the same reason [`SELECTION_INACTIVE`] is.
    pub const SELECTION_INACTIVE_OPACITY: f32 = 0.14;

    /// The current-line highlight - defaults to [`super::surface::CURRENT_LINE`]'s value, the
    /// real, already-painted token (`crate::code_surface::editing`/`crate::code_surface::file_view`'s
    /// own `.bg(theme::surface::CURRENT_LINE)` on the cursor's row).
    pub const CURRENT_LINE: ColorToken = token("editor.current_line", 0x181c20);
    /// The caret bar - defaults to [`super::syntax::CARET`]'s value, the real, already-painted
    /// token.
    pub const CARET: ColorToken = token("editor.caret", 0x5a9ad4);

    /// A matched/matching bracket pair's highlight fill. **Not yet painted by any real renderer**
    /// - bracket-matching isn't implemented in the File view yet.
    pub const MATCHING_BRACKET: ColorToken = token("editor.matching_bracket", 0x2c4a63);

    /// A resting indent guide inside the code surface (GitHub issue #122: "Add settings to
    /// display indents in code editor") - distinct from [`tree::INDENT_GUIDE`], the file-*tree*
    /// sidebar's own real, already-painted indent guide. Now really painted too:
    /// `crate::code_surface::editing::render_editable_file_view_line` draws one guide per real
    /// indent level, gated by `crate::settings::store::AppearanceSettings::show_indent_guides`.
    /// Defaults to [`super::border::DIVIDER`]'s value, matching [`tree::INDENT_GUIDE`]'s own
    /// choice, so the two read as the same visual language.
    pub const INDENT_GUIDE: ColorToken = token("editor.indent_guide", 0x22262a);
    /// The indent guide for the level the caret currently sits in. **Not yet painted by any real
    /// renderer** - GitHub issue #122's own real indent guides (above) don't distinguish an
    /// "active" level, since that would need real scope/bracket-matching data this codebase
    /// doesn't have yet (see [`MATCHING_BRACKET`]'s own docs). Defaults to
    /// [`super::border::SELECTED_EDGE`]'s value, matching [`tree::INDENT_GUIDE_ACTIVE`], so a real
    /// active-level highlight has a token to plug into if bracket-matching is ever built.
    pub const INDENT_GUIDE_ACTIVE: ColorToken = token("editor.indent_guide_active", 0x3f5b74);

    /// A rendered whitespace mark (a middle-dot for a space, an arrow for a tab). **Not yet
    /// painted by any real renderer.**
    pub const WHITESPACE: ColorToken = token("editor.whitespace", 0x41464b);

    /// A minimap's own background fill. **Not yet painted by any real renderer** - there is no
    /// minimap in this codebase yet.
    pub const MINIMAP_BG: ColorToken = token("editor.minimap_bg", 0x131518);

    /// The line-number gutter's text colour - defaults to [`super::text::GUTTER`]'s value, the
    /// real, already-painted token for every non-current row.
    pub const GUTTER_TEXT: ColorToken = token("editor.gutter_text", 0x3a3f44);
    /// The current row's own brighter gutter-number colour - defaults to [`super::text::DIM`]'s
    /// value, the real, already-painted token.
    pub const GUTTER_TEXT_ACTIVE: ColorToken = token("editor.gutter_text_active", 0x8b9197);
    /// The gutter column's own background fill. **Not yet painted by any real renderer** - the
    /// gutter today has no fill of its own; it simply shows through whatever its row already
    /// painted ([`CURRENT_LINE`] on the cursor's row, otherwise transparent). Added for schema
    /// completeness should a visually-distinct gutter background ever be designed.
    pub const GUTTER_BG: ColorToken = token("editor.gutter_bg", 0x131518);

    /// Inline git-blame annotation text. **Not yet painted by any real renderer** - there is no
    /// blame feature in this codebase yet. Defaults to [`super::text::FAINT`]'s value, this
    /// palette's own existing "quiet annotation" tone.
    pub const BLAME_TEXT: ColorToken = token("editor.blame_text", 0x6b7178);

    /// An added line's gutter marker - defaults to [`super::diff::GIT_GUTTER`]'s value, the real,
    /// already-painted 3px marker `crate::code_surface::editing`/`::file_view` paint for a line
    /// [`crate::code_surface::code_view::changed_line_set`] reports as agent-touched.
    pub const DIFF_ADDED: ColorToken = token("editor.diff_added", 0x2c6244);
    /// A removed line's own gutter marker. **Not yet painted by any real renderer** -
    /// [`crate::code_surface::code_view::changed_line_set`]'s own docs record that removed lines
    /// "don't exist in the new file, so they never advance [the new-file line counter]": today's
    /// File view gutter has no way to represent "a line was deleted here" at all, only "this line
    /// was added/changed". Defaults to [`super::diff::DEL_SIGN`]'s value so a future real marker
    /// would read as the same red the standalone Diff view already uses for a removal.
    pub const DIFF_REMOVED: ColorToken = token("editor.diff_removed", 0xa35f5b);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("SELECTION", SELECTION),
        ("SELECTION_INACTIVE", SELECTION_INACTIVE),
        ("CURRENT_LINE", CURRENT_LINE),
        ("CARET", CARET),
        ("MATCHING_BRACKET", MATCHING_BRACKET),
        ("INDENT_GUIDE", INDENT_GUIDE),
        ("INDENT_GUIDE_ACTIVE", INDENT_GUIDE_ACTIVE),
        ("WHITESPACE", WHITESPACE),
        ("MINIMAP_BG", MINIMAP_BG),
        ("GUTTER_TEXT", GUTTER_TEXT),
        ("GUTTER_TEXT_ACTIVE", GUTTER_TEXT_ACTIVE),
        ("GUTTER_BG", GUTTER_BG),
        ("BLAME_TEXT", BLAME_TEXT),
        ("DIFF_ADDED", DIFF_ADDED),
        ("DIFF_REMOVED", DIFF_REMOVED),
    ];
}

/// The terminal and agent CLI surface - prompt, output, its own status tones, and clickable
/// path links.
pub mod term {
    use super::{token, ColorToken};

    pub const PROMPT: ColorToken = token("term.prompt", 0x8fbde6);
    pub const TEXT: ColorToken = token("term.text", 0xa7adb4);
    pub const DIM: ColorToken = token("term.dim", 0x6b7178);
    pub const OK: ColorToken = token("term.ok", 0x6ab97f);
    pub const ERR: ColorToken = token("term.err", 0xe0625c);
    pub const WARN: ColorToken = token("term.warn", 0xd8a94a);
    pub const HEADING: ColorToken = token("term.heading", 0xced4da);
    pub const ACTIVITY: ColorToken = token("term.activity", 0x5a9ad4); // spinner / progress line
    pub const MENU_SEL_FG: ColorToken = token("term.menu_sel_fg", 0xe0b263);
    pub const MENU_SEL_BG: ColorToken = token("term.menu_sel_bg", 0x1f1a10);
    pub const CURSOR: ColorToken = token("term.cursor", 0x5a9ad4);
    /// A clickable path/`path:line` link inside terminal output (`Jerry.dc.html`:
    /// `color:#7fb4e3;border-bottom:1px dotted #3d6a91`).
    pub const LINK: ColorToken = token("term.link", 0x7fb4e3);
    pub const LINK_UNDERLINE: ColorToken = token("term.link_underline", 0x3d6a91);
    /// The link's hover state (`Jerry.dc.html`: `style-hover="color:#a5cdf0;border-bottom:1px
    /// solid #78a8d0"`). Same value as [`super::button::BLUE_FG`], kept as its own token for a
    /// distinct element.
    pub const LINK_HOVER: ColorToken = token("term.link_hover", 0xa5cdf0);
    pub const LINK_UNDERLINE_HOVER: ColorToken = token("term.link_underline_hover", 0x78a8d0);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("PROMPT", PROMPT),
        ("TEXT", TEXT),
        ("DIM", DIM),
        ("OK", OK),
        ("ERR", ERR),
        ("WARN", WARN),
        ("HEADING", HEADING),
        ("ACTIVITY", ACTIVITY),
        ("MENU_SEL_FG", MENU_SEL_FG),
        ("MENU_SEL_BG", MENU_SEL_BG),
        ("CURSOR", CURSOR),
        ("LINK", LINK),
        ("LINK_UNDERLINE", LINK_UNDERLINE),
        ("LINK_HOVER", LINK_HOVER),
        ("LINK_UNDERLINE_HOVER", LINK_UNDERLINE_HOVER),
    ];
}

/// The environment (WSL) chip's tokens - shown in the terminal footer, the status bar, and
/// Settings' `Default environment` row.
pub mod env {
    use super::{token, ColorToken};

    /// Defaults to [`super::term::PROMPT`]'s own value (`Jerry.dc.html`'s `footRemoteFg` for
    /// `plat === 'windows'`), independently themeable from it.
    pub const WSL_FG: ColorToken = token("env.wsl_fg", 0x8fbde6);
    pub const WSL_BG: ColorToken = token("env.wsl_bg", 0x16222c);
    pub const WSL_BORDER: ColorToken = token("env.wsl_border", 0x24384a);
    /// Defaults to [`super::text::FAINT`]'s own value.
    pub const LOCAL_FG: ColorToken = token("env.local_fg", 0x6b7178);
    /// Defaults to [`super::border::DIVIDER`]'s own value.
    pub const LOCAL_BORDER: ColorToken = token("env.local_border", 0x22262a);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("WSL_FG", WSL_FG),
        ("WSL_BG", WSL_BG),
        ("WSL_BORDER", WSL_BORDER),
        ("LOCAL_FG", LOCAL_FG),
        ("LOCAL_BORDER", LOCAL_BORDER),
    ];
}

/// One tint per agent. Used on the rail badge, the CLI tab chip and the
/// conflict side headers, so a colour always means the same agent.
pub mod agent {
    use super::{token, ColorToken};

    pub const SONNET: (ColorToken, ColorToken) = (
        token("agent.sonnet.fg", 0xd8a94a),
        token("agent.sonnet.bg", 0x33280f),
    ); // (fg, bg)
    pub const CODEX: (ColorToken, ColorToken) = (
        token("agent.codex.fg", 0x6ab97f),
        token("agent.codex.bg", 0x1e3327),
    );
    pub const HAIKU: (ColorToken, ColorToken) = (
        token("agent.haiku.fg", 0xc98fbf),
        token("agent.haiku.bg", 0x332030),
    );
    pub const LOCAL: (ColorToken, ColorToken) = (
        token("agent.local.fg", 0x7f9ad4),
        token("agent.local.bg", 0x1f2941),
    );

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("SONNET.fg", SONNET.0),
        ("SONNET.bg", SONNET.1),
        ("CODEX.fg", CODEX.0),
        ("CODEX.bg", CODEX.1),
        ("HAIKU.fg", HAIKU.0),
        ("HAIKU.bg", HAIKU.1),
        ("LOCAL.fg", LOCAL.0),
        ("LOCAL.bg", LOCAL.1),
    ];
}

/// Language chips, shared by the file tree, the code tab and the palette.
pub mod lang {
    use super::{token, ColorToken};

    pub const RS: (ColorToken, ColorToken) =
        (token("lang.rs.fg", 0xc0824a), token("lang.rs.bg", 0x2e2113)); // "rs"
    pub const TOML: (ColorToken, ColorToken) = (
        token("lang.toml.fg", 0x8b9197),
        token("lang.toml.bg", 0x23272b),
    ); // "to"
    pub const MD: (ColorToken, ColorToken) =
        (token("lang.md.fg", 0x7f9ad4), token("lang.md.bg", 0x1d2532)); // "md"
                                                                        // Verified directly against `design_handoff_jerry_ade/revision/tokens.rs:149-160`'s real
                                                                        // hex values, not paraphrased.
    pub const SQL: (ColorToken, ColorToken) = (
        token("lang.sql.fg", 0x6ab97f),
        token("lang.sql.bg", 0x1b2a20),
    ); // "sq"
    pub const TS: (ColorToken, ColorToken) =
        (token("lang.ts.fg", 0x6b9bd1), token("lang.ts.bg", 0x1b2838)); // "ts"
    pub const VUE: (ColorToken, ColorToken) = (
        token("lang.vue.fg", 0x5cb87f),
        token("lang.vue.bg", 0x16261e),
    ); // "vue"
    pub const PY: (ColorToken, ColorToken) =
        (token("lang.py.fg", 0xc9b04a), token("lang.py.bg", 0x2a2612)); // "py"
    pub const GO: (ColorToken, ColorToken) =
        (token("lang.go.fg", 0x5fa8c4), token("lang.go.bg", 0x152730)); // "go"
                                                                        // GitHub issue #32 - three new hues, each picked to stay visually distinct from every
                                                                        // existing chip above rather than reusing a near-identical tint of an unrelated language.
    pub const JSON: (ColorToken, ColorToken) = (
        token("lang.json.fg", 0xb8bcc4),
        token("lang.json.bg", 0x24262b),
    ); // "jsn"
    pub const YAML: (ColorToken, ColorToken) = (
        token("lang.yaml.fg", 0x8aa8cf),
        token("lang.yaml.bg", 0x1c2530),
    ); // "yml"
    pub const C: (ColorToken, ColorToken) =
        (token("lang.c.fg", 0x9a8cc9), token("lang.c.bg", 0x231f30)); // "c"
                                                                      // GitHub issue #154 - two more hues, chosen the same way issue #32's three above were: each
                                                                      // stays visually distinct from *every* existing chip rather than reusing a near-identical
                                                                      // tint. Both hues here were genuinely unoccupied before this issue - the existing set spans
                                                                      // orange-brown (RS), yellow (PY), greens (SQL/VUE), blues (MD/TS/YAML), cyan (GO), purple
                                                                      // (C) and two greys (TOML/JSON), leaving red and magenta free. Enforced, not just asserted in
                                                                      // prose, by `lang_token_tests::every_lang_chip_color_is_distinct_from_every_other`.
    pub const HTML: (ColorToken, ColorToken) = (
        token("lang.html.fg", 0xd1735f),
        token("lang.html.bg", 0x2f1d18),
    ); // "htm"
       // Magenta, deliberately not another purple: `C`'s `#9a8cc9` is a blue-leaning violet, this
       // is red-leaning, so the two do not read as the same chip at chip size.
    pub const CSS: (ColorToken, ColorToken) = (
        token("lang.css.fg", 0xc47fb0),
        token("lang.css.bg", 0x2c1e29),
    ); // "css"
    pub const UNKNOWN: (ColorToken, ColorToken) = (
        token("lang.unknown.fg", 0x6b7178),
        token("lang.unknown.bg", 0x23272b),
    );
    // "."

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("RS.fg", RS.0),
        ("RS.bg", RS.1),
        ("TOML.fg", TOML.0),
        ("TOML.bg", TOML.1),
        ("MD.fg", MD.0),
        ("MD.bg", MD.1),
        ("SQL.fg", SQL.0),
        ("SQL.bg", SQL.1),
        ("TS.fg", TS.0),
        ("TS.bg", TS.1),
        ("VUE.fg", VUE.0),
        ("VUE.bg", VUE.1),
        ("PY.fg", PY.0),
        ("PY.bg", PY.1),
        ("GO.fg", GO.0),
        ("GO.bg", GO.1),
        ("JSON.fg", JSON.0),
        ("JSON.bg", JSON.1),
        ("YAML.fg", YAML.0),
        ("YAML.bg", YAML.1),
        ("C.fg", C.0),
        ("C.bg", C.1),
        ("HTML.fg", HTML.0),
        ("HTML.bg", HTML.1),
        ("CSS.fg", CSS.0),
        ("CSS.bg", CSS.1),
        ("UNKNOWN.fg", UNKNOWN.0),
        ("UNKNOWN.bg", UNKNOWN.1),
    ];
}

/// Buttons, by role: green (primary/confirm), blue (secondary), amber (attention) and the
/// danger red.
pub mod button {
    use super::{token, ColorToken};

    pub const GREEN_BG: ColorToken = token("button.green_bg", 0x24503a);
    pub const GREEN_BG_HOVER: ColorToken = token("button.green_bg_hover", 0x2c6045);
    pub const GREEN_FG: ColorToken = token("button.green_fg", 0x9fdcb6);
    pub const GREEN_KEYCAP: ColorToken = token("button.green_keycap", 0x376b4d);
    /// The keycap glyph colour inside a green primary button (`README.md`/`Jerry.dc.html`:
    /// `#8ac9a4`) - not in `tokens.rs`'s `button` module (only [`GREEN_KEYCAP`], the border, is
    /// transcribed there), added here directly.
    pub const GREEN_KEYCAP_FG: ColorToken = token("button.green_keycap_fg", 0x8ac9a4);
    // The equivalent blue keycap glyph colour (`#8fbde6`) needs no separate constant here -
    // it's the exact same value already ported as `term::PROMPT`.
    pub const BLUE_BG: ColorToken = token("button.blue_bg", 0x243c50);
    pub const BLUE_BG_HOVER: ColorToken = token("button.blue_bg_hover", 0x2c4a63);
    pub const BLUE_FG: ColorToken = token("button.blue_fg", 0xa5cdf0);
    pub const BLUE_KEYCAP: ColorToken = token("button.blue_keycap", 0x365b78);
    pub const AMBER_BG: ColorToken = token("button.amber_bg", 0x3a2c14);
    pub const AMBER_BG_HOVER: ColorToken = token("button.amber_bg_hover", 0x4a3818);
    pub const AMBER_FG: ColorToken = token("button.amber_fg", 0xe0b263);
    pub const DANGER_FG: ColorToken = token("button.danger_fg", 0xc4726d);
    pub const DANGER_FG_HOVER: ColorToken = token("button.danger_fg_hover", 0xe3908b);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("GREEN_BG", GREEN_BG),
        ("GREEN_BG_HOVER", GREEN_BG_HOVER),
        ("GREEN_FG", GREEN_FG),
        ("GREEN_KEYCAP", GREEN_KEYCAP),
        ("GREEN_KEYCAP_FG", GREEN_KEYCAP_FG),
        ("BLUE_BG", BLUE_BG),
        ("BLUE_BG_HOVER", BLUE_BG_HOVER),
        ("BLUE_FG", BLUE_FG),
        ("BLUE_KEYCAP", BLUE_KEYCAP),
        ("AMBER_BG", AMBER_BG),
        ("AMBER_BG_HOVER", AMBER_BG_HOVER),
        ("AMBER_FG", AMBER_FG),
        ("DANGER_FG", DANGER_FG),
        ("DANGER_FG_HOVER", DANGER_FG_HOVER),
    ];
}

/// Toggle switches and the Changes-panel staging checkbox.
pub mod toggle {
    use super::{token, ColorToken};

    pub const TRACK_ON: ColorToken = token("toggle.track_on", 0x2f6d4b);
    pub const TRACK_OFF: ColorToken = token("toggle.track_off", 0x23272b);
    pub const KNOB_ON: ColorToken = token("toggle.knob_on", 0xc8ecd6);
    pub const KNOB_OFF: ColorToken = token("toggle.knob_off", 0x6b7178);
    /// The Changes row staging checkbox's hover border (Revision R12 §5) - not in
    /// `tokens.rs`'s transcribed set (that checkbox previously had no hover treatment at all),
    /// added here directly.
    pub const CHECKBOX_HOVER: ColorToken = token("toggle.checkbox_hover", 0x3f7a55);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("TRACK_ON", TRACK_ON),
        ("TRACK_OFF", TRACK_OFF),
        ("KNOB_ON", KNOB_ON),
        ("KNOB_OFF", KNOB_OFF),
        ("CHECKBOX_HOVER", CHECKBOX_HOVER),
    ];
}

/// Small status tags and the file tree's own A/M change marks.
pub mod tag {
    use super::{token, ColorToken};

    pub const NEW: (ColorToken, ColorToken) =
        (token("tag.new.fg", 0x7fc79a), token("tag.new.bg", 0x1e3b2a));
    pub const DELETED: (ColorToken, ColorToken) = (
        token("tag.deleted.fg", 0xd18b86),
        token("tag.deleted.bg", 0x3a1e1e),
    );
    pub const CONFLICT: (ColorToken, ColorToken) = (
        token("tag.conflict.fg", 0xe0b263),
        token("tag.conflict.bg", 0x3a2c14),
    );
    pub const TREE_ADDED: ColorToken = token("tag.tree_added", 0x5f9c78); // "A" mark
    pub const TREE_MODIFIED: ColorToken = token("tag.tree_modified", 0xa3873f); // "M" mark

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("NEW.fg", NEW.0),
        ("NEW.bg", NEW.1),
        ("DELETED.fg", DELETED.0),
        ("DELETED.bg", DELETED.1),
        ("CONFLICT.fg", CONFLICT.0),
        ("CONFLICT.bg", CONFLICT.1),
        ("TREE_ADDED", TREE_ADDED),
        ("TREE_MODIFIED", TREE_MODIFIED),
    ];
}

/// Exact colours for Surface C's real Completions popup item rows - read directly from
/// `design_handoff_jerry_ade/revision/Jerry.dc.html`'s own `completions`/`KBG`/`KFG` data (the
/// `sel ? ... : ...` ternaries around line 2289, and the `KBG`/`KFG` maps around line 1792) - not
/// reused from any nearby-but-not-identical existing token (e.g. [`super::text::SELECTED`]
/// (`#dde2e7`) is a real, different colour from this module's own [`ITEM_SELECTED_FG`]
/// (`#e3e8ed`), and [`super::surface::CURRENT_LINE`] (`#181c20`) - the File view's current-line
/// tint - is the exact same hex as [`super::surface::POPOVER`] itself, which is why reusing it as
/// the selected-row highlight here used to paint an invisible selection).
pub mod completions_popup {
    use super::{token, ColorToken};

    /// A selected completion row's real background (`Jerry.dc.html`: `c.sel ? '#243c50' : ...`).
    pub const ITEM_SELECTED_BG: ColorToken = token("completions_popup.item_selected_bg", 0x243c50);
    /// A selected completion row's real label colour (`Jerry.dc.html`: `c.sel ? '#e3e8ed' : ...`).
    pub const ITEM_SELECTED_FG: ColorToken = token("completions_popup.item_selected_fg", 0xe3e8ed);
    /// An unselected completion row's real label colour (`Jerry.dc.html`: `... : '#b8bfc6'`) -
    /// the exact same hex as [`super::text::BODY`], carried here as its own token.
    pub const ITEM_FG: ColorToken = token("completions_popup.item_fg", 0xb8bfc6);

    /// `(fg, bg)` for a `function`/`method`/`constructor`-shaped completion item's kind badge
    /// (`Jerry.dc.html`'s `KFG.f`/`KBG.f`).
    pub const KIND_FUNCTION: (ColorToken, ColorToken) = (
        token("completions_popup.kind_function.fg", 0x8fbde6),
        token("completions_popup.kind_function.bg", 0x243c50),
    );
    /// `(fg, bg)` for a `variable`/`field`/`property`/`constant`-shaped completion item's kind
    /// badge (`Jerry.dc.html`'s `KFG.v`/`KBG.v`).
    pub const KIND_VARIABLE: (ColorToken, ColorToken) = (
        token("completions_popup.kind_variable.fg", 0xd8a94a),
        token("completions_popup.kind_variable.bg", 0x33280f),
    );
    /// `(fg, bg)` for a `class`/`struct`/`interface`/`enum`/`type`-shaped completion item's kind
    /// badge (`Jerry.dc.html`'s `KFG.t`/`KBG.t`).
    pub const KIND_TYPE: (ColorToken, ColorToken) = (
        token("completions_popup.kind_type.fg", 0xc294e0),
        token("completions_popup.kind_type.bg", 0x33203e),
    );

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("ITEM_SELECTED_BG", ITEM_SELECTED_BG),
        ("ITEM_SELECTED_FG", ITEM_SELECTED_FG),
        ("ITEM_FG", ITEM_FG),
        ("KIND_FUNCTION.fg", KIND_FUNCTION.0),
        ("KIND_FUNCTION.bg", KIND_FUNCTION.1),
        ("KIND_VARIABLE.fg", KIND_VARIABLE.0),
        ("KIND_VARIABLE.bg", KIND_VARIABLE.1),
        ("KIND_TYPE.fg", KIND_TYPE.0),
        ("KIND_TYPE.bg", KIND_TYPE.1),
    ];
}

/// Settings-surface-only colours read directly from `Jerry.dc.html`'s inline literals for the
/// `settingsOpen` block - real values present in the mockup but missing from `tokens.rs`'s
/// transcription (predates the Settings section). Every other Settings colour reuses an
/// existing token from another module - see `crate::root`'s Settings render methods.
pub mod settings {
    use super::{token, ColorToken};

    /// A nav row's hover background (`Jerry.dc.html`: `style-hover="background:#17191b"`) -
    /// distinct from [`super::surface::ROW_HOVER`] (`#15181b`).
    pub const NAV_ROW_HOVER: ColorToken = token("settings.nav_row_hover", 0x17191b);
    /// The content column's page-subtitle text (`Jerry.dc.html`: `color:#767d84`) - close to
    /// but distinct from [`super::text::DIM`] (`#8b9197`).
    pub const SUBTITLE: ColorToken = token("settings.subtitle", 0x767d84);
    /// A card row's own bottom separator (`Jerry.dc.html`: `border-bottom:1px solid #1f2327`) -
    /// distinct from [`super::border::CARD_FIELD`] (`#22272b`).
    pub const CARD_ROW_SEP: ColorToken = token("settings.card_row_sep", 0x1f2327);
    /// A binary-found status dot on the Agents page. Same hex as [`super::status::REVIEW`],
    /// kept as its own token: the agent `Status` palette is reserved for agent urgency
    /// (`README.md`'s "Status vocabulary — use nowhere else"), and "this binary resolved on
    /// `$PATH`" is a different fact that just happens to want the same green.
    pub const AGENT_READY: ColorToken = token("settings.agent_ready", 0x5cb87f);
    /// A binary-not-found status dot on the Agents page - same reasoning as [`AGENT_READY`],
    /// same hex as [`super::status::FAIL`].
    pub const AGENT_NOT_FOUND: ColorToken = token("settings.agent_not_found", 0xe0625c);
    /// The Worktrees page's "merged and prunable" row dot - distinct from
    /// [`super::status::IDLE`] (`#565d64`, used for the main checkout's own dot).
    pub const WORKTREE_PRUNABLE_DOT: ColorToken = token("settings.worktree_prunable_dot", 0x3f454b);
    /// A selected Appearance-preview-card's / Theme-card's background - see
    /// [`CARD_UNSELECTED_BG`] for the unselected counterpart.
    pub const CARD_SELECTED_BG: ColorToken = token("settings.card_selected_bg", 0x161b1f);
    pub const CARD_UNSELECTED_BG: ColorToken = token("settings.card_unselected_bg", 0x131619);
    /// A Theme card's hover border (`Jerry.dc.html`: `style-hover="border-color:#3a4148"`).
    pub const THEME_CARD_HOVER_BORDER: ColorToken =
        token("settings.theme_card_hover_border", 0x3a4148);
    /// The config snippet block's section-header line colour (`Jerry.dc.html`'s `CSFG.s`:
    /// `#c294e0`).
    pub const SNIPPET_SECTION: ColorToken = token("settings.snippet_section", 0xc294e0);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("NAV_ROW_HOVER", NAV_ROW_HOVER),
        ("SUBTITLE", SUBTITLE),
        ("CARD_ROW_SEP", CARD_ROW_SEP),
        ("AGENT_READY", AGENT_READY),
        ("AGENT_NOT_FOUND", AGENT_NOT_FOUND),
        ("WORKTREE_PRUNABLE_DOT", WORKTREE_PRUNABLE_DOT),
        ("CARD_SELECTED_BG", CARD_SELECTED_BG),
        ("CARD_UNSELECTED_BG", CARD_UNSELECTED_BG),
        ("THEME_CARD_HOVER_BORDER", THEME_CARD_HOVER_BORDER),
        ("SNIPPET_SECTION", SNIPPET_SECTION),
    ];
}

/// The overlay scrollbar's own colours (GitHub issue #30) - not from `design_handoff_jerry_ade`
/// (that mockup has no scrollbar spec at all: every scrollable region there relies on raw,
/// invisible browser/OS scrolling), so these are a deliberate, judgment-call derivation from
/// existing neutral tokens rather than a transcription. `THUMB` defaults to [`text::GUTTER`]'s
/// value (the line-number gutter's own muted grey - already the UI's "quiet structural chrome"
/// colour) and `THUMB_HOVER` to [`status::IDLE`]'s (an agent's resting-state grey, one step
/// brighter) so the two states read as "the same neutral family, one step apart" rather than
/// inventing a third hex pair. Both are painted at reduced opacity (see `crate::root::scrollbar`) rather than full
/// strength, matching the "overlay, not a solid rail" requirement.
pub mod scrollbar {
    use super::{token, ColorToken};

    pub const THUMB: ColorToken = token("scrollbar.thumb", 0x3a3f44);
    pub const THUMB_HOVER: ColorToken = token("scrollbar.thumb_hover", 0x565d64);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[("THUMB", THUMB), ("THUMB_HOVER", THUMB_HOVER)];
}

/// The git graph tab (design handoff `design_handoff_jerry_ade/revision 2/CHANGELOG.md`,
/// 2026-07-31 entry, "git graph (issue #1)") - real hex values transcribed directly from that
/// entry's §2/§3, not paraphrased. The column header band and the removal of the per-commit
/// session column (`HEADER`/`HEADER_BG`/`HEADER_LABEL_FG` below) are `revision 3/
/// REVISION-2026-07-31.md` §6.1/§6.2 instead - that revision supersedes the revision-2 entry
/// for those two points only, everything else here is still the revision-2 values.
pub mod graph {
    use super::{px, token, ColorToken, Pixels};

    /// Row height (§2: "Row height 26").
    pub const ROW: Pixels = px(26.0);
    /// Lane canvas column width (§2: "lane canvas 100").
    pub const LANE_CANVAS: Pixels = px(100.0);
    /// A lane's vertical sits at `x = 9 + lane * 14` (§2).
    pub const LANE_X_BASE: Pixels = px(9.0);
    pub const LANE_STEP: Pixels = px(14.0);
    /// Each S-curve piece's own box width, and its base height (`crate::graph_view::render`'s
    /// `CurveBox::height` adds exactly one stroke to the bottom-edged curve's own height, so that
    /// GPUI's inside-painted bottom border lands on the waist row rather than one row above it -
    /// see that field's own docs). Must be at least `2 * ELBOW_RADIUS` - GPUI
    /// (`Corners::clamp_radii_for_quad_size`, `vendor/zed/crates/gpui/src/style.rs`) clamps every
    /// requested corner radius to half the box's own shorter side, so a smaller box would silently
    /// render a smaller radius than requested (this is exactly what happened before this constant
    /// existed: a 7px-square box with a 7px radius request rendered at an effective 3.5px, a real
    /// user-reported vertical-alignment bug traced back to this GPUI behavior).
    pub const ELBOW_CURVE_SIZE: Pixels = px(10.0);
    /// Each S-curve piece's real, rendered corner radius - always exactly half of
    /// `ELBOW_CURVE_SIZE`, so GPUI's own clamp (see that constant's docs) never kicks in and this
    /// value renders unclamped, not silently halved again.
    pub const ELBOW_RADIUS: Pixels = px(5.0);

    /// The tab chip's own background (§1: "`#2a2030` bg").
    pub const TAB_CHIP_BG: ColorToken = token("graph.tab_chip_bg", 0x2a2030);
    /// The tab chip's fork-glyph colour (§1: "`#c98fbf` fork glyph").
    pub const TAB_CHIP_FG: ColorToken = token("graph.tab_chip_fg", 0xc98fbf);

    /// Six lane colours, cycled by `lane % 6` - lane 0 is the trunk (§2).
    pub const LANES: [ColorToken; 6] = [
        token("graph.lanes.0", 0x6b9bd1),
        token("graph.lanes.1", 0xc98fbf),
        token("graph.lanes.2", 0x5cb87f),
        token("graph.lanes.3", 0xd8a94a),
        token("graph.lanes.4", 0xc0824a),
        token("graph.lanes.5", 0x8f8fd4),
    ];

    /// A local branch ref chip's dim background pair, indexed the same way as [`LANES`] (§2: "local
    /// branch = lane colour on its dim pair").
    pub const LOCAL_BRANCH_DIM_BG: [ColorToken; 6] = [
        token("graph.local_branch_dim_bg.0", 0x1a2733),
        token("graph.local_branch_dim_bg.1", 0x2a2030),
        token("graph.local_branch_dim_bg.2", 0x16261e),
        token("graph.local_branch_dim_bg.3", 0x2b2413),
        token("graph.local_branch_dim_bg.4", 0x2a1e13),
        token("graph.local_branch_dim_bg.5", 0x1f2033),
    ];

    /// `HEAD` ref chip (§2: "`HEAD` `#243c50`/`#a5cdf0`").
    pub const HEAD_CHIP_BG: ColorToken = token("graph.head_chip_bg", 0x243c50);
    pub const HEAD_CHIP_FG: ColorToken = token("graph.head_chip_fg", 0xa5cdf0);
    /// A remote branch chip is outlined only (§2: "remote outlined `#2a2f34`").
    pub const REMOTE_CHIP_BORDER: ColorToken = token("graph.remote_chip_border", 0x2a2f34);
    /// A tag chip (§2: "tag `#2b2413`/`#d8a94a`").
    pub const TAG_CHIP_BG: ColorToken = token("graph.tag_chip_bg", 0x2b2413);
    pub const TAG_CHIP_FG: ColorToken = token("graph.tag_chip_fg", 0xd8a94a);

    /// The commit dot's diameter (§2: "commit 7px filled").
    pub const DOT_COMMIT: Pixels = px(7.0);
    /// The `HEAD`/merge dot's diameter (§2: "**HEAD** 9px", "**merge** 9px").
    pub const DOT_HEAD_OR_MERGE: Pixels = px(9.0);
    /// The `HEAD` dot's ring colour (§2: "a 2px `#5a9ad4` ring").
    pub const HEAD_RING: ColorToken = token("graph.head_ring", 0x5a9ad4);
    /// The working-tree dot's dashed border colour (§2: "1px dashed `#6b7178` border").
    pub const WORKING_TREE_BORDER: ColorToken = token("graph.working_tree_border", 0x6b7178);

    /// The toolbar band's height (§4: "Toolbar 35 high").
    pub const TOOLBAR: Pixels = px(35.0);
    /// The column header band's height (`revision 3/REVISION-2026-07-31.md` §6.1: "Column
    /// header, 22 high"). Sits between [`TOOLBAR`] and the row list -
    /// `crate::graph_view::render::AdeApp::render_graph_view` renders it as a real sibling band,
    /// not a literal folded into the row list's own top padding, so the row `⋯` menu's anchor
    /// (built from a row's own real captured bounds, never a `TOOLBAR`/`HEADER`/index formula -
    /// see [`super::graph::ROW_MENU_HEIGHT`]'s neighbour `Self::toggle_graph_row_menu`) shifts
    /// down for free the moment this band exists, with zero changes to that anchor logic itself.
    pub const HEADER: Pixels = px(22.0);
    /// The column header band's own background (§6.1: "`#101315`") - close to but distinct from
    /// [`super::surface::HEADER`]'s `#121417` (context bar, panel headers), so kept as its own
    /// token rather than reused, the same "same-ish hex, distinct token for a distinct element"
    /// call `super::text::TREE_CARET`'s own doc comment already makes for [`super::text::PATH`].
    pub const HEADER_BG: ColorToken = token("graph.header_bg", 0x101315);
    /// The column header labels' colour (§6.1: "`#4a5057` - quieter than any row content").
    /// Same hex as [`super::text::PATH`]/[`super::text::TREE_CARET`] - again a distinct token
    /// for a distinct element, per those constants' own precedent.
    pub const HEADER_LABEL_FG: ColorToken = token("graph.header_label_fg", 0x4a5057);
    /// The `Push …` menu's width (§4: "opening a 268-wide menu").
    pub const PUSH_MENU_WIDTH: Pixels = px(268.0);
    /// The row `⋯` context menu's width (§4: "a 330-wide context menu").
    pub const ROW_MENU_WIDTH: Pixels = px(330.0);
    /// The row `⋯` context menu's painted height under the test suite's `gpui::TestAppContext` -
    /// its content is fixed (four headers, twelve action rows, one footer line; never varies with
    /// which row opened it), so unlike `crate::sidebar::context_menu::menu_height` (which has to
    /// measure a variable row count) this is a plain constant rather than a formula, pinned by
    /// `crate::graph_view::render::graph_row_menu_tests::
    /// the_row_menu_pins_the_real_height_this_edge_clamp_relies_on` - if that test ever fails, the
    /// menu's content changed and this must be re-measured, not guessed, the same discipline
    /// `crate::lsp::completion_popup::POPOVER_MAX_HEIGHT`'s own docs describe for a hand-derived
    /// popover size constant. Caveat an adversarial audit raised: the test harness's text system
    /// uses synthetic, not real-font, glyph metrics, so this may be off by roughly a line's worth
    /// of height from a real build's actual paint near the very edge of the clamp - the clamp
    /// degrades safely either way (it still keeps the menu on-screen, just not pixel-perfectly
    /// flush with the edge), so this has been left as a known imprecision rather than a blocker.
    pub const ROW_MENU_HEIGHT: Pixels = px(483.0);
    /// Behind-count amber threshold (§5: "behind turns `#a3873f` past 4").
    pub const BEHIND_WARN_THRESHOLD: usize = 4;
    pub const BEHIND_WARN: ColorToken = token("graph.behind_warn", 0xa3873f);
    /// Branches panel row height (§5: "28-high rows").
    pub const BRANCH_ROW: Pixels = px(28.0);
    /// Branches panel filter row height (§5: "a 31-high filter row").
    pub const BRANCHES_FILTER_ROW: Pixels = px(31.0);
    /// A branch with no lane in the visible graph gets a neutral dot (§5).
    pub const BRANCH_NO_LANE_DOT: ColorToken = token("graph.branch_no_lane_dot", 0x3d4248);

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("TAB_CHIP_BG", TAB_CHIP_BG),
        ("TAB_CHIP_FG", TAB_CHIP_FG),
        ("LANES.0", LANES[0]),
        ("LANES.1", LANES[1]),
        ("LANES.2", LANES[2]),
        ("LANES.3", LANES[3]),
        ("LANES.4", LANES[4]),
        ("LANES.5", LANES[5]),
        ("LOCAL_BRANCH_DIM_BG.0", LOCAL_BRANCH_DIM_BG[0]),
        ("LOCAL_BRANCH_DIM_BG.1", LOCAL_BRANCH_DIM_BG[1]),
        ("LOCAL_BRANCH_DIM_BG.2", LOCAL_BRANCH_DIM_BG[2]),
        ("LOCAL_BRANCH_DIM_BG.3", LOCAL_BRANCH_DIM_BG[3]),
        ("LOCAL_BRANCH_DIM_BG.4", LOCAL_BRANCH_DIM_BG[4]),
        ("LOCAL_BRANCH_DIM_BG.5", LOCAL_BRANCH_DIM_BG[5]),
        ("HEAD_CHIP_BG", HEAD_CHIP_BG),
        ("HEAD_CHIP_FG", HEAD_CHIP_FG),
        ("REMOTE_CHIP_BORDER", REMOTE_CHIP_BORDER),
        ("TAG_CHIP_BG", TAG_CHIP_BG),
        ("TAG_CHIP_FG", TAG_CHIP_FG),
        ("HEAD_RING", HEAD_RING),
        ("WORKING_TREE_BORDER", WORKING_TREE_BORDER),
        ("HEADER_BG", HEADER_BG),
        ("HEADER_LABEL_FG", HEADER_LABEL_FG),
        ("BEHIND_WARN", BEHIND_WARN),
        ("BRANCH_NO_LANE_DOT", BRANCH_NO_LANE_DOT),
    ];
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
    /// Shared height for the work-surface tab strip, the session-rail header, and the
    /// files/changes panel header - the three sit side by side under the title bar and must
    /// line up pixel-perfect, so they read off one constant instead of three values that could
    /// drift independently.
    pub const CHROME_HEADER: Pixels = px(36.0);
    pub const CONTEXT_BAR: Pixels = px(32.0);
    pub const DIFF_TOOLBAR: Pixels = px(31.0);
    pub const FILTER_ROW: Pixels = px(30.0);
    pub const SURFACE_FOOTER: Pixels = px(28.0);
    pub const PTY_HEADER: Pixels = px(27.0);
    /// The terminal pane's info footer band (`pid` · grid dimensions · environment chip ·
    /// right-aligned static copy) - distinct from [`SURFACE_FOOTER`] (the agent-level
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
    /// GitHub issue #129: the shared shadow for every dropdown/context-menu in the app - distinct
    /// from [`PALETTE`]'s `0 12 34`. Named `MENU`, not `PLUS_MENU` (its name before this issue) -
    /// it was already used by the title bar menu, tree context menu, and git graph's push/row
    /// menus too, not just the `+` menu; only the name had drifted from what it actually covers.
    pub const MENU: (Pixels, Pixels, Pixels) = (px(0.0), px(14.0), px(30.0));
    // rgba(0,0,0,0.55)
    /// The commit composer's `▾` split-button popover shadow (Revision R12 §5) - same blur/alpha
    /// as [`MENU`], just a negative `y`: unlike every other popover in this module, it opens
    /// *upward* from a button near the bottom of the Changes panel. Before GitHub issue #129 this
    /// also had its own, slightly different blur/alpha (`26`/`0.5`, vs `MENU`'s `30`/`0.55`) with
    /// no real reason for the difference beyond having been introduced separately - direction is
    /// the only genuine difference this popover needs.
    pub const COMMIT_MENU: (Pixels, Pixels, Pixels) = (px(0.0), px(-14.0), px(30.0));
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
/// Scaled: the agent rail (`crate::rail::render`); the title bar/status bar
/// (`crate::status_bar::render`); the command palette's row labels/hints
/// (`crate::palette::render`); the Files/Changes sidebar's row labels, footer hint, and
/// tree caret (`crate::sidebar::render`); the file/agent tab strip's tab labels
/// (`crate::work_surface::render`); and every Settings row's label/hint *and* control
/// (stepper value, choice-segment labels, config banner text, snippet block text - all in
/// `crate::settings::widgets`).
///
/// Deliberately not scaled, each for its own reason: the code surface and terminal panes have
/// their own dedicated font-size mechanisms (`AdeApp::effective_code_rem_px`,
/// `Settings.appearance.terminal_font_size`) that a second multiplier would compound with;
/// chips/badges/keycaps/close-tab glyphs app-wide are small, fixed-size shapes the design treats
/// as part of a component rather than running text; and the rest of `crate::work_surface::render`'s own
/// chrome (agent context bar, toolbar buttons, `+` menu, footer action buttons) is real,
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

/// Palette-only (⌘P) colours read directly from `Jerry.dc.html`'s inline literals for the
/// `paletteOpen` block - real values missing from `tokens.rs`'s transcription (predates the
/// palette section).
pub mod palette {
    use super::{token, ColorToken};

    /// The input row's scope-prefix glyph (`Jerry.dc.html`: `color:#5f7f9e`).
    pub const PREFIX: ColorToken = token("palette.prefix", 0x5f7f9e);
    /// A result group's uppercase header label (`Jerry.dc.html`: `color:#5b6167`) - close to
    /// but distinct from [`super::text::FAINT`] (`#6b7178`).
    pub const GROUP_HEADER: ColorToken = token("palette.group_header", 0x5b6167);
    /// An unselected result row's hover background (`Jerry.dc.html`: `style-hover`:
    /// `background:#191d20`) - distinct from [`super::surface::ROW_HOVER`] (`#15181b`, which
    /// happens to equal the palette panel's own background, [`super::surface::PALETTE`]).
    pub const ROW_HOVER: ColorToken = token("palette.row_hover", 0x191d20);
    /// The selected/first row's label colour (`Jerry.dc.html`: `fg: first ? '#e3e8ed' :
    /// '#c2c7cc'`) - one hex step brighter than [`super::text::SELECTED`] (`#dde2e7`).
    pub const LABEL_SELECTED: ColorToken = token("palette.label_selected", 0xe3e8ed);
    /// A command result's kind chip `(fg, bg)` (`Jerry.dc.html`: `background:#1d2532` /
    /// `color:#7f9ad4`) - the same hex pair as [`super::lang::MD`], kept as its own token since
    /// a command chip and a Markdown-file chip are unrelated concepts.
    pub const COMMAND_CHIP: (ColorToken, ColorToken) = (
        token("palette.command_chip.fg", 0x7f9ad4),
        token("palette.command_chip.bg", 0x1d2532),
    );

    /// Every real [`ColorToken`] this module declares, paired with its own Rust `const` name -
    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry. See that constant's
    /// own docs for what walks this and why every token has to appear here.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("PREFIX", PREFIX),
        ("GROUP_HEADER", GROUP_HEADER),
        ("ROW_HOVER", ROW_HOVER),
        ("LABEL_SELECTED", LABEL_SELECTED),
        ("COMMAND_CHIP.fg", COMMAND_CHIP.0),
        ("COMMAND_CHIP.bg", COMMAND_CHIP.1),
    ];
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
    // `crate::sidebar::file_tree::tests::same` exists.
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
            // GitHub issue #32's three and issue #154's two - the original version of this test
            // covered only the eight chips that existed when it was written, so every chip added
            // since had been going unchecked against the very rule its own doc comment claims.
            ("json", lang::JSON),
            ("yaml", lang::YAML),
            ("c", lang::C),
            ("html", lang::HTML),
            ("css", lang::CSS),
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

/// Real, source-parsing coverage that [`TOKEN_GROUPS`] is honestly *total* - the property every
/// other piece of this rewrite depends on. The registry is what theme-file key validation
/// (`crate::settings::custom_theme`), the built-in theme generator
/// (`crate::settings::builtin_themes`) and the "generate from colour" action all walk, so a token
/// that exists in this file but not in the registry would be a colour no theme could ever change
/// *and* one no generated file would ever mention - silently, with nothing else failing.
///
/// Reflection can't see `const` declarations, so these tests read this module's own real source
/// (`include_str!("theme.rs")` - the literal file being compiled, not a copy) and compare what it
/// declares against what the registry lists.
#[cfg(test)]
mod token_registry_tests {
    use super::*;

    /// Every real `pub const ...: ColorToken` (and `(ColorToken, ColorToken)`, and
    /// `[ColorToken; N]`) declaration in this file, as `(module, const name, how many keys it
    /// contributes)`, parsed straight out of the source text.
    fn declared_in_source() -> Vec<(String, String, usize)> {
        const SOURCE: &str = include_str!("theme.rs");
        let mut module = String::new();
        let mut found = Vec::new();
        for line in SOURCE.lines() {
            if let Some(rest) = line.strip_prefix("pub mod ") {
                if let Some(name) = rest.strip_suffix(" {") {
                    module = name.to_string();
                }
                continue;
            }
            let Some(rest) = line.strip_prefix("    pub const ") else {
                continue;
            };
            let Some((name, type_and_value)) = rest.split_once(": ") else {
                continue;
            };
            let count = if type_and_value.starts_with("ColorToken = token(") {
                1
            } else if type_and_value.starts_with("(ColorToken, ColorToken) = ") {
                2
            } else if let Some(rest) = type_and_value.strip_prefix("[ColorToken; ") {
                rest.split(']')
                    .next()
                    .and_then(|digits| digits.parse::<usize>().ok())
                    .expect("a [ColorToken; N] declaration must name a real N")
            } else {
                continue;
            };
            found.push((module.clone(), name.to_string(), count));
        }
        found
    }

    #[test]
    fn the_source_parser_itself_finds_a_plausible_number_of_real_declarations() {
        // A guard on the *test*, not the code under test: if this file's formatting ever drifts
        // far enough that the parser above silently stops matching declarations, every other test
        // here would vacuously pass over an empty list.
        let declared = declared_in_source();
        assert!(
            declared.len() > 150,
            "only parsed {} token declarations out of this module's own source - the parser has \
             almost certainly stopped matching, not the palette shrunk",
            declared.len()
        );
        assert!(declared
            .iter()
            .any(|(m, n, _)| m == "surface" && n == "WINDOW"));
        assert!(declared
            .iter()
            .any(|(m, n, c)| m == "agent" && n == "SONNET" && *c == 2));
        assert!(declared
            .iter()
            .any(|(m, n, c)| m == "graph" && n == "LANES" && *c == 6));
    }

    #[test]
    fn every_real_color_token_in_this_file_is_registered() {
        for (module, name, count) in declared_in_source() {
            let Some((_, tokens)) = TOKEN_GROUPS.iter().find(|(group, _)| *group == module) else {
                panic!(
                    "module `{module}` declares real ColorTokens (e.g. {name}) but is missing from \
                     TOKEN_GROUPS entirely - every one of its tokens would be unthemeable"
                );
            };
            let registered = tokens
                .iter()
                .filter(|(registered_name, _)| {
                    *registered_name == name || registered_name.starts_with(&format!("{name}."))
                })
                .count();
            assert_eq!(
                registered, count,
                "{module}::{name} contributes {count} real key(s) but TOKEN_GROUPS lists \
                 {registered} - a token missing from the registry can never be themed and never \
                 appears in a generated theme file"
            );
        }
    }

    #[test]
    fn every_registered_token_key_matches_its_own_const_name_and_module() {
        for (module, tokens) in TOKEN_GROUPS {
            for (name, token) in *tokens {
                let expected = format!("{module}.{}", name.to_ascii_lowercase());
                assert_eq!(
                    token.key, expected,
                    "{module}::{name}'s key is {:?}, but this module's documented naming rule \
                     (\"{{module}}.{{const name lowercased}}\") makes it {expected:?} - a theme \
                     file author reading the Rust source would name the wrong key",
                    token.key
                );
            }
        }
    }

    #[test]
    fn every_registered_key_is_unique_across_the_whole_app() {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for token in all_tokens() {
            assert!(
                seen.insert(token.key),
                "{:?} is registered twice - one of the two would silently shadow the other in \
                 every compiled palette",
                token.key
            );
        }
    }

    #[test]
    fn token_for_key_round_trips_every_real_key_and_rejects_anything_else() {
        for token in all_tokens() {
            assert_eq!(
                token_for_key(token.key).map(|found| found.key),
                Some(token.key)
            );
        }
        assert_eq!(token_for_key("surface.no_such_token"), None);
        assert_eq!(token_for_key("not_a_module.window"), None);
        assert_eq!(
            token_for_key(""),
            None,
            "the unthemeable literal key is not a real token"
        );
    }

    /// The registry has to be big enough to be believable as "the whole app's palette" - a real
    /// floor, not an exact pin (adding a token shouldn't break a test).
    #[test]
    fn the_registry_covers_the_whole_palette_not_a_sample_of_it() {
        assert!(
            all_tokens().count() >= 250,
            "only {} registered tokens - this app's palette is ~270",
            all_tokens().count()
        );
    }
}

/// Real regression coverage for the live theme-resolution mechanism itself - [`CURRENT_THEME`] is
/// real, thread-local, mutable state, so every test here installs its palette through
/// [`with_palette`], which restores the Jerry Dark default on `Drop` (so it still runs if the test
/// body panics). A test leaking a palette would silently corrupt every *other* test on the same
/// thread that reads a colour token.
#[cfg(test)]
mod theme_runtime_tests {
    use super::*;

    struct ResetThemeOnDrop;

    impl Drop for ResetThemeOnDrop {
        fn drop(&mut self) {
            set_current_theme(None);
        }
    }

    fn with_palette(entries: &[(&'static str, u32)]) -> ResetThemeOnDrop {
        let palette: Palette = entries
            .iter()
            .map(|(key, value)| (*key, hex_rgba(*value)))
            .collect();
        set_current_theme(Some(Rc::new(palette)));
        ResetThemeOnDrop
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    /// The real identity case: with no palette installed, `resolve()` returns the token's own
    /// compiled default completely unchanged - not even a lossy `Rgba -> Hsla -> Rgba` round trip
    /// (see the module docs for why this matters for every other exact-hex test in this crate).
    #[test]
    fn jerry_dark_resolve_is_bit_exact_with_no_lookup_at_all() {
        assert!(
            current_theme_palette().is_none(),
            "the real default before any test touches it"
        );
        assert!(same(surface::WINDOW.resolve(), surface::WINDOW.default));
        assert!(same(syntax::KEYWORD.resolve(), hex_rgba(0xb477cf)));
    }

    /// The real, load-bearing proof a palette actually changes what gets rendered.
    #[test]
    fn an_installed_palette_really_changes_what_a_token_resolves_to() {
        let jerry_dark = surface::WINDOW.resolve();
        let _guard = with_palette(&[("surface.window", 0xf4f1ea)]);
        assert!(!same(surface::WINDOW.resolve(), jerry_dark));
        assert!(same(surface::WINDOW.resolve(), hex_rgba(0xf4f1ea)));
    }

    /// A *partial* palette - the whole point of the "override only what you want" file format:
    /// keys it doesn't name keep resolving to their own compiled defaults, in the same breath as
    /// the ones it does name resolve to its values.
    #[test]
    fn a_partial_palette_leaves_every_key_it_does_not_name_on_its_own_default() {
        let _guard = with_palette(&[("syntax.keyword", 0xff79c6)]);
        assert!(same(syntax::KEYWORD.resolve(), hex_rgba(0xff79c6)));
        assert!(
            same(surface::WINDOW.resolve(), surface::WINDOW.default),
            "a key the palette never names must fall straight back to the token's own default"
        );
        assert!(same(text::BODY.resolve(), text::BODY.default));
    }

    /// Every former Rust-level alias is now independently overridable - the concrete thing this
    /// rewrite bought. Moving `syntax.operator` must not drag `syntax.text` (its old alias
    /// target) along with it, and vice versa.
    ///
    /// Uses `OPERATOR`/`TEXT` specifically because those two genuinely still *share* a default,
    /// which is what makes the test meaningful: if the two started from different values, an
    /// assertion that they resolve differently would pass trivially without proving anything about
    /// the override mechanism. (`VARIABLE_PARAMETER`/`VARIABLE` used to serve this role, until the
    /// identifier family got its own real colours - see `syntax`'s own module docs.)
    #[test]
    fn a_former_alias_can_now_be_moved_without_moving_what_it_used_to_alias() {
        assert!(
            same(syntax::OPERATOR.default, syntax::TEXT.default),
            "sanity check: the two still share a default, which is what makes this test meaningful"
        );
        let _guard = with_palette(&[("syntax.operator", 0x50fa7b)]);
        assert!(same(syntax::OPERATOR.resolve(), hex_rgba(0x50fa7b)));
        assert!(
            same(syntax::TEXT.resolve(), syntax::TEXT.default),
            "syntax::TEXT used to be the very same const - overriding the operator bucket must no \
             longer touch it"
        );
    }

    /// Clearing the palette restores the exact original values, not some residue - the real
    /// round-trip safety mutable global state needs.
    #[test]
    fn clearing_the_palette_restores_the_exact_original_values() {
        let original = surface::WINDOW.default;
        {
            let _guard = with_palette(&[("surface.window", 0x123456)]);
            assert!(!same(surface::WINDOW.resolve(), original));
        }
        assert!(current_theme_palette().is_none());
        assert!(same(surface::WINDOW.resolve(), original));
    }

    /// [`ColorToken::literal`]'s deliberately unthemeable colours ignore any installed palette -
    /// including one that (impossibly) tried to name the empty key.
    #[test]
    fn a_literal_token_is_never_themed() {
        let transparent = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
        let token = ColorToken::literal(transparent);
        let _guard = with_palette(&[("", 0xff0000), ("surface.window", 0xff0000)]);
        assert!(
            same(token.resolve(), transparent),
            "a literal colour must stay exactly what it was constructed with"
        );
    }

    #[test]
    fn theme_is_light_reads_the_real_window_background_lightness() {
        assert!(
            theme_is_light(hex_rgba(0xf4f1ea)),
            "Paper's own window background is light"
        );
        assert!(!theme_is_light(hex_rgba(0x0e0f11)), "Jerry Dark's is not");
        assert!(!theme_is_light(surface::WINDOW.default));
    }
}

/// Real coverage for the HSL derivation utility - no longer part of live resolution (see
/// [`derive_shift`]'s own docs), but still the real generator behind the five migrated built-in
/// theme files, the "generate from colour" action, and an imported VSCode theme's whole-app chrome.
#[cfg(test)]
mod derivation_tests {
    use super::*;

    /// [`derive_shift`]'s lightness fit is solved from the two background-ish swatches
    /// specifically (index 0 and 1) - a real, direct unit test of the pure function.
    #[test]
    fn derive_shift_solves_an_exact_linear_fit_through_the_two_background_swatches() {
        // A synthetic "base" theme (window bg lightness ~10%, panel ~20%) and "target" theme
        // (window bg ~50%, panel ~70%) - the fit should map base 0.10 -> target 0.50 and base
        // 0.20 -> target 0.70 exactly.
        let base = [0x1a1a1a, 0x333333, 0x808080, 0x808080, 0x808080];
        let target = [0x808080, 0xb3b3b3, 0x808080, 0x808080, 0x808080];
        let shift = derive_shift(base, target);

        let remap = |hex_value: u32| -> f32 {
            let hsla: Hsla = hex_rgba(hex_value).into();
            (hsla.l * shift.lightness_scale + shift.lightness_offset).clamp(0.0, 1.0)
        };
        let target_bg: Hsla = hex_rgba(0x808080).into();
        let target_panel: Hsla = hex_rgba(0xb3b3b3).into();

        assert!((remap(0x1a1a1a) - target_bg.l).abs() < 0.01);
        assert!((remap(0x333333) - target_panel.l).abs() < 0.01);
    }

    /// A degenerate `base` (identical window/panel lightness - a real divide-by-near-zero case in
    /// the lightness fit) must fall back to an identity scale rather than producing `NaN`/`inf`
    /// and corrupting every generated colour.
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

    /// [`shift_from_seed`]'s documented contract: hue and saturation only, lightness untouched.
    #[test]
    fn shift_from_seed_rotates_hue_and_scales_saturation_but_never_lightness() {
        let seed = hex_rgba(0xe07a5f); // a warm coral, far from Jerry Dark's accent blue
        let shift = shift_from_seed(seed);
        assert_eq!(shift.lightness_scale, 1.0);
        assert_eq!(shift.lightness_offset, 0.0);

        // The reference accent, run through this shift, must land on the seed's own hue - that is
        // the whole promise of "generate a theme from this colour".
        let rotated: Hsla = apply_shift(hex_rgba(0x74ade8), shift).into();
        let seed_hsla: Hsla = seed.into();
        assert!(
            (rotated.h - seed_hsla.h).abs() < 0.01,
            "the app's accent should land on the seed's hue ({} vs {})",
            rotated.h,
            seed_hsla.h
        );
        assert!((rotated.s - seed_hsla.s).abs() < 0.02);
    }

    /// A seed identical to the reference accent is a real no-op, not a near-miss.
    #[test]
    fn a_seed_equal_to_the_reference_accent_derives_the_identity_palette() {
        let shift = shift_from_seed(hex_rgba(0x74ade8));
        for (key, color) in derived_palette(shift) {
            let token = token_for_key(key).expect("every derived key is a real registered token");
            let (r, g, b) = (
                (color.r * 255.0).round(),
                (color.g * 255.0).round(),
                (color.b * 255.0).round(),
            );
            let (dr, dg, db) = (
                (token.default.r * 255.0).round(),
                (token.default.g * 255.0).round(),
                (token.default.b * 255.0).round(),
            );
            assert!(
                (r - dr).abs() <= 1.0 && (g - dg).abs() <= 1.0 && (b - db).abs() <= 1.0,
                "{key} moved ({r},{g},{b}) away from its own default ({dr},{dg},{db}) under a \
                 seed that is literally the reference accent"
            );
        }
    }

    /// [`derived_palette`] really covers the *whole* registry - the property that makes a
    /// generated theme file a complete palette rather than a partial one.
    #[test]
    fn derived_palette_names_every_registered_token_exactly_once() {
        let shift = shift_from_seed(hex_rgba(0x8fae6b));
        let derived = derived_palette(shift);
        assert_eq!(derived.len(), all_tokens().count());
        let keys: std::collections::HashSet<&str> = derived.iter().map(|(key, _)| *key).collect();
        for token in all_tokens() {
            assert!(
                keys.contains(token.key),
                "{} is missing from a derived palette",
                token.key
            );
        }
    }
}

/// GitHub issue #31's "verify contrast across the bundled light and dark themes" checklist item -
/// a real, computed WCAG 2.x contrast-ratio check (not eyeballed), for every one of [`syntax`]'s
/// real foreground tokens against the work-surface background ([`surface::CENTER`]) they actually
/// render on, across every one of the six real bundled themes.
///
/// Now measured through the *real* mechanism this rewrite installed: each theme's own checked-in
/// file, compiled into a real [`Palette`] and installed exactly as selecting it in the app would,
/// rather than through a live HSL derivation. The numbers are unchanged, which is the point - the
/// migration was required to preserve every colour.
///
/// ## Why the threshold is 2.5:1, not WCAG's own 4.5:1
///
/// A real, honest finding from computing this rather than assuming it: [`syntax::COMMENT`]
/// (`#5d636f` in Jerry Dark) was **already** the dimmest token in this palette, at a measured
/// 3.03:1 against [`surface::CENTER`] in Jerry Dark itself - deliberately dim, a real,
/// pre-existing design choice (a comment should recede), not a regression. WCAG's own 4.5:1
/// "normal text" minimum would fail that pre-existing token outright, in the one theme this whole
/// palette was hand-authored against. 2.5:1 is chosen instead as a real, still-meaningful floor -
/// well above "invisible" (a ratio near 1.0) while not rejecting a token this codebase already
/// ships and that this issue was never asked to re-tune.
///
/// The stricter sweep covers Jerry Dark and Paper (the one bundled light theme) specifically -
/// what the issue asks for by name. A second, wider sweep covers all six at a deliberately looser
/// 1.5:1 floor: `Slate`'s and `Ember`'s own derived `COMMENT` measures as low as ~2.15:1, a real,
/// honestly-disclosed pre-existing gap in the derivation those files were generated from, not
/// something this change caused - wide enough to pass every real measured value while still
/// catching a genuine near-invisible pairing should a future edit introduce one.
#[cfg(test)]
mod syntax_contrast_tests {
    use super::*;

    pub(super) struct ResetThemeOnDrop;

    impl Drop for ResetThemeOnDrop {
        fn drop(&mut self) {
            set_current_theme(None);
        }
    }

    /// Installs a real bundled theme exactly the way selecting its card does - compiled from its
    /// own checked-in file through the real `base` chain, not a synthesized palette.
    pub(super) fn with_bundled_theme(name: &str) -> ResetThemeOnDrop {
        let palette = crate::settings::custom_theme::compile_palette_by_name(name, &[])
            .expect("a bundled theme must compile");
        set_current_theme(palette.map(Rc::new));
        ResetThemeOnDrop
    }

    /// WCAG 2.x relative luminance (<https://www.w3.org/TR/WCAG21/#dfn-relative-luminance>),
    /// applied directly to [`Rgba`]'s own already-`0.0..=1.0` sRGB components - the same formula
    /// every standard WCAG contrast checker uses.
    fn relative_luminance(color: Rgba) -> f32 {
        fn channel(component: f32) -> f32 {
            if component <= 0.03928 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    /// The real WCAG contrast ratio between two resolved colours - order-independent (always
    /// `>= 1.0`), matching the standard definition.
    pub(super) fn contrast_ratio(a: Rgba, b: Rgba) -> f32 {
        let (luminance_a, luminance_b) = (relative_luminance(a), relative_luminance(b));
        let (higher, lower) = if luminance_a > luminance_b {
            (luminance_a, luminance_b)
        } else {
            (luminance_b, luminance_a)
        };
        (higher + 0.05) / (lower + 0.05)
    }

    /// Every real [`syntax`] foreground token a [`crate::code_surface::code_view::HighlightKind`]
    /// can resolve to - not [`syntax`]'s handful of non-scope tokens (`CARET`/`*_UNDERLINE`/
    /// `DIAGNOSTIC_*`), which aren't code-surface *text* colours painted over [`surface::CENTER`].
    /// Built from `HighlightKind::ALL` itself rather than a hand-listed array, so a new bucket is
    /// covered here automatically.
    fn syntax_tokens() -> Vec<(&'static str, ColorToken)> {
        crate::code_surface::code_view::HighlightKind::ALL
            .into_iter()
            .map(|kind| {
                let key: &'static str =
                    Box::leak(format!("syntax.{}", kind.name()).into_boxed_str());
                (
                    key,
                    token_for_key(key).expect("every HighlightKind has a real syntax token"),
                )
            })
            .collect()
    }

    #[test]
    fn every_syntax_token_clears_a_real_contrast_floor_in_jerry_dark_and_paper() {
        const MIN_RATIO: f32 = 2.5;
        for name in ["Jerry Dark", "Paper"] {
            let _guard = with_bundled_theme(name);
            let background = surface::CENTER.resolve();
            for (key, token) in syntax_tokens() {
                let ratio = contrast_ratio(token.resolve(), background);
                assert!(
                    ratio >= MIN_RATIO,
                    "{key} only reaches {ratio:.2}:1 against surface::CENTER in {name} - below \
                     the real {MIN_RATIO}:1 floor"
                );
            }
        }
    }

    #[test]
    fn every_syntax_token_clears_a_looser_floor_across_every_bundled_theme() {
        const MIN_RATIO: f32 = 1.5;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let background = surface::CENTER.resolve();
            for (key, token) in syntax_tokens() {
                let ratio = contrast_ratio(token.resolve(), background);
                assert!(
                    ratio >= MIN_RATIO,
                    "{key} only reaches {ratio:.2}:1 against surface::CENTER in {} - below the \
                     real {MIN_RATIO}:1 floor",
                    def.name
                );
            }
        }
    }

    /// A real, disclosed self-check on the contrast machinery itself: the same colour against
    /// itself must measure exactly `1.0`, and pure black against pure white must measure the real,
    /// well-known WCAG maximum of `21.0`.
    #[test]
    fn contrast_ratio_matches_known_reference_values() {
        let white = Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        };
        let black = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.001);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.01);
    }
}

/// The identifier family's own regression coverage - [`syntax::VARIABLE`],
/// [`syntax::VARIABLE_PARAMETER`] and [`syntax::PROPERTY`] must really read as their own colours,
/// in **every** bundled theme, not just in Jerry Dark.
///
/// That "every bundled theme" part is the load-bearing half. Those three tokens used to default to
/// [`syntax::TEXT`]'s near-white grey, which made most of a source file render as one
/// undifferentiated tone. Giving them real hues fixes Jerry Dark on its own, but the other five
/// bundled themes are *generated files* (`crate::settings::builtin_themes`) holding literal
/// per-token colours - so a change to these defaults that forgets to regenerate them would leave
/// those five silently serving the old near-white value, which would be worse than not fixing this
/// at all. Every check below runs against each theme's own real compiled palette, so a stale
/// generated file fails here rather than shipping.
/// GitHub issue #168's bracket-pair depth ring, pinned the same way the identifier family below
/// is: the three properties the six colours were actually selected against, measured in **every**
/// bundled theme rather than only in Jerry Dark. The five non-Jerry-Dark themes are generated by
/// running [`derive_shift`] over these defaults, so a change to a default that looks fine in Jerry
/// Dark can still collapse under the derivation - which is not hypothetical: an earlier draft's
/// `#9b8cff` derived to `#020109` under `Paper`'s inverting lightness fit, ΔE 8.8 from that
/// theme's own plain text, i.e. a coloured bracket indistinguishable from an uncoloured one. Every
/// floor here would have caught it.
#[cfg(test)]
mod syntax_bracket_ring_tests {
    use super::syntax_contrast_tests::{contrast_ratio, with_bundled_theme};
    use super::syntax_identifier_palette_tests::delta_e;
    use super::*;
    use crate::code_surface::code_view::HighlightKind;

    /// A colour's CIE-Lab chroma (`sqrt(a*^2 + b* ^2)`) - how *saturated* it is, independent of
    /// how light it is. The one number that exposed the replaced ring as out of family, and the
    /// one every ΔE-only check was blind to.
    fn chroma(color: Rgba) -> f32 {
        let (_, a, b) = super::syntax_identifier_palette_tests::lab(color);
        (a * a + b * b).sqrt()
    }

    /// The ring's six real tokens, in ring order - read through `HighlightKind` rather than
    /// hand-listed, so this can't silently drift from what the renderer actually paints.
    fn ring_tokens() -> Vec<(&'static str, ColorToken)> {
        HighlightKind::BRACKET_DEPTH_RING
            .into_iter()
            .map(|kind| {
                let key: &'static str =
                    Box::leak(format!("syntax.{}", kind.name()).into_boxed_str());
                (
                    key,
                    token_for_key(key).expect("every ring bucket has a real syntax token"),
                )
            })
            .collect()
    }

    /// The whole point of the feature: two nesting levels a reader is comparing must not look
    /// alike. Cyclically adjacent (depth `n` against `n + 1`) is the pair that matters most, since
    /// those two nest directly inside one another.
    #[test]
    fn cyclically_adjacent_ring_colours_stay_far_apart_in_every_bundled_theme() {
        // Lower than the replaced ring's 40 on purpose: that number came from an unconstrained
        // optimiser that bought ΔE with saturation this palette never uses. 30 is set from what a
        // reader needs - >13x the ~2.3 just-noticeable difference - and the real measured worst
        // case across every bundled theme is 34.0. See this module's own "bracket-pair depth ring"
        // docs for the full story.
        const MIN_DELTA_E: f32 = 30.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let ring = ring_tokens();
            for index in 0..ring.len() {
                let (name_a, token_a) = ring[index];
                let (name_b, token_b) = ring[(index + 1) % ring.len()];
                let distance = delta_e(token_a.resolve(), token_b.resolve());
                assert!(
                    distance >= MIN_DELTA_E,
                    "{name_a} and {name_b} are adjacent depths but only ΔE {distance:.1} apart in \
                     {} - a reader could not tell one nesting level from the next",
                    def.name
                );
            }
        }
    }

    /// Non-adjacent depths matter too, just less: six levels of nesting has to stay legible, not
    /// merely three.
    #[test]
    fn no_two_ring_colours_collide_in_any_bundled_theme() {
        // Non-adjacent depths matter less than adjacent ones - see the floor above for why these
        // numbers moved down. Real measured worst case: 26.7.
        const MIN_DELTA_E: f32 = 24.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let ring = ring_tokens();
            for (index, (name_a, token_a)) in ring.iter().enumerate() {
                for (name_b, token_b) in ring.iter().skip(index + 1) {
                    let distance = delta_e(token_a.resolve(), token_b.resolve());
                    assert!(
                        distance >= MIN_DELTA_E,
                        "{name_a} and {name_b} are only ΔE {distance:.1} apart in {}",
                        def.name
                    );
                }
            }
        }
    }

    /// A *matched* bracket reading identically to an *unmatched* one would erase the whole
    /// matched/unmatched distinction this feature's honest-degradation design rests on - and
    /// `syntax::PUNCTUATION_BRACKET` is exactly `syntax::TEXT` by deliberate design, so this one
    /// check covers both.
    #[test]
    fn every_ring_colour_is_perceptibly_different_from_plain_text() {
        // Real measured worst case: 19.0, in `Paper`. Still nearly 2x the floor the identifier
        // family is held to against the same background.
        const MIN_DELTA_E: f32 = 17.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let text = syntax::TEXT.resolve();
            for (name, token) in ring_tokens() {
                let distance = delta_e(token.resolve(), text);
                assert!(
                    distance >= MIN_DELTA_E,
                    "{name} is only ΔE {distance:.1} from plain text in {} - a matched bracket \
                     would be indistinguishable from an unmatched one. If this fired after a \
                     change to the defaults, the five generated theme files probably need \
                     regenerating (see crate::settings::builtin_themes).",
                    def.name
                );
            }
        }
    }

    /// A bracket is one thin glyph, so the ring is held to a stricter contrast floor than the
    /// 1.5:1 `syntax_contrast_tests` applies to the four non-Jerry-Dark, non-`Paper` themes.
    #[test]
    fn every_ring_colour_clears_a_stricter_contrast_floor_than_the_palette_at_large() {
        const MIN_RATIO: f32 = 2.5;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let background = surface::CENTER.resolve();
            for (name, token) in ring_tokens() {
                let ratio = contrast_ratio(token.resolve(), background);
                assert!(
                    ratio >= MIN_RATIO,
                    "{name} only reaches {ratio:.2}:1 against surface::CENTER in {} - below the \
                     {MIN_RATIO}:1 floor a single-glyph token needs",
                    def.name
                );
            }
        }
    }

    /// The regression test for the real bug this ring was rewritten to fix, and the one thing
    /// every other check here missed.
    ///
    /// The first version of this ring was produced by maximising pairwise ΔE in open colour space.
    /// Maximising ΔE rewards chroma, so it bought its separation with saturation: two of its six
    /// colours reached C* 88.7 and 93.3 against a palette whose most saturated token
    /// ([`syntax::KEYWORD`]) is C* 53.4 and whose mean is 33.7. Every distinctness test above
    /// passed. It still looked wrong, because a colour set can be perfectly distinguishable and
    /// still not belong to the palette it sits in.
    ///
    /// So: no ring colour may be more saturated than this palette's own most saturated token, and
    /// the ring's mean chroma must stay near the palette's. That is what "derived from the theme's
    /// own hues" actually has to mean numerically.
    #[test]
    fn the_ring_stays_inside_the_palettes_own_chroma_register() {
        /// The real, semantic (non-neutral, non-ring) tokens the ring has to live alongside.
        fn palette_chromas() -> Vec<f32> {
            [
                syntax::KEYWORD,
                syntax::FUNCTION,
                syntax::TYPE,
                syntax::CONSTANT,
                syntax::STRING,
                syntax::STRING_ESCAPE,
                syntax::VARIABLE,
                syntax::VARIABLE_PARAMETER,
                syntax::PROPERTY,
                syntax::ATTRIBUTE,
                syntax::EMPHASIS,
            ]
            .into_iter()
            .map(|token| chroma(token.resolve()))
            .collect()
        }

        // Enforced strictly in Jerry Dark, which is where these colours are actually *authored*.
        // The other five are mechanical `derive_shift` transforms of exactly these values, and
        // that transform scales HSL saturation uniformly while CIE-Lab chroma responds
        // non-uniformly by hue and lightness - so a derived theme can push one ring colour a
        // little past its own palette max (measured worst: Ember, 1.44x) without anything being
        // wrong with the choice made here. Those get a loose blowout bound instead of a strict
        // one; tightening it would be pinning an artifact of the derivation, not the palette work.
        {
            let _guard = with_bundled_theme("Jerry Dark");
            let palette = palette_chromas();
            let palette_max = palette.iter().copied().fold(0.0f32, f32::max);
            let palette_mean = palette.iter().sum::<f32>() / palette.len() as f32;
            for (name, token) in ring_tokens() {
                let ring_chroma = chroma(token.resolve());
                assert!(
                    ring_chroma <= palette_max,
                    "{name} has chroma {ring_chroma:.1}, above this palette's own most saturated \
                     token ({palette_max:.1}) - the bracket ring must belong to the palette's \
                     register, not shout over it. The replaced ring reached 93.3 here."
                );
            }
            let ring_mean = ring_tokens()
                .into_iter()
                .map(|(_, token)| chroma(token.resolve()))
                .sum::<f32>()
                / 6.0;
            assert!(
                ring_mean <= palette_mean * 1.25,
                "the ring's mean chroma is {ring_mean:.1} against the palette's own \
                 {palette_mean:.1} ({:.2}x) - it has drifted out of register. If this fired after \
                 a colour change, the ring was probably re-picked by ΔE distance alone, which \
                 rewards saturation; see this module's \"bracket-pair depth ring\" docs. The \
                 replaced ring measured 1.97x here.",
                ring_mean / palette_mean
            );
        }

        // Every bundled theme: a loose bound that still catches a gross blowout.
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let palette = palette_chromas();
            let palette_mean = palette.iter().sum::<f32>() / palette.len() as f32;
            let ring_mean = ring_tokens()
                .into_iter()
                .map(|(_, token)| chroma(token.resolve()))
                .sum::<f32>()
                / 6.0;
            assert!(
                ring_mean <= palette_mean * 1.6,
                "the ring's mean chroma is {ring_mean:.1} against {}'s own {palette_mean:.1} - \
                 far enough out of register to read as a foreign accent",
                def.name
            );
        }
    }

    /// Each ring colour deliberately *borrows* a semantic token's hue - that is the whole point -
    /// but must never be mistakable for it. Real measured worst case: ΔE 14.8, `BRACKET_6` against
    /// `FUNCTION`, both blues.
    #[test]
    fn no_ring_colour_impersonates_the_semantic_token_it_borrows_its_hue_from() {
        const MIN_DELTA_E: f32 = 12.0;
        let semantic: [(&str, ColorToken); 8] = [
            ("KEYWORD", syntax::KEYWORD),
            ("FUNCTION", syntax::FUNCTION),
            ("TYPE", syntax::TYPE),
            ("CONSTANT", syntax::CONSTANT),
            ("STRING", syntax::STRING),
            ("VARIABLE", syntax::VARIABLE),
            ("VARIABLE_PARAMETER", syntax::VARIABLE_PARAMETER),
            ("ATTRIBUTE", syntax::ATTRIBUTE),
        ];
        let _guard = with_bundled_theme("Jerry Dark");
        for (ring_name, ring_token) in ring_tokens() {
            for (semantic_name, semantic_token) in semantic {
                let distance = delta_e(ring_token.resolve(), semantic_token.resolve());
                assert!(
                    distance >= MIN_DELTA_E,
                    "{ring_name} is only ΔE {distance:.1} from {semantic_name} - a coloured \
                     bracket would read as that token rather than as structure"
                );
            }
        }
    }

    /// The ring is six *independently keyed* tokens a theme file can move one at a time, not six
    /// aliases of one colour - the mistake that would quietly turn this feature back into the flat
    /// single-bracket-colour non-solution GitHub issue #168 explicitly rejected.
    #[test]
    fn the_six_ring_tokens_are_six_real_independently_keyed_colours() {
        let keys: std::collections::HashSet<&str> =
            ring_tokens().into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys.len(), 6, "six distinct registered keys");
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let values: std::collections::HashSet<[u32; 4]> = ring_tokens()
                .into_iter()
                .map(|(_, token)| {
                    let color = token.resolve();
                    [
                        color.r.to_bits(),
                        color.g.to_bits(),
                        color.b.to_bits(),
                        color.a.to_bits(),
                    ]
                })
                .collect();
            assert_eq!(values.len(), 6, "six distinct colours in {}", def.name);
        }
    }
}

#[cfg(test)]
mod syntax_identifier_palette_tests {
    use super::*;

    struct ResetThemeOnDrop;

    impl Drop for ResetThemeOnDrop {
        fn drop(&mut self) {
            set_current_theme(None);
        }
    }

    /// Installs a real bundled theme exactly the way selecting its card does.
    fn with_bundled_theme(name: &str) -> ResetThemeOnDrop {
        let palette = crate::settings::custom_theme::compile_palette_by_name(name, &[])
            .expect("a bundled theme must compile");
        set_current_theme(palette.map(Rc::new));
        ResetThemeOnDrop
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    /// The three tokens this pass gave real colours to.
    fn identifier_tokens() -> [(&'static str, ColorToken); 3] {
        [
            ("VARIABLE", syntax::VARIABLE),
            ("VARIABLE_PARAMETER", syntax::VARIABLE_PARAMETER),
            ("PROPERTY", syntax::PROPERTY),
        ]
    }

    /// CIE Lab, for a real perceptual distance rather than a raw RGB one - the same measure the
    /// three colours were chosen against (see [`syntax`]'s own module docs). sRGB D65, the
    /// standard conversion.
    pub(super) fn lab(color: Rgba) -> (f32, f32, f32) {
        fn linear(component: f32) -> f32 {
            if component <= 0.04045 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        }
        fn pivot(t: f32) -> f32 {
            if t > 0.008856 {
                t.cbrt()
            } else {
                7.787 * t + 16.0 / 116.0
            }
        }
        let (r, g, b) = (linear(color.r), linear(color.g), linear(color.b));
        let x = pivot((0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047);
        let y = pivot(0.2126 * r + 0.7152 * g + 0.0722 * b);
        let z = pivot((0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883);
        (116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z))
    }

    /// Perceptual distance. ~2.3 is the just-noticeable difference; this module requires far more.
    pub(super) fn delta_e(a: Rgba, b: Rgba) -> f32 {
        let (la, aa, ba) = lab(a);
        let (lb, ab, bb) = lab(b);
        ((la - lb).powi(2) + (aa - ab).powi(2) + (ba - bb).powi(2)).sqrt()
    }

    /// The real headline fix: none of the three is plain text any more, in any bundled theme.
    #[test]
    fn no_identifier_token_renders_as_plain_text_in_any_bundled_theme() {
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let text = syntax::TEXT.resolve();
            for (name, token) in identifier_tokens() {
                assert!(
                    !same(token.resolve(), text),
                    "{name} resolves to exactly syntax::TEXT's own colour in {} - plain \
                     identifiers, parameters and property access would all render as \
                     undifferentiated plain text again. If this fired after a change to the \
                     defaults, the five generated theme files almost certainly need regenerating \
                     (see crate::settings::builtin_themes).",
                    def.name
                );
            }
        }
    }

    /// Not merely *different* from plain text, but perceptibly so - a one-hex-digit difference
    /// would pass the check above while still looking identical on screen.
    #[test]
    fn every_identifier_token_is_perceptibly_different_from_plain_text() {
        // Chosen against the ~2.3 just-noticeable threshold with a real margin; the tightest of
        // the three in Jerry Dark measures ~18.
        const MIN_DELTA_E: f32 = 10.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let text = syntax::TEXT.resolve();
            for (name, token) in identifier_tokens() {
                let distance = delta_e(token.resolve(), text);
                assert!(
                    distance >= MIN_DELTA_E,
                    "{name} is only ΔE {distance:.1} from plain text in {} - below the \
                     {MIN_DELTA_E} floor, so it would still read as undifferentiated grey",
                    def.name
                );
            }
        }
    }

    /// The three are real, separate colours from each other too - a parameter is distinguishable
    /// from an ordinary local, and a property from both.
    #[test]
    fn the_three_identifier_tokens_are_perceptibly_distinct_from_each_other() {
        const MIN_DELTA_E: f32 = 10.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let tokens = identifier_tokens();
            for (index, (name_a, token_a)) in tokens.iter().enumerate() {
                for (name_b, token_b) in tokens.iter().skip(index + 1) {
                    let distance = delta_e(token_a.resolve(), token_b.resolve());
                    assert!(
                        distance >= MIN_DELTA_E,
                        "{name_a} and {name_b} are only ΔE {distance:.1} apart in {} - below the \
                         {MIN_DELTA_E} floor",
                        def.name
                    );
                }
            }
        }
    }

    /// And they don't crowd any hue this palette had already claimed - the constraint the three
    /// colours were actually picked under (see [`syntax`]'s own module docs).
    #[test]
    fn no_identifier_token_crowds_an_already_claimed_syntax_hue() {
        // Deliberately looser than the ΔE 16 the colours were chosen at in Jerry Dark: the five
        // derived themes compress the palette's own saturation/lightness range by design, so
        // every gap narrows a little under them. This still catches a genuine collision.
        const MIN_DELTA_E: f32 = 8.0;
        let claimed: [(&str, ColorToken); 8] = [
            ("KEYWORD", syntax::KEYWORD),
            ("FUNCTION", syntax::FUNCTION),
            ("TYPE", syntax::TYPE),
            ("CONSTANT", syntax::CONSTANT),
            ("STRING", syntax::STRING),
            ("ATTRIBUTE", syntax::ATTRIBUTE),
            ("COMMENT", syntax::COMMENT),
            ("EMPHASIS", syntax::EMPHASIS),
        ];
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            for (name, token) in identifier_tokens() {
                for (claimed_name, claimed_token) in claimed {
                    let distance = delta_e(token.resolve(), claimed_token.resolve());
                    assert!(
                        distance >= MIN_DELTA_E,
                        "{name} is only ΔE {distance:.1} from {claimed_name} in {} - it would \
                         read as that colour rather than its own",
                        def.name
                    );
                }
            }
        }
    }

    /// The maintainer's own scope line for this pass, pinned as a test: operators, brackets,
    /// delimiters and interpolation regions deliberately still render exactly as plain text.
    ///
    /// GitHub issue #168's bracket-pair colouring landed **without** relaxing this, which is the
    /// whole point of keeping it: the rainbow lives in its own six
    /// [`syntax::BRACKET_1`]..[`syntax::BRACKET_6`] tokens, and `PUNCTUATION_BRACKET` stays plain
    /// as the real fallback an *unmatched* bracket keeps. If a future change ever "implements
    /// bracket colouring" by giving this one token a hue instead, that is the flat single-colour
    /// non-solution issue #168 explicitly rejected, and this is what catches it.
    #[test]
    fn operators_and_punctuation_deliberately_still_render_as_plain_text() {
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let text = syntax::TEXT.resolve();
            for (name, token) in [
                ("OPERATOR", syntax::OPERATOR),
                ("PUNCTUATION_BRACKET", syntax::PUNCTUATION_BRACKET),
                ("PUNCTUATION_DELIMITER", syntax::PUNCTUATION_DELIMITER),
                ("EMBEDDED", syntax::EMBEDDED),
            ] {
                assert!(
                    same(token.resolve(), text),
                    "{name} no longer matches syntax::TEXT in {} - colouring operators and \
                     punctuation flatly is deliberately out of scope; bracket-pair colouring is \
                     done through syntax::BRACKET_1..BRACKET_6 instead, and PUNCTUATION_BRACKET \
                     has to stay plain to remain a usable fallback for an unmatched bracket",
                    def.name
                );
            }
        }
    }
}
