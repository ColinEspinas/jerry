//! Jerry's design tokens: every colour and dimension the UI paints, defined once here. What
//! each group is *for*, and the rules that constrain it, is `docs/design/`.

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
    ("terminal", terminal::TOKENS),
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
    ("changes", changes::TOKENS),
    ("budget", budget::TOKENS),
    ("status_bar", status_bar::TOKENS),
    ("notes", notes::TOKENS),
    ("history", history::TOKENS),
    ("search", search::TOKENS),
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

/// Real OKLCH colour maths - the perceptual space this palette is authored and derived in.
mod oklch {
    use super::Rgba;

    fn to_linear(component: f32) -> f32 {
        if component <= 0.04045 {
            component / 12.92
        } else {
            ((component + 0.055) / 1.055).powf(2.4)
        }
    }

    fn from_linear(component: f32) -> f32 {
        if component <= 0.003_130_8 {
            12.92 * component
        } else {
            1.055 * component.powf(1.0 / 2.4) - 0.055
        }
    }

    /// `(L, C, H)` - `L` and `C` in OKLab's own 0..~1 and 0..~0.4 ranges, `H` in degrees 0..360.
    pub(super) fn of(color: Rgba) -> (f32, f32, f32) {
        let (r, g, b) = (to_linear(color.r), to_linear(color.g), to_linear(color.b));
        let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
        let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
        let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
        let lightness = 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s;
        let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
        let b_axis = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
        let chroma = (a * a + b_axis * b_axis).sqrt();
        let hue = b_axis.atan2(a).to_degrees().rem_euclid(360.0);
        (lightness, chroma, hue)
    }

    /// The linear-light sRGB triple for an OKLCH colour, **unclamped** - a negative or >1 component
    /// is exactly how [`rgba_from_oklch`] detects that a colour is outside the sRGB gamut.
    fn to_linear_rgb(lightness: f32, chroma: f32, hue: f32) -> (f32, f32, f32) {
        let a = chroma * hue.to_radians().cos();
        let b = chroma * hue.to_radians().sin();
        let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
        let m = (lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
        let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
        (
            4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
            -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
            -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
        )
    }

    fn in_gamut(lightness: f32, chroma: f32, hue: f32) -> bool {
        let (r, g, b) = to_linear_rgb(lightness, chroma, hue);
        const EPSILON: f32 = 1e-4;
        [r, g, b]
            .into_iter()
            .all(|component| (-EPSILON..=1.0 + EPSILON).contains(&component))
    }

    /// A real sRGB colour for an OKLCH triple, **gamut-mapped by reducing chroma only**.
    pub(super) fn to_rgba(lightness: f32, chroma: f32, hue: f32, alpha: f32) -> Rgba {
        let lightness = lightness.clamp(0.0, 1.0);
        let chroma = chroma.max(0.0);
        let usable = if in_gamut(lightness, chroma, hue) {
            chroma
        } else {
            let (mut low, mut high) = (0.0f32, chroma);
            for _ in 0..32 {
                let mid = 0.5 * (low + high);
                if in_gamut(lightness, mid, hue) {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            low
        };
        let (r, g, b) = to_linear_rgb(lightness, usable, hue);
        Rgba {
            r: from_linear(r.clamp(0.0, 1.0)),
            g: from_linear(g.clamp(0.0, 1.0)),
            b: from_linear(b.clamp(0.0, 1.0)),
            a: alpha,
        }
    }
}

/// The OKLCH `(L, C, H)` of a real colour - see the [`oklch`] module for why this space.
pub fn oklch_of(color: Rgba) -> (f32, f32, f32) {
    oklch::of(color)
}

/// A real sRGB colour from an OKLCH triple, gamut-mapped by reducing chroma only - see
/// [`oklch::to_rgba`].
pub fn rgba_from_oklch(lightness: f32, chroma: f32, hue: f32, alpha: f32) -> Rgba {
    oklch::to_rgba(lightness, chroma, hue, alpha)
}

/// The shortest angular distance between two hues, in degrees (0..=180) - hue is circular, so a
/// plain subtraction is wrong across the 0/360 wrap.
pub fn hue_distance(a: f32, b: f32) -> f32 {
    let difference = (a - b).abs().rem_euclid(360.0);
    difference.min(360.0 - difference)
}

/// A real, systematic OKLCH transform - see [`derive_shift`]'s own docs for how one is computed,
/// [`apply_shift`] for how it's applied to a single colour, and [`derived_palette`] for the real
/// whole-palette generation both of this module's remaining callers use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OklchShift {
    /// Added to hue, in **degrees** (wraps via `rem_euclid(360.0)`).
    pub hue: f32,
    /// Multiplies perceptual chroma. Not clamped to a maximum here: a chroma too high for sRGB at
    /// the resulting lightness is gamut-mapped by [`oklch::to_rgba`], which reduces chroma while
    /// holding lightness and hue exactly.
    pub chroma_scale: f32,
    /// `new_lightness = old_lightness * lightness_scale + lightness_offset` - a linear remap, not
    /// a plain additive shift, so a light theme (`Paper`) can be derived from Jerry Dark's own
    /// near-black baseline without every already-light token clipping. Clamped to `0.0..=1.0` in
    /// [`apply_shift`].
    pub lightness_scale: f32,
    pub lightness_offset: f32,
}

/// The no-op shift - what [`derive_shift`] returns for a target identical to its base.
pub const IDENTITY_SHIFT: OklchShift = OklchShift {
    hue: 0.0,
    chroma_scale: 1.0,
    lightness_scale: 1.0,
    lightness_offset: 0.0,
};

/// Applies `shift` to `base` in OKLCH: rotate hue, scale chroma, remap lightness, then gamut-map
/// back into sRGB by reducing chroma only ([`oklch::to_rgba`]).
pub fn apply_shift(base: Rgba, shift: OklchShift) -> Rgba {
    let (lightness, chroma, hue) = oklch::of(base);
    oklch::to_rgba(
        (lightness * shift.lightness_scale + shift.lightness_offset).clamp(0.0, 1.0),
        (chroma * shift.chroma_scale).max(0.0),
        (hue + shift.hue).rem_euclid(360.0),
        base.a,
    )
}

/// Derives a real, systematic [`OklchShift`] from two themes' own five `[background, panel,
/// green-ish, amber-ish, blue-ish]` swatches - the mechanism the five migrated built-in theme
/// files are generated with (`crate::settings::builtin_themes`, which pins each theme's own
/// original swatches) and the one an imported VSCode theme's whole-app chrome still goes through
/// (`crate::settings::vscode_theme`) for the many tokens no VSCode colour key maps onto:
pub fn derive_shift(base_swatches: [u32; 5], target_swatches: [u32; 5]) -> OklchShift {
    fn oklch_of_hex(hex_value: u32) -> (f32, f32, f32) {
        oklch::of(hex_rgba(hex_value))
    }

    let base: Vec<(f32, f32, f32)> = base_swatches.into_iter().map(oklch_of_hex).collect();
    let target: Vec<(f32, f32, f32)> = target_swatches.into_iter().map(oklch_of_hex).collect();

    // Lightness: an exact linear fit through the two background-ish swatches (index 0, 1).
    let (base_bg, base_panel) = (base[0].0, base[1].0);
    let (target_bg, target_panel) = (target[0].0, target[1].0);
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
        let delta = (target[index].2 - base[index].2).to_radians();
        sin_sum += delta.sin();
        cos_sum += delta.cos();
    }
    let hue = sin_sum.atan2(cos_sum).to_degrees().rem_euclid(360.0);

    // Chroma: mean ratio across the same three chromatic swatches.
    let mut ratio_sum = 0.0f32;
    let mut ratio_count = 0.0f32;
    for index in 2..5 {
        if base[index].1 > 0.001 {
            ratio_sum += target[index].1 / base[index].1;
            ratio_count += 1.0;
        }
    }
    let chroma_scale = if ratio_count > 0.0 {
        (ratio_sum / ratio_count).clamp(0.0, 3.0)
    } else {
        1.0
    };

    OklchShift {
        hue,
        chroma_scale,
        lightness_scale,
        lightness_offset,
    }
}

/// Jerry Dark's own real accent blue ([`syntax::FUNCTION`]/`#74ade8`) - the reference hue
/// [`shift_from_seed`] rotates a user's seed colour against. Pinned here as the one place that
/// choice is made, rather than repeated at each caller.
const SEED_REFERENCE_ACCENT: u32 = 0x74ade8;

/// Derives an [`OklchShift`] from a single seed colour - the real maths behind the Themes page's
/// "Generate from colour" action (GitHub issue #141).
pub fn shift_from_seed(seed: Rgba) -> OklchShift {
    let (_, seed_chroma, seed_hue) = oklch::of(seed);
    let (_, reference_chroma, reference_hue) = oklch::of(hex_rgba(SEED_REFERENCE_ACCENT));
    let chroma_scale = if reference_chroma > 0.001 {
        (seed_chroma / reference_chroma).clamp(0.0, 3.0)
    } else {
        1.0
    };
    OklchShift {
        hue: (seed_hue - reference_hue).rem_euclid(360.0),
        chroma_scale,
        lightness_scale: 1.0,
        lightness_offset: 0.0,
    }
}

/// The WCAG 2.x contrast ratio between two real colours - order-independent.
pub fn contrast_ratio(a: Rgba, b: Rgba) -> f32 {
    fn relative_luminance(color: Rgba) -> f32 {
        fn channel(component: f32) -> f32 {
            if component <= 0.04045 {
                component / 12.92
            } else {
                ((component + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }
    let (first, second) = (relative_luminance(a), relative_luminance(b));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// The real WCAG floor a given syntax token has to clear against its own theme's editor
/// background, or `None` for a token this guard deliberately leaves alone.
fn syntax_contrast_floor(key: &str) -> Option<f32> {
    let scope = key.strip_prefix("syntax.")?;
    match scope {
        // Real backgrounds and non-colour tokens - a floor against the background is meaningless.
        "diagnostic_row_bg" => None,
        "operator" | "punctuation_bracket" | "punctuation_delimiter" | "punctuation_special" => {
            Some(3.0)
        }
        _ if scope.starts_with("bracket_") => Some(3.0),
        _ => Some(4.5),
    }
}

/// [`syntax_contrast_floor`], exposed for the one test that has to assert the same floors against
/// the real checked-in theme *files* rather than against a generated palette - see
/// `crate::settings::builtin_themes::tests::every_bundled_theme_file_clears_its_syntax_contrast_floors`.
pub fn syntax_contrast_floor_for_test(key: &str) -> Option<f32> {
    syntax_contrast_floor(key)
}

/// Pushes any syntax colour that lands below its [`syntax_contrast_floor`] away from `background`
/// in OKLCH lightness until it clears - holding hue and chroma, so the colour keeps its identity
/// and only its lightness moves.
fn enforce_syntax_contrast_floors(palette: &mut [(&'static str, Rgba)]) {
    let Some(background) = palette
        .iter()
        .find(|(key, _)| *key == "surface.center")
        .map(|(_, color)| *color)
    else {
        return;
    };
    // Which way is "away from the background"? On a dark theme foregrounds get lighter; on a light
    // one they get darker. Decided from the background itself rather than from each token, so a
    // whole palette moves consistently.
    let (background_lightness, _, _) = oklch::of(background);
    let lighten = background_lightness < 0.5;

    for (key, color) in palette.iter_mut() {
        let Some(floor) = syntax_contrast_floor(key) else {
            continue;
        };
        *color = pushed_to_clear_floor(*color, background, floor, lighten);
    }
}

/// The one real "move this colour away from that background until it clears `floor`" step both
/// contrast guards share - [`enforce_syntax_contrast_floors`] and
/// [`enforce_terminal_foreground_contrast`].
fn pushed_to_clear_floor(color: Rgba, background: Rgba, floor: f32, lighten: bool) -> Rgba {
    // Measured on the **quantized** colour, not the float one. A theme file stores `#rrggbb`, so an
    // 8-bit rounding step is applied to whatever this produces; searching in float space and
    // ignoring that lands colours a hundredth of a ratio point under the floor, which is a real
    // failure of a real assertion rather than a rounding nicety.
    let quantize = |candidate: Rgba| -> Rgba {
        Rgba {
            r: (candidate.r * 255.0).round() / 255.0,
            g: (candidate.g * 255.0).round() / 255.0,
            b: (candidate.b * 255.0).round() / 255.0,
            a: candidate.a,
        }
    };
    // A hair above the floor, not exactly on it. Landing a colour on 4.4999 is a real assertion
    // failure, and the difference between an f32 ratio computed here and an f64 one computed by any
    // external checker is comfortably inside that margin.
    let target = floor * 1.005;
    let clears = |candidate: Rgba| contrast_ratio(quantize(candidate), background) >= target;
    if clears(color) {
        return color;
    }

    let (lightness, chroma, hue) = oklch::of(color);
    let limit = if lighten { 1.0 } else { 0.0 };
    let mut best = oklch::to_rgba(limit, chroma, hue, color.a);
    if !clears(best) {
        // Even pure white/black cannot clear it - take the extreme and let the theme's own
        // validation report the palette as unreadable rather than silently pretending.
        return best;
    }
    // Binary-search the smallest move that clears the floor: contrast is monotonic in lightness
    // once we are moving away from the background, so this converges.
    let (mut low, mut high) = (lightness, limit);
    for _ in 0..24 {
        let mid = 0.5 * (low + high);
        let candidate = oklch::to_rgba(mid, chroma, hue, color.a);
        if clears(candidate) {
            best = candidate;
            high = mid;
        } else {
            low = mid;
        }
    }
    best
}

/// The same guard [`enforce_syntax_contrast_floors`] applies to code, applied to unstyled terminal
/// output - `terminal.foreground` against `terminal.background` rather than against
/// `surface.center`, because that is the surface it is actually painted on.
fn enforce_terminal_foreground_contrast(palette: &mut [(&'static str, Rgba)]) {
    let Some(background) = palette
        .iter()
        .find(|(key, _)| *key == terminal::BACKGROUND.key)
        .map(|(_, color)| *color)
    else {
        return;
    };
    let (background_lightness, _, _) = oklch::of(background);
    let lighten = background_lightness < 0.5;

    for (key, color) in palette.iter_mut() {
        if *key == terminal::FOREGROUND.key {
            *color = pushed_to_clear_floor(*color, background, 4.5, lighten);
        }
    }
}

/// Runs `shift` over **every** real registered token ([`TOKEN_GROUPS`]) and hands back the whole
/// resulting palette as real, literal `(key, colour)` pairs in registry order - the one shared
/// generator behind both the built-in theme migration (`crate::settings::builtin_themes`) and the
/// "generate a theme from one colour" action, so those two can never derive palettes differently.
pub fn derived_palette(shift: OklchShift) -> Vec<(&'static str, Rgba)> {
    let mut palette: Vec<(&'static str, Rgba)> = all_tokens()
        .map(|token| (token.key, apply_shift(token.default, shift)))
        .collect();
    enforce_syntax_contrast_floors(&mut palette);
    pin_terminal_ansi_palette(&mut palette);
    enforce_terminal_foreground_contrast(&mut palette);
    palette
}

/// Replaces the sixteen derived `terminal.ansi.*` entries with a real, *authored* ANSI palette -
/// [`terminal::ANSI`]'s own defaults for a dark derived theme, [`terminal::LIGHT_ANSI`] for a light
/// one, decided from the palette's own derived `surface.window` via [`theme_is_light`].
fn pin_terminal_ansi_palette(palette: &mut [(&'static str, Rgba)]) {
    let Some(window) = palette
        .iter()
        .find(|(key, _)| *key == "surface.window")
        .map(|(_, color)| *color)
    else {
        return;
    };
    let light = theme_is_light(window);
    for (key, color) in palette.iter_mut() {
        let Some(index) = key
            .strip_prefix("terminal.ansi.")
            .and_then(|digits| digits.parse::<usize>().ok())
        else {
            continue;
        };
        *color = if light {
            hex_rgba(terminal::LIGHT_ANSI[index])
        } else {
            terminal::ANSI[index].default
        };
    }
}
/// Backgrounds - every solid fill in the app, from the window itself down to popovers,
/// hover states and keycaps.
pub mod surface {
    use super::{token, ColorToken};

    pub const WINDOW: ColorToken = token("surface.window", 0x0e0f11); // window body
    pub const WINDOW_BORDER: ColorToken = token("surface.window_border", 0x262a2e);
    pub const TITLE_BAR: ColorToken = token("surface.title_bar", 0x101214);
    pub const RAIL: ColorToken = token("surface.rail", 0x101113); // left rail + right panel
    /// The sidebar strip's own recessed band (GitHub issue #291) - the 36px view switcher above
    /// the rail. Deliberately **darker than [`RAIL`]**, and that is the whole reason it is its own
    /// token rather than a reuse of [`WINDOW`] or [`PTY`]: a tab only reads as connected if the
    /// strip behind it is **darker than the panel**. A strip sharing the rail's own background
    /// makes the selected cell float instead of sitting in the strip, so this is two steps below
    /// [`RAIL`], which the selected cell itself uses.
    pub const SIDEBAR_STRIP: ColorToken = token("surface.sidebar_strip", 0x0a0b0d);
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
    /// The hint-size keycap's own background - distinct from [`KEYCAP`]'s standard-size value.
    pub const KEYCAP_HINT: ColorToken = token("surface.keycap_hint", 0x15181a);
    pub const CHIP_NEUTRAL: ColorToken = token("surface.chip_neutral", 0x23272b);
    pub const CURRENT_LINE: ColorToken = token("surface.current_line", 0x181c20);
    /// The Changes panel's Runs section - pinned to the panel's own bottom in its own capped,
    /// independently-scrolled well, distinct from the lighter [`HEADER`] the other three sections'
    /// shared scroller sits on.
    pub const RUNS_WELL: ColorToken = token("surface.runs_well", 0x0f1113);
    /// The Windows/Linux title bar's close caption button's hover fill. The original design
    /// spec'd a muted maroon; Colin asked for the real Windows Fluent Design close-hover red
    /// (`#E81123`, the same color Windows 10/11's own native title bar uses) instead - a
    /// deliberate override, not a stale-spec bug.
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
    /// The same row hover, for a **destructive** menu row (GitHub issue #290). The destructive
    /// tint is carried on the hover fill too, not on the resting label alone. Without it, `Delete` and
    /// `Copy Path` are visually identical the moment the pointer is on either of them, which is
    /// exactly when the click is about to happen.
    pub const MENU_ROW_HOVER_DESTRUCTIVE: ColorToken =
        token("surface.menu_row_hover_destructive", 0x2a1719);
    /// A file tab's close-affordance hover fill - one hex step off [`CHIP_NEUTRAL`]
    /// (`#23272b`), kept as its own token.
    pub const TAB_CLOSE_HOVER: ColorToken = token("surface.tab_close_hover", 0x23282c);
    /// The Hover/Diagnostic popover footer's own band background, shared by both cards'
    /// `source · code`/`F12 definition` footer rows - one hex step darker than [`CARD_SUNK`]
    /// (`#131619`, used for every *other* card footer in the app), not a duplicate of it: the
    /// mockup genuinely uses two adjacent-but-different footer tones, and this app's own contrast
    /// tests already pin `CARD_SUNK`'s exact value elsewhere, so reusing it here would have
    /// silently painted the wrong one of the two.
    pub const LSP_POPOVER_FOOTER: ColorToken = token("surface.lsp_popover_footer", 0x141719);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("WINDOW", WINDOW),
        ("WINDOW_BORDER", WINDOW_BORDER),
        ("TITLE_BAR", TITLE_BAR),
        ("RAIL", RAIL),
        ("SIDEBAR_STRIP", SIDEBAR_STRIP),
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
        ("MENU_ROW_HOVER_DESTRUCTIVE", MENU_ROW_HOVER_DESTRUCTIVE),
        ("TAB_CLOSE_HOVER", TAB_CLOSE_HOVER),
        ("LSP_POPOVER_FOOTER", LSP_POPOVER_FOOTER),
        ("RUNS_WELL", RUNS_WELL),
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
    /// The Diagnostic popover's own border. Paired with
    /// [`super::syntax::DIAGNOSTIC_ROW_BG`] for that card's background - together they give the
    /// Diagnostic popover the design's own red-tinted chrome, distinct from the Hover/Completions
    /// popovers' neutral [`POPOVER`] - see `crate::code_surface::lsp_ui::render_diagnostic_card_content`.
    /// Lives here rather than in `syntax` (despite pairing with a `syntax.*` token) because it's a
    /// border, not a syntax-highlighted foreground color: `syntax`'s own contrast-floor enforcement
    /// (`enforce_syntax_contrast_floors`) requires every `syntax.*` token to clear a 4.5:1 ratio
    /// against the code background, which is the *text-readability* bar - wrong for a deliberately
    /// subtle card outline, and it would silently push this exact hex away from the design's own
    /// value under a derived theme.
    pub const DIAGNOSTIC_CARD: ColorToken = token("border.diagnostic_card", 0x3a2224);
    /// The Diagnostic popover's own footer band's top border - `#2b2224` in the mockup, a real,
    /// deliberately different shade from [`DIAGNOSTIC_CARD`]'s outer `#3a2224` (the mockup uses
    /// two distinct border tones on the same card: a stronger one for the whole card's outline, a
    /// subtler one for the internal seam above the footer) - see
    /// `crate::code_surface::lsp_ui::render_diagnostic_card_content`.
    pub const DIAGNOSTIC_CARD_FOOTER: ColorToken = token("border.diagnostic_card_footer", 0x2b2224);

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
        ("DIAGNOSTIC_CARD", DIAGNOSTIC_CARD),
        ("DIAGNOSTIC_CARD_FOOTER", DIAGNOSTIC_CARD_FOOTER),
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
    /// Formerly the guide colour for a level in the selected/open file's ancestor chain, defaulting
    /// to [`super::border::SELECTED_EDGE`]'s own value (`#3f5b74`). GitHub issue #406: the file
    /// tree's indent guides no longer ever paint this - they always render
    /// [`INDENT_GUIDE`], regardless of selection, focus, or which file is open, per explicit
    /// product feedback that a coloured guide read as wrong even when nothing was actually
    /// selected. Left defined (rather than removed) purely so a custom theme file that still sets
    /// `tree.indent_guide_active` keeps loading instead of failing on an unknown key; it has no
    /// remaining effect on anything painted.
    pub const INDENT_GUIDE_ACTIVE: ColorToken = token("tree.indent_guide_active", 0x3f5b74);

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
    /// The colour a hovered glyph lifts to in a surface whose *background* already encodes
    /// selection - today the sidebar strip's cells (GitHub issue #291).
    pub const GLYPH_HOVER: ColorToken = token("text.glyph_hover", 0xc8ced4);

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
        ("GLYPH_HOVER", GLYPH_HOVER),
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

/// Tokens the rail needs that have no exact match elsewhere in this module - every other colour
/// it calls for (the branch/note/model/activity greys, the amber flag, the spine/selection edges)
/// already has one, reused directly at the call site rather than duplicated here under a second
/// name. See `docs/design/rail.md`.
pub mod rail {
    use super::{token, ColorToken};

    /// Repo group header's uppercase name.
    pub const REPO_HEADER_NAME: ColorToken = token("rail.repo_header_name", 0x9aa1a8);
    /// Active worktree row header background (§2.2: "Active worktree header background
    /// `#181c1f`").
    pub const WORKTREE_ACTIVE_BG: ColorToken = token("rail.worktree_active_bg", 0x181c1f);
    /// Worktree row hover background (§2.2: "hover `#16191c`").
    pub const WORKTREE_HOVER_BG: ColorToken = token("rail.worktree_hover_bg", 0x16191c);
    /// An agent row's title, one level below its parent worktree's branch name. Deliberately
    /// dimmer than [`super::text::STRONG`], which the branch above it uses: fix hierarchy by
    /// shrinking the child, never by growing the parent.
    /// A `crate::rail::Status::Idle` agent drops further still, to [`super::text::DIMMER`].
    pub const AGENT_TITLE: ColorToken = token("rail.agent_title", 0xa3a9b0);
    /// The repo header's amber urgency **count** - the number beside the [`super::status::ASK`]
    /// dot, as in `● 2` for needs input.
    pub const REPO_ASK_COUNT: ColorToken = token("rail.repo_ask_count", 0xc99b4e);
    /// The repo header's red urgency **count** - the number beside the [`super::status::FAIL`]
    /// dot (§4: "`● 1` red (`#e0625c` dot, `#c4726d` text, failed)"). Its own key, for the reason
    /// [`Self::REPO_ASK_COUNT`]'s docs give.
    pub const REPO_FAIL_COUNT: ColorToken = token("rail.repo_fail_count", 0xc4726d);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("REPO_HEADER_NAME", REPO_HEADER_NAME),
        ("WORKTREE_ACTIVE_BG", WORKTREE_ACTIVE_BG),
        ("WORKTREE_HOVER_BG", WORKTREE_HOVER_BG),
        ("AGENT_TITLE", AGENT_TITLE),
        ("REPO_ASK_COUNT", REPO_ASK_COUNT),
        ("REPO_FAIL_COUNT", REPO_FAIL_COUNT),
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
pub mod syntax {
    use super::{token, ColorToken};

    pub const TEXT: ColorToken = token("syntax.text", 0xacb2bc);
    pub const KEYWORD: ColorToken = token("syntax.keyword", 0xc194d6);
    pub const FUNCTION: ColorToken = token("syntax.function", 0x74ade8);
    /// `function.method` (`tree-sitter-rust`'s `@function.method`, `-javascript`'s own) - see the
    /// module docs' fallback-chain section.
    pub const FUNCTION_METHOD: ColorToken = token("syntax.function_method", 0x74ade8);
    /// `function.definition` - a function or method's name **where it is declared**, the one
    /// place the reader learns where a name comes from. See
    /// `crate::code_surface::code_view`'s own `RUST_DEFINITION_SUPPLEMENT` for the real query
    /// rules that separate this from a call site (no bundled grammar query does).
    pub const FUNCTION_DEFINITION: ColorToken = token("syntax.function_definition", 0xa19fe8);
    pub const TYPE: ColorToken = token("syntax.type", 0xc7a356);
    /// `type.builtin` (`tree-sitter-rust`'s `(primitive_type) @type.builtin`, `-typescript`'s
    /// `(predefined_type) @type.builtin`) - see the module docs' fallback-chain section.
    pub const TYPE_BUILTIN: ColorToken = token("syntax.type_builtin", 0xc7a356);
    /// `constant` (an all-caps identifier, per every one of this app's four grammars' own naming
    /// convention heuristic) - the same value [`LITERAL`] used to carry before this module split
    /// the old six-bucket "Literal" classification into its real, individually-scoped captures.
    pub const CONSTANT: ColorToken = token("syntax.constant", 0xde946b);
    /// `constant.builtin` (`true`/`false`/`None`/`undefined`/an integer or float literal - Rust
    /// and JavaScript/TypeScript both route numeric/boolean literals through this real capture
    /// name rather than a plain `number`) - see the module docs' fallback-chain section.
    pub const CONSTANT_BUILTIN: ColorToken = token("syntax.constant_builtin", 0xde946b);
    /// `string` (`(string_literal) @string`, `(template_string) @string`, ...) - a real, distinct
    /// hue from [`CONSTANT`] (unlike the replaced six-bucket palette, which lumped every literal
    /// together) so a string reads apart from a number at a glance.
    pub const STRING: ColorToken = token("syntax.string", 0x98b46a);
    /// `string.escape` - registered under both this checklist name and the real capture name every
    /// one of this app's grammars that supports escapes actually emits, plain `escape`
    /// (`tree-sitter-rust`'s `(escape_sequence) @escape`, `-python`'s own identical rule; neither
    /// JavaScript's nor TypeScript's own bundled query captures string escapes at all, verified
    /// directly against their real `queries/highlights.scm` - so this bucket is genuinely reachable
    /// for Rust/Python source only). A brighter tint of [`STRING`] rather than a plain alias: an
    /// escape sequence is a real, deliberately-distinct sub-token within a string, not a fallback
    /// case.
    pub const STRING_ESCAPE: ColorToken = token("syntax.string_escape", 0xbddb8e);
    /// `number` (`-python`'s `[(integer)(float)] @number`, `-javascript`'s `(number) @number`;
    /// Rust has no separate `number` capture at all - its own numeric literals arrive as
    /// `@constant.builtin` instead, see [`CONSTANT_BUILTIN`]). Defaults to [`CONSTANT`]'s value:
    /// both are numeric-literal buckets under a different grammar's own naming choice, and keeping
    /// them visually identical is what makes "a number looks like a number" consistent regardless
    /// of which of the four languages produced it.
    pub const NUMBER: ColorToken = token("syntax.number", 0xde946b);
    pub const COMMENT: ColorToken = token("syntax.comment", 0x7a818a);
    /// `comment.doc` - registered under both this checklist name and the real capture name
    /// `tree-sitter-rust`'s own query actually emits, `comment.documentation`
    /// (`(line_comment (doc_comment)) @comment.documentation`); none of this app's other three
    /// grammars has a doc-comment concept in their bundled query. A brighter tint of [`COMMENT`]
    /// (not a plain alias) so a `///` doc comment reads as more prominent than an ordinary `//`
    /// one, the same real distinction most editors make.
    pub const COMMENT_DOC: ColorToken = token("syntax.comment_doc", 0x8c939c);
    /// GitHub issue #200's own doc-comment tag bucket - a JSDoc-style `@param`/`@returns`/
    /// `@example` block tag, or a `{@link ...}`-style inline tag, within an already-[`COMMENT_DOC`]
    /// comment. Not a real tree-sitter capture (no bundled grammar this app ships parses *inside*
    /// a comment body at all - see `crate::code_surface::code_view::doc_tag_ranges`'s own docs for
    /// the plain text scan that finds these instead), so it never appears in [`HIGHLIGHT_NAMES`].
    /// Shares [`KEYWORD`]'s own purple rather than inventing a new hue: a doc tag really is playing
    /// a keyword's role (a fixed, structural vocabulary word) just inside prose instead of code,
    /// the same reasoning [`EMPHASIS`] gives for reusing that same purple in Markdown italics.
    pub const COMMENT_DOC_TAG: ColorToken = token("syntax.comment_doc_tag", 0xc194d6);
    /// `variable` - a real, live-classified bucket (`-python`'s own blanket `(identifier)
    /// @variable`, `-javascript`'s identical blanket rule). A dusty rose on the shared accent
    /// tier, warm against [`PROPERTY`]'s cool cyan-blue so an `a.b.c` chain reads at a glance.
    pub const VARIABLE: ColorToken = token("syntax.variable", 0xda8db2);
    /// `variable.parameter` (`tree-sitter-rust`'s `(parameter (identifier) @variable.parameter)`,
    /// `-typescript`'s `required_parameter`/`optional_parameter` rules) - [`VARIABLE`]'s own rose
    /// family, deeper and considerably more saturated. A function's inputs are worth picking out
    /// from the locals around them, and staying inside [`VARIABLE`]'s family says "still a
    /// variable, just a distinguished one" rather than inventing an unrelated hue for a closely
    /// related concept. Deeper rather than brighter for a real reason - see the module docs.
    pub const VARIABLE_PARAMETER: ColorToken = token("syntax.variable_parameter", 0xe28c93);
    /// `variable.builtin` (`self`/`this`/`super`/`cls`) - the bucket the replaced six-colour
    /// design table called "literal/self"; defaults to [`CONSTANT`]'s old `LITERAL` value so this
    /// one real, pre-existing visual choice (self-references read like literals here) survives the
    /// split unchanged.
    pub const VARIABLE_BUILTIN: ColorToken = token("syntax.variable_builtin", 0xde946b);
    /// `property` (a field/attribute access - `tree-sitter-rust`'s `(field_identifier) @property`,
    /// `-python`'s `(attribute attribute: (identifier) @property)`, `-javascript`'s
    /// `(property_identifier) @property`) - a muted cyan-blue, deliberately outside [`VARIABLE`]'s
    /// warm family: a field access is not a local binding but a name looked up on another object,
    /// and the warm/cool split is what makes an `a.b.c` chain legible at a glance. See the module
    /// docs' "identifier family" section.
    pub const PROPERTY: ColorToken = token("syntax.property", 0x51b7d8);
    /// `operator` (`+`, `==`, `&&`, ...) - a real, live-classified bucket (previously fell
    /// through unmatched). The one family deliberately held *below* plain [`TEXT`]: being quieter
    /// than the code is the whole job of punctuation, so it sits at `L 0.560` and clears the 3:1
    /// de-emphasized floor rather than the 4.5:1 body-text one.
    pub const OPERATOR: ColorToken = token("syntax.operator", 0x6f757e);
    /// `punctuation.bracket` (`(`/`)`/`[`/`]`/`{`/`}`, and `<`/`>` in a generic-argument position)
    /// - see [`OPERATOR`]'s own docs for why this sits below plain [`TEXT`].
    pub const PUNCTUATION_BRACKET: ColorToken = token("syntax.punctuation_bracket", 0x6f757e);
    /// `punctuation.delimiter` (`,`/`;`/`:`/`.`/`::`) - see [`OPERATOR`]'s own docs.
    pub const PUNCTUATION_DELIMITER: ColorToken = token("syntax.punctuation_delimiter", 0x6f757e);

    /// GitHub issue #168's rotating bracket-pair depth ring, colour 1 of 6 - the colour a real,
    /// *matched* `(`/`[`/`{` pair at nesting depth 0 (and 6, and 12, ...) paints, both halves of
    /// the pair alike. See this module's own "bracket-pair depth ring" section for how these six
    /// were chosen and measured, and
    /// [`crate::code_surface::code_view::colorize_bracket_pairs`] for the real matcher that
    /// decides which brackets reach these buckets at all (an unmatched one keeps
    /// [`PUNCTUATION_BRACKET`]'s de-emphasized tone, which is what makes malformed or mid-edit
    /// code degrade visibly-but-quietly rather than lying about structure).
    pub const BRACKET_1: ColorToken = token("syntax.bracket_1", 0x9f5d72);
    /// Bracket-pair depth ring, colour 2 of 6 (nesting depth 1, 7, ...) - see [`BRACKET_1`].
    pub const BRACKET_2: ColorToken = token("syntax.bracket_2", 0x9b673b);
    /// Bracket-pair depth ring, colour 3 of 6 (nesting depth 2, 8, ...) - see [`BRACKET_1`].
    pub const BRACKET_3: ColorToken = token("syntax.bracket_3", 0x6e7c3c);
    /// Bracket-pair depth ring, colour 4 of 6 (nesting depth 3, 9, ...) - see [`BRACKET_1`].
    pub const BRACKET_4: ColorToken = token("syntax.bracket_4", 0x268676);
    /// Bracket-pair depth ring, colour 5 of 6 (nesting depth 4, 10, ...) - see [`BRACKET_1`].
    pub const BRACKET_5: ColorToken = token("syntax.bracket_5", 0x3d7ba4);
    /// Bracket-pair depth ring, colour 6 of 6 (nesting depth 5, 11, ...) - see [`BRACKET_1`].
    pub const BRACKET_6: ColorToken = token("syntax.bracket_6", 0x7d68a2);
    /// `tag` (a lowercase JSX element name, `-javascript`'s own JSX query) - see the module docs'
    /// fallback-chain section for why this defaults to [`TYPE`]'s value rather than its own hue: it
    /// preserves this module's pre-existing "a JSX element name is coloured like the type it
    /// names" choice unchanged, now through a real, dedicated schema slot instead of folding `tag`
    /// and `type` into one [`crate::code_surface::code_view::HighlightKind`] variant.
    pub const TAG: ColorToken = token("syntax.tag", 0xc7a356);
    /// `attribute` (Rust's `#[derive(...)]`/`#![...]`, `-javascript`'s JSX attribute name query) -
    /// a real, distinct hue (not a fallback) since a decorator/attribute is genuinely unlike
    /// anything else in the six-bucket original palette.
    pub const ATTRIBUTE: ColorToken = token("syntax.attribute", 0x4bbeb1);
    /// `embedded` (the interpolated-expression region of a template string/f-string, e.g.
    /// `` `n=${count}` ``'s `${count}` or an f-string's `{value}`) - defaults to [`TEXT`]'s
    /// value. The
    /// interpolated expression's own tokens (identifiers, calls, numbers, ...) already get their
    /// own, more specific captures that win over this one by nesting (see
    /// [`crate::code_surface::code_view`]'s own "`HighlightStart`s nest" docs), so this bucket is
    /// only ever visible for the rare leftover byte inside an interpolation no more specific
    /// capture covers - not worth a colour of its own.
    pub const EMBEDDED: ColorToken = token("syntax.embedded", 0xacb2bc);

    /// GitHub issue #104's own real, prose-specific buckets - Markdown's `text.title`/
    /// `text.uri`/`text.reference`/`text.emphasis`/`text.strong` have no reasonable existing
    /// code-highlighting analog (unlike every other capture this app has ever wired, which is
    /// force-fittable onto an existing bucket - see this module's own fallback-chain docs above),
    /// so they get their own honestly-named [`crate::code_surface::code_view::HighlightKind`]
    /// variants and real, distinct hues rather than a confusing reuse of e.g. `KEYWORD` for a
    /// heading. Verified in a real rendered window.
    pub const HEADING: ColorToken = token("syntax.heading", 0xc7a356);
    /// `text.uri`/`text.reference` (a link's destination and its visible label/text) - reuses
    /// [`FUNCTION`]'s own blue as its default, the conventional "this is a link" hue in most
    /// editors/themes.
    pub const LINK: ColorToken = token("syntax.link", 0x74ade8);
    /// `text.strong` (`**bold**`) - a real, distinct hue since this app's rendering pipeline has
    /// no per-run font-weight support yet (`RenderedLine::runs` only carries `(SharedString,
    /// HighlightKind)` - no style/weight field), so a colour is the only real signal available
    /// for now; a brighter tint of [`TEXT`] rather than [`TEXT`] itself, so bold prose still reads
    /// as more prominent than plain text even without real bold rendering.
    pub const STRONG: ColorToken = token("syntax.strong", 0xd3dae4);
    /// `text.emphasis` (`*italic*`) - same real font-style limitation as [`STRONG`]; a soft
    /// shares [`super::syntax::KEYWORD`]'s purple, which is unambiguous in context: a Markdown
    /// file has no keywords for it to collide with, exactly as [`HEADING`] shares [`TYPE`]'s gold.
    pub const EMPHASIS: ColorToken = token("syntax.emphasis", 0xc194d6);

    /// GitHub issue #183's own seven classification-precision splits - each capture below was a
    /// real, distinct grammar-level concept quietly folded into a coarser existing bucket. Every
    /// one keeps its old parent's exact colour by default (the restraint palette has no reason to
    /// tell them apart *visually* yet - see [`crate::code_surface::code_view::HighlightKind`]'s
    /// own docs for each variant's real capture/grammar evidence), so this is purely a
    /// classification fix: a future theme (or a future palette revision) now has a real,
    /// independent token to differentiate any of them without this module changing again.
    pub const PUNCTUATION_SPECIAL: ColorToken = token("syntax.punctuation_special", 0x6f757e);
    /// `label` (a Rust lifetime, a C goto target, a YAML anchor/alias) - defaults to
    /// [`VARIABLE`]'s own colour, its pre-issue bucket. See `HighlightKind::Label`'s own docs for
    /// why these three real, unrelated concepts still share this one token.
    pub const LABEL: ColorToken = token("syntax.label", 0xda8db2);
    /// `string.special` (a JS/TS regex literal, a TOML datetime, a CSS colour value) - defaults
    /// to [`STRING`]'s own colour, its pre-issue bucket.
    pub const STRING_SPECIAL: ColorToken = token("syntax.string_special", 0x98b46a);
    /// `function.builtin` (Python's `len`/`print`, Go's `append`/`make`/`panic`, JavaScript's
    /// `require`) - defaults to [`FUNCTION`]'s own colour, its pre-issue bucket.
    pub const FUNCTION_BUILTIN: ColorToken = token("syntax.function_builtin", 0x74ade8);
    /// `function.macro` (Rust's `println!`-style macro invocations) - defaults to [`FUNCTION`]'s
    /// own colour, its pre-issue bucket.
    pub const FUNCTION_MACRO: ColorToken = token("syntax.function_macro", 0x74ade8);
    /// `tag.error` (HTML's mismatched/erroneous closing tag) - defaults to [`TAG`]'s own colour,
    /// its pre-issue bucket.
    pub const TAG_ERROR: ColorToken = token("syntax.tag_error", 0xc7a356);
    /// `constructor` (Rust/Python/JavaScript's shared `^[A-Z]`-starts-with-a-capital heuristic for
    /// an enum-variant/struct construction site) - defaults to [`TYPE`]'s own colour, its
    /// pre-issue bucket.
    pub const CONSTRUCTOR: ColorToken = token("syntax.constructor", 0xc7a356);

    pub const CARET: ColorToken = token("syntax.caret", 0x4d97de);
    /// The code editor's real selection fill opacity (GitHub issue #27) while genuinely
    /// focused - applied on top of [`CARET`], the same color the solid caret itself paints, so
    /// selection and caret read as one consistent, theme-aware "insertion cursor" family rather
    /// than two independently-chosen colors.
    pub const SELECTION_OPACITY: f32 = 0.28;
    /// The same selection fill, dimmed further while the editor is unfocused (issue #27:
    /// "selection remains visible (dimmed) when the editor loses focus") - still genuinely
    /// visible, just clearly de-emphasized relative to the focused case above.
    pub const SELECTION_UNFOCUSED_OPACITY: f32 = 0.14;
    pub const ERROR_UNDERLINE: ColorToken = token("syntax.error_underline", 0xdc655f); // 2px dotted
    pub const HOVER_UNDERLINE: ColorToken = token("syntax.hover_underline", 0x5a84af); // 1px solid

    /// The File view's Diagnostic-state row tint (`README.md`: "row tinted `#191416`") -
    /// distinct from [`super::surface::CURRENT_LINE`].
    pub const DIAGNOSTIC_ROW_BG: ColorToken = token("syntax.diagnostic_row_bg", 0x191416);
    /// The Diagnostic state's dim, end-of-line inline message text (`README.md`: `#6b4a48`).
    pub const DIAGNOSTIC_INLINE_MESSAGE: ColorToken =
        token("syntax.diagnostic_inline_message", 0xb6706b);
    /// The Diagnostic state's card message headline. Same hex as
    /// [`super::button::DANGER_FG_HOVER`], kept as its own token - unrelated elements that
    /// happen to share a designed red. Was previously `0xf07f77` - a real, uncaught typo against
    /// this same doc comment's own cited value, fixed as part of GitHub issue #186's design
    /// review follow-up.
    pub const DIAGNOSTIC_CARD_MESSAGE: ColorToken =
        token("syntax.diagnostic_card_message", 0xe3908b);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("TEXT", TEXT),
        ("KEYWORD", KEYWORD),
        ("FUNCTION", FUNCTION),
        ("FUNCTION_METHOD", FUNCTION_METHOD),
        ("FUNCTION_DEFINITION", FUNCTION_DEFINITION),
        ("TYPE", TYPE),
        ("TYPE_BUILTIN", TYPE_BUILTIN),
        ("CONSTANT", CONSTANT),
        ("CONSTANT_BUILTIN", CONSTANT_BUILTIN),
        ("STRING", STRING),
        ("STRING_ESCAPE", STRING_ESCAPE),
        ("NUMBER", NUMBER),
        ("COMMENT", COMMENT),
        ("COMMENT_DOC", COMMENT_DOC),
        ("COMMENT_DOC_TAG", COMMENT_DOC_TAG),
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
        ("PUNCTUATION_SPECIAL", PUNCTUATION_SPECIAL),
        ("LABEL", LABEL),
        ("STRING_SPECIAL", STRING_SPECIAL),
        ("FUNCTION_BUILTIN", FUNCTION_BUILTIN),
        ("FUNCTION_MACRO", FUNCTION_MACRO),
        ("TAG_ERROR", TAG_ERROR),
        ("CONSTRUCTOR", CONSTRUCTOR),
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
    /// A clickable path/`path:line` link inside terminal output, under a dotted
    /// [`LINK_UNDERLINE`] rule.
    pub const LINK: ColorToken = token("term.link", 0x7fb4e3);
    pub const LINK_UNDERLINE: ColorToken = token("term.link_underline", 0x3d6a91);
    /// The link's hover state, which also swaps the dotted underline for a solid one. Same value
    /// as [`super::button::BLUE_FG`], kept as its own token for a distinct element.
    pub const LINK_HOVER: ColorToken = token("term.link_hover", 0xa5cdf0);
    pub const LINK_UNDERLINE_HOVER: ColorToken = token("term.link_underline_hover", 0x78a8d0);

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

/// The integrated terminal's own rendered-content palette - the colours a program running inside a
/// terminal pane actually paints its characters with.
pub mod terminal {
    use super::{token, ColorToken};

    /// The terminal pane's own fill - and what `NamedColor::Background` resolves to. Defaults to
    /// [`super::surface::PTY`]'s value, the token this palette already documents as "agent CLI +
    /// terminal", and the exact colour of the surface this pane is painted *into*
    /// (`crate::work_surface::render`'s `pty-surface`), so out of the box the terminal stops being
    /// a lighter rectangle floating inside the app's own chrome.
    pub const BACKGROUND: ColorToken = token("terminal.background", 0x0d0f11);
    /// Unstyled output - and what `NamedColor::Foreground`/`BrightForeground` resolve to. Defaults
    /// to [`super::term::TEXT`]'s value, this palette's own designed "terminal output text" tone,
    /// which until this module existed no renderer had ever actually painted.
    pub const FOREGROUND: ColorToken = token("terminal.foreground", 0xa7adb4);
    /// What `NamedColor::Cursor` resolves to - the colour a program gets when it explicitly asks
    /// for "the cursor colour" rather than a concrete one. Defaults to [`super::term::CURSOR`]'s
    /// value, the caret colour this app already paints elsewhere.
    pub const CURSOR: ColorToken = token("terminal.cursor", 0x5a9ad4);
    /// The fill painted behind a selected cell - GitHub issue #158's real mouse selection.
    pub const SELECTION: ColorToken = token("terminal.selection", 0x273a4d);

    /// `failed to start process: ...`, the line the pane paints when a spawn never happened.
    pub const SPAWN_ERROR: ColorToken = token("terminal.spawn_error", 0xff6b6b);
    /// The `[process exited]` label, once the child is gone.
    pub const PROCESS_EXITED: ColorToken = token("terminal.process_exited", 0xffcc66);

    /// The standard ANSI 16-colour palette, indexed `0..=15` by `NamedColor`'s own discriminants
    /// and by `Color::Indexed(0..=15)`, in the conventional order: black, red, green, yellow, blue,
    /// magenta, cyan, white, then the eight bright variants in the same order.
    pub const ANSI: [ColorToken; 16] = [
        token("terminal.ansi.0", 0x000000),
        token("terminal.ansi.1", 0xcd3131),
        token("terminal.ansi.2", 0x0dbc79),
        token("terminal.ansi.3", 0xe5e510),
        token("terminal.ansi.4", 0x2472c8),
        token("terminal.ansi.5", 0xbc3fbc),
        token("terminal.ansi.6", 0x11a8cd),
        token("terminal.ansi.7", 0xe5e5e5),
        token("terminal.ansi.8", 0x666666),
        token("terminal.ansi.9", 0xf14c4c),
        token("terminal.ansi.10", 0x23d18b),
        token("terminal.ansi.11", 0xf5f543),
        token("terminal.ansi.12", 0x3b8eea),
        token("terminal.ansi.13", 0xd670d6),
        token("terminal.ansi.14", 0x29b8db),
        token("terminal.ansi.15", 0xffffff),
    ];

    /// [`ANSI`]'s light-theme counterpart - VS Code's own default *light* terminal palette, same
    /// provenance and same `0..=15` ordering.
    pub const LIGHT_ANSI: [u32; 16] = [
        0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, 0x666666,
        0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
    ];

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("BACKGROUND", BACKGROUND),
        ("FOREGROUND", FOREGROUND),
        ("CURSOR", CURSOR),
        ("SELECTION", SELECTION),
        ("SPAWN_ERROR", SPAWN_ERROR),
        ("PROCESS_EXITED", PROCESS_EXITED),
        ("ANSI.0", ANSI[0]),
        ("ANSI.1", ANSI[1]),
        ("ANSI.2", ANSI[2]),
        ("ANSI.3", ANSI[3]),
        ("ANSI.4", ANSI[4]),
        ("ANSI.5", ANSI[5]),
        ("ANSI.6", ANSI[6]),
        ("ANSI.7", ANSI[7]),
        ("ANSI.8", ANSI[8]),
        ("ANSI.9", ANSI[9]),
        ("ANSI.10", ANSI[10]),
        ("ANSI.11", ANSI[11]),
        ("ANSI.12", ANSI[12]),
        ("ANSI.13", ANSI[13]),
        ("ANSI.14", ANSI[14]),
        ("ANSI.15", ANSI[15]),
    ];
}

/// The environment (WSL) chip's tokens - shown in the terminal footer, the status bar, and
/// Settings' `Default environment` row.
pub mod env {
    use super::{token, ColorToken};

    /// Defaults to [`super::term::PROMPT`]'s own value, independently themeable from it.
    pub const WSL_FG: ColorToken = token("env.wsl_fg", 0x8fbde6);
    pub const WSL_BG: ColorToken = token("env.wsl_bg", 0x16222c);
    pub const WSL_BORDER: ColorToken = token("env.wsl_border", 0x24384a);
    /// Defaults to [`super::text::FAINT`]'s own value.
    pub const LOCAL_FG: ColorToken = token("env.local_fg", 0x6b7178);
    /// Defaults to [`super::border::DIVIDER`]'s own value.
    pub const LOCAL_BORDER: ColorToken = token("env.local_border", 0x22262a);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("WSL_FG", WSL_FG),
        ("WSL_BG", WSL_BG),
        ("WSL_BORDER", WSL_BORDER),
        ("LOCAL_FG", LOCAL_FG),
        ("LOCAL_BORDER", LOCAL_BORDER),
    ];
}

/// One tint per agent - the **agent tint pool**. Used on the rail badge, the CLI tab chip, the
/// Changes panel's per-run left edge and the conflict side headers, so a colour always means the
/// same agent.
pub mod agent {
    use super::{token, ColorToken};

    /// Copper - `sonnet-4.5`. Was amber `#d8a94a`, one step from the needs-input amber it sits
    /// beside in the rail (see the module docs' reallocation table).
    pub const SONNET: (ColorToken, ColorToken) = (
        token("agent.sonnet.fg", 0xcf8a5c),
        token("agent.sonnet.bg", 0x31210f),
    ); // (fg, bg)
    /// Teal - `gpt-5-codex`. Was green `#6ab97f`, which is the additions colour; this also unifies
    /// the two different greens the same agent used in `sessions` vs `histDefs`.
    pub const CODEX: (ColorToken, ColorToken) = (
        token("agent.codex.fg", 0x4fb3a5),
        token("agent.codex.bg", 0x12302c),
    );
    /// Periwinkle - `haiku-4.5`. Was `#c98fbf`, the exact branch-scope violet: the collision
    /// review caught and the reason the allocation rule exists at all.
    pub const HAIKU: (ColorToken, ColorToken) = (
        token("agent.haiku.fg", 0x9d8fd4),
        token("agent.haiku.bg", 0x241f33),
    );
    /// Steel blue - the Cursor agent CLI. Unchanged by §4a: already outside all five families.
    /// Claimed by Cursor in GitHub issue #463; the `local` key name is the mock's original
    /// `qwen-local` slot, kept as-is because it is the public override key every built-in and
    /// user theme already writes (`assets/themes/*.toml`'s `[agent] local.fg`).
    pub const LOCAL: (ColorToken, ColorToken) = (
        token("agent.local.fg", 0x7f9ad4),
        token("agent.local.bg", 0x1f2941),
    );

    /// The real, enumerable agent tint pool - `(hue name, (fg, bg))` - and the list
    /// [`super::agent_tint_allocation_tests`] walks to enforce the module docs' reserved-hue rule.
    pub const TINT_POOL: &[(&str, (ColorToken, ColorToken))] = &[
        ("copper", SONNET),
        ("teal", CODEX),
        ("periwinkle", HAIKU),
        ("steel blue", LOCAL),
    ];

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
    /// The keycap glyph colour inside a green primary button - the fill to [`GREEN_KEYCAP`]'s
    /// border.
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

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("TRACK_ON", TRACK_ON),
        ("TRACK_OFF", TRACK_OFF),
        ("KNOB_ON", KNOB_ON),
        ("KNOB_OFF", KNOB_OFF),
        ("CHECKBOX_HOVER", CHECKBOX_HOVER),
    ];
}

/// Git's own `A`/`M`/`D` status letters on a file row, and the file tree's own A/M change marks.
pub mod tag {
    use super::{token, ColorToken};

    /// `A` - a file git reports as added.
    pub const STATUS_ADDED: ColorToken = token("tag.status_added", 0x5f9c78);
    /// `M` - a file git reports as modified. Neutral on purpose: the common case does not
    /// shout.
    pub const STATUS_MODIFIED: ColorToken = token("tag.status_modified", 0x767d84);
    /// `D` - a file git reports as deleted.
    pub const STATUS_DELETED: ColorToken = token("tag.status_deleted", 0xb06a66);
    pub const TREE_ADDED: ColorToken = token("tag.tree_added", 0x5f9c78); // "A" mark
    pub const TREE_MODIFIED: ColorToken = token("tag.tree_modified", 0xa3873f); // "M" mark

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("STATUS_ADDED", STATUS_ADDED),
        ("STATUS_MODIFIED", STATUS_MODIFIED),
        ("STATUS_DELETED", STATUS_DELETED),
        ("TREE_ADDED", TREE_ADDED),
        ("TREE_MODIFIED", TREE_MODIFIED),
    ];
}

/// Exact colours for Surface C's real Completions popup item rows - each its own token rather
/// than a reuse of a nearby-but-not-identical existing one (e.g. [`super::text::SELECTED`]
/// (`#dde2e7`) is a real, different colour from this module's own [`ITEM_SELECTED_FG`]
/// (`#e3e8ed`), and [`super::surface::CURRENT_LINE`] (`#181c20`) - the File view's current-line
/// tint - is the exact same hex as [`super::surface::POPOVER`] itself, which is why reusing it as
/// the selected-row highlight here used to paint an invisible selection).
pub mod completions_popup {
    use super::{token, ColorToken};

    /// A selected completion row's background.
    pub const ITEM_SELECTED_BG: ColorToken = token("completions_popup.item_selected_bg", 0x243c50);
    /// A selected completion row's label colour.
    pub const ITEM_SELECTED_FG: ColorToken = token("completions_popup.item_selected_fg", 0xe3e8ed);
    /// An unselected completion row's label colour - the exact same hex as
    /// [`super::text::BODY`], carried here as its own token.
    pub const ITEM_FG: ColorToken = token("completions_popup.item_fg", 0xb8bfc6);

    /// `(fg, bg)` for a `function`/`method`/`constructor`-shaped completion item's kind badge.
    pub const KIND_FUNCTION: (ColorToken, ColorToken) = (
        token("completions_popup.kind_function.fg", 0x8fbde6),
        token("completions_popup.kind_function.bg", 0x243c50),
    );
    /// `(fg, bg)` for a `variable`/`field`/`property`/`constant`-shaped completion item's kind
    /// badge.
    pub const KIND_VARIABLE: (ColorToken, ColorToken) = (
        token("completions_popup.kind_variable.fg", 0xd8a94a),
        token("completions_popup.kind_variable.bg", 0x33280f),
    );
    /// `(fg, bg)` for a `class`/`struct`/`interface`/`enum`/`type`-shaped completion item's kind
    /// badge.
    pub const KIND_TYPE: (ColorToken, ColorToken) = (
        token("completions_popup.kind_type.fg", 0xc294e0),
        token("completions_popup.kind_type.bg", 0x33203e),
    );

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

/// Settings-surface-only colours: the ones that have no equivalent in another module. Every
/// other Settings colour reuses an existing token - see `crate::root`'s Settings render methods.
pub mod settings {
    use super::{token, ColorToken};

    /// A nav row's hover background - distinct from [`super::surface::ROW_HOVER`] (`#15181b`).
    pub const NAV_ROW_HOVER: ColorToken = token("settings.nav_row_hover", 0x17191b);
    /// The content column's page-subtitle text - close to but distinct from
    /// [`super::text::DIM`] (`#8b9197`).
    pub const SUBTITLE: ColorToken = token("settings.subtitle", 0x767d84);
    /// A card row's own bottom separator - distinct from [`super::border::CARD_FIELD`]
    /// (`#22272b`).
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
    /// A Theme card's hover border.
    pub const THEME_CARD_HOVER_BORDER: ColorToken =
        token("settings.theme_card_hover_border", 0x3a4148);
    /// The config snippet block's section-header line colour.
    pub const SNIPPET_SECTION: ColorToken = token("settings.snippet_section", 0xc294e0);

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

/// The overlay scrollbar (GitHub issue #30).
pub mod scrollbar {
    use super::{px, token, ColorToken, Pixels};

    pub const THUMB: ColorToken = token("scrollbar.thumb", 0x2b3137);
    pub const THUMB_HOVER: ColorToken = token("scrollbar.thumb_hover", 0x3d444b);

    /// The track's full width (a vertical bar) or height (a horizontal one) - §4p's `width /
    /// height`. The thumb is [`THUMB_INSET`] narrower on each side, so what is actually painted is
    /// `WIDTH - 2 * THUMB_INSET` wide.
    pub const WIDTH: Pixels = px(10.0);
    /// The thumb's corner radius - §4p's `radius 5`.
    pub const THUMB_RADIUS: Pixels = px(5.0);
    /// How far clear of the track's edges the thumb floats - §4p's "2px transparent border and
    /// `background-clip:content-box`". In CSS that is a transparent border; drawn directly, it is
    /// an inset.
    pub const THUMB_INSET: Pixels = px(2.0);

    pub const TOKENS: &[(&str, ColorToken)] = &[("THUMB", THUMB), ("THUMB_HOVER", THUMB_HOVER)];
}

/// Per-provider rate-limit budget - the meter on an agent tab and in the budget popover.
pub mod budget {
    use super::{token, ColorToken};

    /// Above 40% remaining.
    pub const OK: ColorToken = token("budget.ok", 0x7fc79a);
    /// 15-40% remaining.
    pub const WARN: ColorToken = token("budget.warn", 0xc99b4e);
    /// Below 15% remaining.
    pub const CRITICAL: ColorToken = token("budget.critical", 0xc4726d);
    /// The unfilled remainder of the meter - the same neutral the diffstat bar's own empty segment
    /// uses ([`diff::STAT_EMPTY`]'s value), given its own key so a theme can move the meter's
    /// track without moving the diffstat's.
    pub const TRACK: ColorToken = token("budget.track", 0x22262a);
    /// A provider that is connected but whose last poll is stale (`last read 3m ago`), and the
    /// `not connected` row.
    pub const STALE: ColorToken = token("budget.stale", 0x6b7178);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("OK", OK),
        ("WARN", WARN),
        ("CRITICAL", CRITICAL),
        ("TRACK", TRACK),
        ("STALE", STALE),
    ];
}

/// The Changes panel - four stacked, collapsible sections in one scroller, plus the file row's
/// own seen/unseen and discard states.
pub mod changes {
    use super::{token, ColorToken};

    /// `UNCOMMITTED`'s 2px left edge - what is dirty in the checkout. Shares its value with
    /// [`border::SELECTED_EDGE`] (this palette's "structural blue") but carries its own key.
    pub const EDGE_UNCOMMITTED: ColorToken = token("changes.edge_uncommitted", 0x3f5b74);
    /// `COMMITS`' 2px left edge.
    pub const EDGE_NEUTRAL: ColorToken = token("changes.edge_neutral", 0x22262a);
    /// `AGAINST MAIN`'s 2px left edge - the branch-scope violet, in one of its reserved structural
    /// positions (see this module's own docs).
    pub const EDGE_AGAINST_MAIN: ColorToken = token("changes.edge_against_main", 0xc98fbf);

    /// Section header label - 9.5px/600 uppercase, `.09em` tracking.
    pub const SECTION_LABEL: ColorToken = token("changes.section_label", 0x9aa1a8);
    /// Section header count - 9.5px mono, immediately after the label.
    pub const SECTION_COUNT: ColorToken = token("changes.section_count", 0x4a5057);
    /// The section header's own disclosure caret (`▾`/`▸`).
    pub const SECTION_CARET: ColorToken = token("changes.section_caret", 0x8b9197);

    /// A run row's meta line while the run is **still moving**.
    pub const RUN_META_LIVE: ColorToken = token("changes.run_meta_live", 0x8a7548);
    /// A run row's meta line once the run has ended - the neutral half of [`RUN_META_LIVE`]'s
    /// pair. Same hex as [`super::text::FAINTER`], a distinct token for a distinct element (the
    /// same convention [`super::text::TREE_CARET`]'s own docs record).
    pub const RUN_META_ENDED: ColorToken = token("changes.run_meta_ended", 0x5e646a);
    /// The right-aligned per-section diffstat's `+N`, 10px mono.
    pub const SECTION_STAT_ADD: ColorToken = token("changes.section_stat_add", 0x7fc79a);
    /// The right-aligned per-section diffstat's `−N`, 10px mono.
    pub const SECTION_STAT_DEL: ColorToken = token("changes.section_stat_del", 0xc4726d);

    /// A filename not seen since the agent last changed it - reads forward (§4i).
    pub const FILENAME_UNSEEN: ColorToken = token("changes.filename_unseen", 0xdde2e7);
    /// A filename seen since the agent last changed it - recedes (§4i).
    pub const FILENAME_SEEN: ColorToken = token("changes.filename_seen", 0x767d84);

    /// The armed `Discard?` pill's fill. Discard is the one irreversible action in the panel - it
    /// destroys an agent's work with no git object behind it - so §4i gives it two clicks: the
    /// first swaps the hover icon for this pill, the second commits, and leaving the row cancels.
    pub const DISCARD_BG: ColorToken = token("changes.discard_bg", 0x2a1719);
    /// The armed `Discard?` pill's 1px border.
    pub const DISCARD_BORDER: ColorToken = token("changes.discard_border", 0x4a2422);
    /// The armed `Discard?` pill's label.
    pub const DISCARD_FG: ColorToken = token("changes.discard_fg", 0xe0847e);

    /// The `⚠` ring around a file row's author chips - *this path has lines from more than one
    /// agent* - and the control that filters the open diff by author (GitHub issue #287).
    pub const SHARED_RING: ColorToken = token("changes.shared_ring", 0x8a6420);

    /// The `you` gutter bar in the diff view - your own hand edit flips that line back to
    /// you, and `you` is deliberately **not** an agent, so it is deliberately not an agent tint
    /// either.
    pub const HAND_EDIT_GUTTER: ColorToken = token("changes.hand_edit_gutter", 0x4e545a);
    /// The `you` author chip's glyph, the neutral counterpart to an agent chip's own tint.
    pub const HAND_EDIT_CHIP_FG: ColorToken = token("changes.hand_edit_chip_fg", 0x8b9197);
    /// The `you` author chip's fill. Same hex as [`super::surface::CHIP_NEUTRAL`], its own key -
    /// see [`RUN_META_ENDED`] for this palette's convention on shared values.
    pub const HAND_EDIT_CHIP_BG: ColorToken = token("changes.hand_edit_chip_bg", 0x23272b);

    /// The `<agent> only ✕` indicator that appears in the file toolbar while a per-author filter
    /// is active, and **only** while it is active - without it a filtered diff would read as
    /// the whole diff.
    pub const FILTER_BG: ColorToken = token("changes.filter_bg", 0x1d2226);
    /// [`FILTER_BG`]'s hover fill - the indicator is a real control (clicking it clears the
    /// filter), so it answers the pointer.
    pub const FILTER_HOVER_BG: ColorToken = token("changes.filter_hover_bg", 0x242a2f);

    /// A hover-action button's own hover fill, inside the floating bar §4i puts on a hovered row.
    /// The bar's shell is the app's one popover chrome
    /// (`crate::root::widgets::menu_popover_chrome`, whose [`super::surface::PALETTE`]/
    /// [`super::border::POPOVER`]/[`super::radius::CARD`] really are §4i's own stated `#15181b`,
    /// `1px #2b3238`, radius 6); only the two buttons inside it need values of their own.
    pub const HOVER_ACTION_HOVER_BG: ColorToken = token("changes.hover_action_hover_bg", 0x232930);
    /// The discard button's hover fill - a red wash rather than [`HOVER_ACTION_HOVER_BG`]'s
    /// neutral one, because it is the one irreversible action in the panel and the hover is the
    /// last moment before the two-click confirm starts.
    pub const DISCARD_HOVER_BG: ColorToken = token("changes.discard_hover_bg", 0x33191b);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("EDGE_UNCOMMITTED", EDGE_UNCOMMITTED),
        ("EDGE_NEUTRAL", EDGE_NEUTRAL),
        ("EDGE_AGAINST_MAIN", EDGE_AGAINST_MAIN),
        ("SECTION_LABEL", SECTION_LABEL),
        ("SECTION_COUNT", SECTION_COUNT),
        ("SECTION_CARET", SECTION_CARET),
        ("RUN_META_LIVE", RUN_META_LIVE),
        ("RUN_META_ENDED", RUN_META_ENDED),
        ("SECTION_STAT_ADD", SECTION_STAT_ADD),
        ("SECTION_STAT_DEL", SECTION_STAT_DEL),
        ("FILENAME_UNSEEN", FILENAME_UNSEEN),
        ("FILENAME_SEEN", FILENAME_SEEN),
        ("SHARED_RING", SHARED_RING),
        ("HAND_EDIT_GUTTER", HAND_EDIT_GUTTER),
        ("HAND_EDIT_CHIP_FG", HAND_EDIT_CHIP_FG),
        ("HAND_EDIT_CHIP_BG", HAND_EDIT_CHIP_BG),
        ("FILTER_BG", FILTER_BG),
        ("FILTER_HOVER_BG", FILTER_HOVER_BG),
        ("DISCARD_BG", DISCARD_BG),
        ("DISCARD_BORDER", DISCARD_BORDER),
        ("DISCARD_FG", DISCARD_FG),
        ("HOVER_ACTION_HOVER_BG", HOVER_ACTION_HOVER_BG),
        ("DISCARD_HOVER_BG", DISCARD_HOVER_BG),
    ];
}

/// The status bar's three type tiers.
pub mod status_bar {
    use super::{token, ColorToken};

    /// Tier 1 - the readouts you are meant to find first (`main ↑2 ↓0`, `4 agents running`).
    pub const PRIMARY: ColorToken = token("status_bar.primary", 0xa9b0b7);
    /// Tier 2 - the supporting half of a split readout, dimmer than [`PRIMARY`] but still meant
    /// to be read: provider names in the bar once GitHub issue #294 lands, and today the
    /// Resources popover's own memory column, which is that same "detail you read only if the
    /// count surprised you" tone.
    pub const SECONDARY: ColorToken = token("status_bar.secondary", 0x7d848b);
    /// Tier 3 - resource readouts, present but never competing (`41% cpu · 3.4 GB`).
    pub const RECESSIVE: ColorToken = token("status_bar.recessive", 0x4a5057);
    /// The 1px, 13-high rules between tiers.
    pub const DIVIDER: ColorToken = token("status_bar.divider", 0x2b3137);

    /// The Resources popover's uppercase section labels (`CPU`, `MEMORY`, `ON DISK`,
    /// `LIVE NOW`) - §4d's `#5b6167`.
    pub const SECTION_LABEL: ColorToken = token("status_bar.section_label", 0x5b6167);
    /// The unfilled part of a load meter's 3px track (§4d's `#23282c`).
    pub const METER_TRACK: ColorToken = token("status_bar.meter_track", 0x23282c);

    /// `loadHue()`'s three steps (§4d, verbatim: "grey below 60%, amber to 85%, red above.
    /// Healthy load spends no colour"). Kept as three tokens rather than reusing
    /// [`super::status`]'s agent-state hues on purpose: an agent's amber means "this agent is
    /// waiting for you", a load meter's amber means "your work is affected" - the same pixel
    /// colour, two different reserved meanings, and a shared token would let a re-theme of one
    /// silently move the other.
    pub const LOAD_NEUTRAL: ColorToken = token("status_bar.load_neutral", 0x5e646a);
    /// 60% < load <= 85%.
    pub const LOAD_ELEVATED: ColorToken = token("status_bar.load_elevated", 0xc99b4e);
    /// load > 85%.
    pub const LOAD_CRITICAL: ColorToken = token("status_bar.load_critical", 0xc4726d);

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("PRIMARY", PRIMARY),
        ("SECONDARY", SECONDARY),
        ("RECESSIVE", RECESSIVE),
        ("DIVIDER", DIVIDER),
        ("SECTION_LABEL", SECTION_LABEL),
        ("METER_TRACK", METER_TRACK),
        ("LOAD_NEUTRAL", LOAD_NEUTRAL),
        ("LOAD_ELEVATED", LOAD_ELEVATED),
        ("LOAD_CRITICAL", LOAD_CRITICAL),
    ];
}

/// Diff-line review notes (GitHub issue #288) - the notes bar above the hunks and the card
/// pinned beneath a line.
pub mod notes {
    use super::{token, ColorToken};

    /// The card's 2px left edge, and the 5px square that opens the notes bar - selection blue.
    pub const EDGE: ColorToken = token("notes.edge", 0x5a9ad4);
    /// The pinned card's fill.
    pub const CARD_BG: ColorToken = token("notes.card_bg", 0x151a1f);
    /// The pinned card's 1px border (the three sides that are not [`EDGE`]).
    pub const CARD_BORDER: ColorToken = token("notes.card_border", 0x2b3d4f);
    /// The card's own note text.
    pub const CARD_FG: ColorToken = token("notes.card_fg", 0xc2c7cc);
    /// The placeholder shown in an empty, still-being-typed card.
    pub const CARD_PLACEHOLDER: ColorToken = token("notes.card_placeholder", 0x5e646a);

    /// A card's `draft` mark - it has not been delivered to anyone yet.
    pub const MARK_DRAFT: ColorToken = token("notes.mark_draft", 0x7fa9cf);
    /// A card's `sent` mark, and the bar's own `✓ sent` confirmation.
    pub const MARK_SENT: ColorToken = token("notes.mark_sent", 0x7fc79a);

    /// The diff row's note column when a note really is pinned on that line (`●`).
    pub const DOT: ColorToken = token("notes.dot", 0x8fbde6);
    /// The same column's `○` - a line that is the note cursor but carries no note yet.
    pub const DOT_EMPTY: ColorToken = token("notes.dot_empty", 0x3a3f44);

    /// The notes bar's band.
    pub const BAR_BG: ColorToken = token("notes.bar_bg", 0x141a20);
    /// Its 1px bottom rule against the hunks below it.
    pub const BAR_BORDER: ColorToken = token("notes.bar_border", 0x223140);
    /// The bar's count sentence (`1 note on this file`).
    pub const BAR_LABEL: ColorToken = token("notes.bar_label", 0xa5cdf0);
    /// The bar's fixed explanatory line (*one prompt, line-anchored · pinned after the
    /// revision*), deliberately recessive against [`BAR_LABEL`].
    pub const BAR_META: ColorToken = token("notes.bar_meta", 0x5e646a);
    /// A delivery that really failed, said out loud in the bar rather than swallowed.
    pub const BAR_ERROR: ColorToken = token("notes.bar_error", 0xc4726d);

    /// The `Send notes to <agent>` button's fill, border, hover fill and label.
    pub const SEND_BG: ColorToken = token("notes.send_bg", 0x18232d);
    /// Its 1px border, and its keycaps' border.
    pub const SEND_BORDER: ColorToken = token("notes.send_border", 0x365b78);
    /// Its hover fill.
    pub const SEND_HOVER_BG: ColorToken = token("notes.send_hover_bg", 0x1e2d3a);
    /// Its label.
    pub const SEND_FG: ColorToken = token("notes.send_fg", 0xa5cdf0);
    /// Its `⌘⏎` keycaps' glyphs.
    pub const SEND_CAP_FG: ColorToken = token("notes.send_cap_fg", 0x7fa9cf);

    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("EDGE", EDGE),
        ("CARD_BG", CARD_BG),
        ("CARD_BORDER", CARD_BORDER),
        ("CARD_FG", CARD_FG),
        ("CARD_PLACEHOLDER", CARD_PLACEHOLDER),
        ("MARK_DRAFT", MARK_DRAFT),
        ("MARK_SENT", MARK_SENT),
        ("DOT", DOT),
        ("DOT_EMPTY", DOT_EMPTY),
        ("BAR_BG", BAR_BG),
        ("BAR_BORDER", BAR_BORDER),
        ("BAR_LABEL", BAR_LABEL),
        ("BAR_META", BAR_META),
        ("BAR_ERROR", BAR_ERROR),
        ("SEND_BG", SEND_BG),
        ("SEND_BORDER", SEND_BORDER),
        ("SEND_HOVER_BG", SEND_HOVER_BG),
        ("SEND_FG", SEND_FG),
        ("SEND_CAP_FG", SEND_CAP_FG),
    ];
}

/// Agent history - the sidebar's run list and the run-transcript tab (GitHub issue #227).
pub mod history {
    use super::{token, ColorToken};

    /// `done` - its last turn ended cleanly and Jerry watched it end.
    pub const OUT_DONE_FG: ColorToken = token("history.out_done_fg", 0x7fc79a);
    pub const OUT_DONE_BG: ColorToken = token("history.out_done_bg", 0x16261e);
    /// `interrupted` - ended while it was still working, or still waiting on a human.
    pub const OUT_INTERRUPTED_FG: ColorToken = token("history.out_interrupted_fg", 0xc99b4e);
    pub const OUT_INTERRUPTED_BG: ColorToken = token("history.out_interrupted_bg", 0x2b2413);
    /// `failed` - its last real signal was a failure.
    pub const OUT_FAILED_FG: ColorToken = token("history.out_failed_fg", 0xc4726d);
    pub const OUT_FAILED_BG: ColorToken = token("history.out_failed_bg", 0x2a1719);
    /// `abandoned` - nobody ever saw it end.
    pub const OUT_ABANDONED_FG: ColorToken = token("history.out_abandoned_fg", 0x8b9197);
    pub const OUT_ABANDONED_BG: ColorToken = token("history.out_abandoned_bg", 0x1e2225);

    /// Drift dot, 0 commits since - `at the tip`.
    pub const DRIFT_TIP: ColorToken = token("history.drift_tip", 0x5cb87f);
    /// Drift dot, 1-2 commits since.
    pub const DRIFT_NEAR: ColorToken = token("history.drift_near", 0x8fbde6);
    /// Drift dot, 3+ commits since.
    pub const DRIFT_FAR: ColorToken = token("history.drift_far", 0xc99b4e);
    /// The drift *label*, in the two bands that do not tint it (§4's table names a text colour
    /// only for the far band). Shares [`text::FAINTER`]'s value, with its own key so a theme can
    /// move the history list's own recessive text without moving the rail's.
    pub const DRIFT_TEXT: ColorToken = token("history.drift_text", 0x5e646a);
    /// The drift label in the far band only (§4: "`N commits since` in `#a3873f`").
    pub const DRIFT_FAR_TEXT: ColorToken = token("history.drift_far_text", 0xa3873f);

    /// The scope toggle's selected segment - the `all` / `this worktree` pair. Its own keys
    /// rather than [`surface::SEGMENT_ACTIVE`]'s, because this control is a *bordered* pill
    /// inside a 27px band rather than the settings screen's tracked segmented control.
    pub const SCOPE_ON_BG: ColorToken = token("history.scope_on_bg", 0x1d2226);
    pub const SCOPE_ON_BORDER: ColorToken = token("history.scope_on_border", 0x2a3138);
    pub const SCOPE_ON_FG: ColorToken = token("history.scope_on_fg", 0xc2c7cc);
    pub const SCOPE_OFF_FG: ColorToken = token("history.scope_off_fg", 0x5e646a);
    pub const SCOPE_HOVER_BG: ColorToken = token("history.scope_hover_bg", 0x1b1f22);

    /// The repo header label in the history list's repo → worktree → run hierarchy.
    pub const REPO_LABEL: ColorToken = token("history.repo_label", 0x9aa1a8);
    /// A worktree group's label when it is *not* the active worktree; the active one takes
    /// [`text::SELECTED`] and the selection edge instead (§6: "Active worktree carries the blue
    /// edge and opens by default").
    pub const GROUP_LABEL: ColorToken = token("history.group_label", 0xa9b0b7);
    /// A run row's title when the row is not the open one.
    pub const ROW_TITLE: ColorToken = token("history.row_title", 0xc2c7cc);

    /// The transcript body's four line tones - the leading `❯ claude --resume …` command line,
    /// ordinary body text, the indented `⎿ …` detail lines, and the `● …` lead line that opens
    /// and closes a synthesised transcript.
    pub const TRANSCRIPT_PROMPT: ColorToken = token("history.transcript_prompt", 0x8fbde6);
    pub const TRANSCRIPT_BODY: ColorToken = token("history.transcript_body", 0xa7adb4);
    pub const TRANSCRIPT_DETAIL: ColorToken = token("history.transcript_detail", 0x6b7178);
    pub const TRANSCRIPT_LEAD: ColorToken = token("history.transcript_lead", 0xced4da);

    /// The transcript body's opacity - **70%**, the one signal that this is a recording, not
    /// a live pane.
    pub const TRANSCRIPT_OPACITY: f32 = 0.70;

    /// The run-transcript footer's `Resume here` button - §3's own triple, verbatim: "**Resume
    /// here** (`enter`, green `#1c3a2a` / `#376b4d` / `#9fdcb6`)", i.e. fill / border / label.
    pub const RESUME_BG: ColorToken = token("history.resume_bg", 0x1c3a2a);
    pub const RESUME_BG_HOVER: ColorToken = token("history.resume_bg_hover", 0x24503a);
    pub const RESUME_BORDER: ColorToken = token("history.resume_border", 0x376b4d);
    pub const RESUME_FG: ColorToken = token("history.resume_fg", 0x9fdcb6);

    /// Every real [`ColorToken`] this module declares - see [`super::TOKEN_GROUPS`].
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("OUT_DONE_FG", OUT_DONE_FG),
        ("OUT_DONE_BG", OUT_DONE_BG),
        ("OUT_INTERRUPTED_FG", OUT_INTERRUPTED_FG),
        ("OUT_INTERRUPTED_BG", OUT_INTERRUPTED_BG),
        ("OUT_FAILED_FG", OUT_FAILED_FG),
        ("OUT_FAILED_BG", OUT_FAILED_BG),
        ("OUT_ABANDONED_FG", OUT_ABANDONED_FG),
        ("OUT_ABANDONED_BG", OUT_ABANDONED_BG),
        ("DRIFT_TIP", DRIFT_TIP),
        ("DRIFT_NEAR", DRIFT_NEAR),
        ("DRIFT_FAR", DRIFT_FAR),
        ("DRIFT_TEXT", DRIFT_TEXT),
        ("DRIFT_FAR_TEXT", DRIFT_FAR_TEXT),
        ("SCOPE_ON_BG", SCOPE_ON_BG),
        ("SCOPE_ON_BORDER", SCOPE_ON_BORDER),
        ("SCOPE_ON_FG", SCOPE_ON_FG),
        ("SCOPE_OFF_FG", SCOPE_OFF_FG),
        ("SCOPE_HOVER_BG", SCOPE_HOVER_BG),
        ("REPO_LABEL", REPO_LABEL),
        ("GROUP_LABEL", GROUP_LABEL),
        ("ROW_TITLE", ROW_TITLE),
        ("TRANSCRIPT_PROMPT", TRANSCRIPT_PROMPT),
        ("TRANSCRIPT_BODY", TRANSCRIPT_BODY),
        ("TRANSCRIPT_DETAIL", TRANSCRIPT_DETAIL),
        ("TRANSCRIPT_LEAD", TRANSCRIPT_LEAD),
        ("RESUME_BG", RESUME_BG),
        ("RESUME_BG_HOVER", RESUME_BG_HOVER),
        ("RESUME_BORDER", RESUME_BORDER),
        ("RESUME_FG", RESUME_FG),
    ];
}

/// The right panel's Search tab (GitHub issue #162).
pub mod search {
    use super::{token, ColorToken};

    /// A modifier button's active fill - `Aa` / `ab` / `.*` while that modifier is on
    /// (§5: "on-state bg `#1d3242` fg `#a5cdf0`, off `#5e646a`"). Shared with the query row's
    /// own `⇄` and funnel toggles, which §4v draws in the same active pair.
    pub const MODIFIER_ON_BG: ColorToken = token("search.modifier_on_bg", 0x1d3242);
    /// That button's glyph while it is on.
    pub const MODIFIER_ON_FG: ColorToken = token("search.modifier_on_fg", 0xa5cdf0);

    /// The highlight behind the matched text on a result row (§4v: "the hit highlighted
    /// `#2b3d4f`/`#a5cdf0`").
    pub const MATCH_BG: ColorToken = token("search.match_bg", 0x2b3d4f);
    /// The matched text itself.
    pub const MATCH_FG: ColorToken = token("search.match_fg", 0xa5cdf0);

    /// The un-matched context either side of the hit on a result row - deliberately recessive
    /// against [`MATCH_FG`], since the line is there to place the hit, not to be read in full.
    pub const LINE: ColorToken = token("search.line", 0x767d84);

    /// the module's slice of [`super::TOKEN_GROUPS`]'s whole-app registry.
    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("MODIFIER_ON_BG", MODIFIER_ON_BG),
        ("MODIFIER_ON_FG", MODIFIER_ON_FG),
        ("MATCH_BG", MATCH_BG),
        ("MATCH_FG", MATCH_FG),
        ("LINE", LINE),
    ];
}

/// The git graph tab (GitHub issue #1). The column header band and the removal of the per-commit
/// session column (`HEADER`/`HEADER_BG`/`HEADER_LABEL_FG` below) came later and supersede the
/// graph's first spec on those two points only; everything else here is that first spec's values.
pub mod graph {
    use super::{px, token, ColorToken, Pixels};

    /// Row height (§2: "Row height 26").
    pub const ROW: Pixels = px(26.0);
    /// Lane canvas column width (§2: "lane canvas 100").
    pub const LANE_CANVAS: Pixels = px(100.0);
    /// A lane's vertical sits at `x = 9 + lane * 14` (§2).
    pub const LANE_X_BASE: Pixels = px(9.0);
    pub const LANE_STEP: Pixels = px(14.0);
    /// Stroke width of every line the commit graph draws: the lane verticals, the elbow bridge,
    /// and both elbow curves' borders (plus the rebase surface's fold elbow, which deliberately
    /// shares the graph's visual vocabulary).
    pub const LINE_WIDTH: Pixels = px(2.0);
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
    /// GitHub issue #242 phase B's interactive-rebase banner band height (design spec §1.2:
    /// "44px tall"). Sits where [`TOOLBAR`] normally would while the graph pane is in rebase
    /// mode - see `crate::graph_view::rebase_render`'s module docs.
    pub const REBASE_BANNER: Pixels = px(44.0);
    /// An interactive-rebase plan row's height, 28. Deliberately **not** [`ROW`] (26, the
    /// ordinary commit list's): the fold elbow is `top:-1`/`bottom:13` inside this box, which
    /// only lands on the
    /// action chip's centreline (an 18-high chip centred in 28 has its centre at 14, and a
    /// bottom-inset-13 box's inside-painted 1px bottom border occupies exactly 14..15) at 28.
    /// §2.2's own rebase menu anchor formula (`44 + 22 + 3 + row × 28 + 30`) counts in 28s too.
    pub const REBASE_ROW: Pixels = px(28.0);
    /// The plan list's own 3px top/bottom padding (§2.2's `+ 3` term).
    pub const REBASE_LIST_PAD: Pixels = px(3.0);
    /// The plan footer band's height (§1.4: "**Footer**, 28 high").
    pub const REBASE_FOOTER: Pixels = px(28.0);
    /// The fold elbow's slot width on a `squash`/`fixup` row (§1.4: "20 wide on `squash`/`fixup`
    /// rows only"). A non-folding row has no slot at all, which is what makes a fold row read as
    /// indented under the commit it folds into.
    pub const REBASE_FOLD_INDENT: Pixels = px(20.0);
    /// The fold elbow's own insets inside [`REBASE_FOLD_INDENT`] (§1.4: "inset 5 each side,
    /// `top:-1` so it meets the row above's edge, `bottom:13` so it lands on the chip
    /// centreline").
    pub const REBASE_FOLD_ELBOW_INSET_X: Pixels = px(5.0);
    pub const REBASE_FOLD_ELBOW_TOP: Pixels = px(-1.0);
    pub const REBASE_FOLD_ELBOW_BOTTOM: Pixels = px(13.0);
    /// The action chip's height (§1.4: "18 high, `0 7` padding").
    pub const REBASE_CHIP: Pixels = px(18.0);
    /// The action menu's width (§1.4: "**Action menu:** 274 wide").
    pub const REBASE_MENU_WIDTH: Pixels = px(274.0);
    /// The action menu's own row columns (§1.4: "✓ mark 9 · action name 46 · hint flex").
    pub const REBASE_MENU_MARK: Pixels = px(9.0);
    pub const REBASE_MENU_NAME: Pixels = px(46.0);
    /// The plan's own column widths (§1.3: "`action` 104 (13 left pad, clears the rows' 2px
    /// selection edge) · `commit` flex · `files` 62 right · `sha` 62 right · pause column 22").
    pub const REBASE_COL_ACTION: Pixels = px(104.0);
    pub const REBASE_COL_ACTION_PAD: Pixels = px(13.0);
    pub const REBASE_COL_NUMERIC: Pixels = px(62.0);
    pub const REBASE_COL_PAUSE: Pixels = px(22.0);
    /// The drag handle's slot (§1.4: "drag handle | 11 wide").
    pub const REBASE_DRAG_SLOT: Pixels = px(11.0);
    /// Every 5px square this surface paints - the pause marks (§1.5), the footer legend's own
    /// copy of the outlined one, the warning-stack severity dots (§1.7) and the stopped strip's
    /// (§1.8). One constant because the design deliberately makes them the same mark.
    pub const REBASE_MARK: Pixels = px(5.0);

    /// The six interactive-rebase action chips (§1.4's action table, verbatim). Their own token
    /// family rather than reaches into [`super::term`]/[`super::budget`]/[`super::diff`]: several
    /// of these hexes already exist elsewhere in this module under names that mean something
    /// entirely different (`#d8a94a` is [`TAG_CHIP_FG`], `#8fbde6` is `term::PROMPT`, `#c4726d`
    /// is `button::DANGER_FG`), and a re-theme of "the terminal's prompt colour" must not silently
    /// move "what `reword` looks like" - the same call [`super::status_bar`]'s own load-hue tokens
    /// document. §1.4 is explicit that these are "the existing status palette - no new hues", so
    /// this family introduces no hue the reserved-hue rule ([`super::agent`]) does not already
    /// allocate: `edit` is the attention amber it shares with the planned-pause mark, `drop` is
    /// the failure/deletion red, `squash`/`fixup` are the additions green (one step apart, because
    /// they do the same thing to history), `reword` is the informational blue.
    pub const REBASE_PICK_FG: ColorToken = token("graph.rebase_pick_fg", 0xa9b0b7);
    pub const REBASE_PICK_BG: ColorToken = token("graph.rebase_pick_bg", 0x1c2023);
    pub const REBASE_PICK_BORDER: ColorToken = token("graph.rebase_pick_border", 0x2a2f34);
    pub const REBASE_REWORD_FG: ColorToken = token("graph.rebase_reword_fg", 0x8fbde6);
    pub const REBASE_REWORD_BG: ColorToken = token("graph.rebase_reword_bg", 0x1d2532);
    pub const REBASE_REWORD_BORDER: ColorToken = token("graph.rebase_reword_border", 0x2b3d4f);
    /// A `reword` row's message field before a message has really been supplied (§1.6: "1px
    /// border `#3b4a58` (→ `#2b3d4f` once a message is supplied)"). The brighter of the pair, on
    /// purpose: an unanswered field is the one asking for something.
    pub const REBASE_REWORD_BORDER_EMPTY: ColorToken =
        token("graph.rebase_reword_border_empty", 0x3b4a58);
    pub const REBASE_EDIT_FG: ColorToken = token("graph.rebase_edit_fg", 0xd8a94a);
    pub const REBASE_EDIT_BG: ColorToken = token("graph.rebase_edit_bg", 0x2b2413);
    pub const REBASE_EDIT_BORDER: ColorToken = token("graph.rebase_edit_border", 0x3f3418);
    pub const REBASE_SQUASH_FG: ColorToken = token("graph.rebase_squash_fg", 0x7fc79a);
    pub const REBASE_SQUASH_BG: ColorToken = token("graph.rebase_squash_bg", 0x16261e);
    pub const REBASE_SQUASH_BORDER: ColorToken = token("graph.rebase_squash_border", 0x24503a);
    pub const REBASE_FIXUP_FG: ColorToken = token("graph.rebase_fixup_fg", 0x5f9c78);
    pub const REBASE_FIXUP_BG: ColorToken = token("graph.rebase_fixup_bg", 0x16261e);
    pub const REBASE_FIXUP_BORDER: ColorToken = token("graph.rebase_fixup_border", 0x1e3b2a);
    pub const REBASE_DROP_FG: ColorToken = token("graph.rebase_drop_fg", 0xc4726d);
    pub const REBASE_DROP_BG: ColorToken = token("graph.rebase_drop_bg", 0x2a1719);
    pub const REBASE_DROP_BORDER: ColorToken = token("graph.rebase_drop_border", 0x4a2422);
    /// Any action chip's hover border (§1.4: "hover border `#3a4148`").
    pub const REBASE_CHIP_HOVER_BORDER: ColorToken =
        token("graph.rebase_chip_hover_border", 0x3a4148);
    /// The plan row's `⋮⋮` drag handle (§1.4: "`⋮⋮` 9px mono `#363b40`").
    pub const REBASE_DRAG_HANDLE: ColorToken = token("graph.rebase_drag_handle", 0x363b40);
    /// The column header's own outlined pause square (§1.3: "pause column 22, carrying an
    /// outlined 5px square in `#3a3f44`" - dimmer than the rows' own `#8a6420`
    /// `status::ASK_CARD_EDGE`, because it is a legend, not a live mark).
    pub const REBASE_HEADER_PAUSE_MARK: ColorToken =
        token("graph.rebase_header_pause_mark", 0x3a3f44);
    /// A warning row's body line (§1.7: "body 10.5px/15 `#767d84`").
    pub const REBASE_WARNING_BODY: ColorToken = token("graph.rebase_warning_body", 0x767d84);
    /// The remote-commits warning's own dot (§1.7 warning 2: "blue `#8fbde6`"). Same hex as
    /// [`REBASE_REWORD_FG`], separate token for the same reason that family exists at all: this
    /// one means "informational, not urgent", not "this row will be reworded".
    pub const REBASE_WARNING_REMOTE: ColorToken = token("graph.rebase_warning_remote", 0x8fbde6);
    /// The column header band's height, 22. Sits between [`TOOLBAR`] and the row list -
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
    /// its content is fixed (four headers, ten action rows, one footer line; never varies with
    /// which row opened it), so unlike `crate::menu::model::menu_height` (which has to
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
    pub const ROW_MENU_HEIGHT: Pixels = px(425.0);
    /// The Branches panel's own branch right-click context menu width (GitHub issue #241).
    pub const BRANCH_MENU_WIDTH: Pixels = px(400.0);
    /// The branch context menu's painted height under the test suite's `gpui::TestAppContext` -
    /// pinned by `crate::graph_view::render::graph_branch_menu_tests::
    /// the_branch_menu_pins_the_real_height_this_edge_clamp_relies_on` for exactly the reasons
    /// [`ROW_MENU_HEIGHT`]'s own docs give (fixed content, no analytical formula, same
    /// synthetic-glyph-metrics caveat, same safe degradation if it ever drifts).
    pub const BRANCH_MENU_HEIGHT: Pixels = px(338.0);
    /// Behind-count amber threshold (§5: "behind turns `#a3873f` past 4").
    pub const BEHIND_WARN_THRESHOLD: usize = 4;
    pub const BEHIND_WARN: ColorToken = token("graph.behind_warn", 0xa3873f);
    /// Branches panel row height (§5: "28-high rows").
    pub const BRANCH_ROW: Pixels = px(28.0);
    /// Branches panel filter row height (§5: "a 31-high filter row").
    pub const BRANCHES_FILTER_ROW: Pixels = px(31.0);
    /// A branch with no lane in the visible graph gets a neutral dot (§5).
    pub const BRANCH_NO_LANE_DOT: ColorToken = token("graph.branch_no_lane_dot", 0x3d4248);

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
        ("REBASE_PICK_FG", REBASE_PICK_FG),
        ("REBASE_PICK_BG", REBASE_PICK_BG),
        ("REBASE_PICK_BORDER", REBASE_PICK_BORDER),
        ("REBASE_REWORD_FG", REBASE_REWORD_FG),
        ("REBASE_REWORD_BG", REBASE_REWORD_BG),
        ("REBASE_REWORD_BORDER", REBASE_REWORD_BORDER),
        ("REBASE_REWORD_BORDER_EMPTY", REBASE_REWORD_BORDER_EMPTY),
        ("REBASE_EDIT_FG", REBASE_EDIT_FG),
        ("REBASE_EDIT_BG", REBASE_EDIT_BG),
        ("REBASE_EDIT_BORDER", REBASE_EDIT_BORDER),
        ("REBASE_SQUASH_FG", REBASE_SQUASH_FG),
        ("REBASE_SQUASH_BG", REBASE_SQUASH_BG),
        ("REBASE_SQUASH_BORDER", REBASE_SQUASH_BORDER),
        ("REBASE_FIXUP_FG", REBASE_FIXUP_FG),
        ("REBASE_FIXUP_BG", REBASE_FIXUP_BG),
        ("REBASE_FIXUP_BORDER", REBASE_FIXUP_BORDER),
        ("REBASE_DROP_FG", REBASE_DROP_FG),
        ("REBASE_DROP_BG", REBASE_DROP_BG),
        ("REBASE_DROP_BORDER", REBASE_DROP_BORDER),
        ("REBASE_CHIP_HOVER_BORDER", REBASE_CHIP_HOVER_BORDER),
        ("REBASE_DRAG_HANDLE", REBASE_DRAG_HANDLE),
        ("REBASE_HEADER_PAUSE_MARK", REBASE_HEADER_PAUSE_MARK),
        ("REBASE_WARNING_BODY", REBASE_WARNING_BODY),
        ("REBASE_WARNING_REMOTE", REBASE_WARNING_REMOTE),
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
    /// The 5px squares the interactive-rebase plan paints - its pause marks, its
    /// warning-stack severity dots and the stopped strip's own. All four are
    /// `border-radius:1px`, and at 5px across the
    /// difference from [`MARK`] is the difference between reading as a square and reading as a
    /// dot - which is load-bearing here, since this app already uses a real circle for "an agent's
    /// status" and these deliberately are not that.
    pub const MARK_SM: Pixels = px(1.0);
    pub const PILL: Pixels = px(8.0); // toggle track (26x15)
}

pub mod band {
    use super::{px, Pixels};

    pub const TITLE_BAR: Pixels = px(38.0);
    /// Shared height for the work-surface tab strip, the rail's own sidebar strip
    /// (`crate::rail::strip`, GitHub issue #291 - it *replaced* the plain rail header at this
    /// same height), and the files/changes panel header - the three sit side by side under the
    /// title bar and must line up pixel-perfect, so they read off one constant instead of three
    /// values that could drift independently.
    pub const CHROME_HEADER: Pixels = px(36.0);
    pub const CONTEXT_BAR: Pixels = px(32.0);
    pub const DIFF_TOOLBAR: Pixels = px(31.0);
    pub const FILTER_ROW: Pixels = px(30.0);
    pub const SURFACE_FOOTER: Pixels = px(28.0);
    pub const PTY_HEADER: Pixels = px(27.0);
    /// The **shell** pane's info footer band (`pid` · grid dimensions · environment chip ·
    /// right-aligned static copy) - the alternative to [`SURFACE_FOOTER`] (the **agent** pane's
    /// readout strip: GitHub issue #295), never stacked with it. A pane gets exactly one bottom
    /// bar, chosen by `ProcessKind`.
    pub const PTY_INFO_FOOTER: Pixels = px(26.0);
    pub const BREADCRUMB: Pixels = px(26.0);
    /// Raised twice, 26 -> 28 -> 30, the second time for the three-tier rebuild that also took
    /// the group gap from 9 to 14 (GitHub issue #293).
    pub const STATUS_BAR: Pixels = px(30.0);
    pub const PALETTE_INPUT: Pixels = px(44.0);
    pub const PALETTE_ROW: Pixels = px(30.0);
    pub const CHANGE_ROW: Pixels = px(27.0);
    /// One Changes-panel section header, 24 high.
    pub const CHANGES_SECTION_HEADER: Pixels = px(24.0);
    /// One Runs-section row - two lines, `padding: 7 10 8 8`, 3px between them.
    pub const RUN_ROW: Pixels = px(48.0);
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

    // The right panel's Search tab (GitHub issue #162). Every value below is a real designed
    // measurement, not a round number picked here. The query row itself is [`FILTER_ROW`] (30),
    // shared with the rail's own filter - they are the same object in two places and must not
    // drift.
    /// The `⇄` replace row, revealed under the query row.
    pub const SEARCH_REPLACE_ROW: Pixels = px(28.0);
    /// One of the two `include`/`exclude` glob rows, revealed under those.
    pub const SEARCH_GLOB_ROW: Pixels = px(25.0);
    /// The count row: `14 results in 6 files` plus the `⇄` / funnel / fold-all controls.
    pub const SEARCH_COUNT_ROW: Pixels = px(24.0);
    /// A result tree's file row - caret, chip, name, dimmed directory, match count.
    pub const SEARCH_FILE_ROW: Pixels = px(24.0);
    /// One match row under it - line number, then the line with its hit highlighted.
    pub const SEARCH_MATCH_ROW: Pixels = px(19.0);
    /// The square hit box every 17x17 icon-only control button uses - the search panel's count
    /// row and find bar, and the rail footer's prune button - the same 17px hit box as the
    /// other icon buttons. **Not** the icon's own optical size - that is
    /// `crate::icons::IconSize::Control` (12px), a real measurement of the glyph's actual
    /// geometry, which sits inset and centred inside this box rather than filling it. The two
    /// were wrongly conflated when this constant was first named after `IconSize::Control`'s
    /// (then-incorrect) 17px value; GitHub issue filed 2026-08-16 for the "icons too big"
    /// screenshot report this caused.
    pub const ICON_BUTTON_HIT: Pixels = px(17.0);
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
    /// One sidebar-strip cell's width (GitHub issue #291). The strip's cells are the same object
    /// as the centre tabs - **full-height cells, no radius, no gap** - inside a
    /// [`super::band::CHROME_HEADER`]-high band. Every cell in the strip is exactly this wide,
    /// view cells and the `+`/`⋯` alike, which is what makes the dividing rules read as a tab
    /// strip's segments rather than as icons on a dark band.
    pub const SIDEBAR_STRIP_CELL: Pixels = px(38.0);
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
    // rgba(0,0,0,0.55)
}

/// Honestly-scoped application of `Settings.appearance.interface_scale_percent` - text-size
/// scaling only, deliberately not padding/spacing/icon/fixed-chrome dimensions (retrofitting
/// every literal `Pixels` constant in this module to scale is out of scope). See
/// `crate::root::AdeApp::ui_text_size` for the render-side application, which chooses whether to
/// call [`scaled_px`] at each call site.
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

/// Palette-only (⌘P) colours: the ones that have no equivalent in another module. See
/// `docs/design/command-palette.md`.
pub mod palette {
    use super::{token, ColorToken};

    /// The input row's scope-prefix glyph.
    pub const PREFIX: ColorToken = token("palette.prefix", 0x5f7f9e);
    /// A result group's uppercase header label - close to but distinct from
    /// [`super::text::FAINT`] (`#6b7178`).
    pub const GROUP_HEADER: ColorToken = token("palette.group_header", 0x5b6167);
    /// An unselected result row's hover background - distinct from
    /// [`super::surface::ROW_HOVER`] (`#15181b`, which
    /// happens to equal the palette panel's own background, [`super::surface::PALETTE`]).
    pub const ROW_HOVER: ColorToken = token("palette.row_hover", 0x191d20);
    /// The selected/first row's label colour - one hex step brighter than
    /// [`super::text::SELECTED`] (`#dde2e7`).
    pub const LABEL_SELECTED: ColorToken = token("palette.label_selected", 0xe3e8ed);
    /// A command result's kind chip `(fg, bg)` - the same hex pair as [`super::lang::MD`], kept
    /// as its own token since a command chip and a Markdown-file chip are unrelated concepts.
    pub const COMMAND_CHIP: (ColorToken, ColorToken) = (
        token("palette.command_chip.fg", 0x7f9ad4),
        token("palette.command_chip.bg", 0x1d2532),
    );

    pub const TOKENS: &[(&str, ColorToken)] = &[
        ("PREFIX", PREFIX),
        ("GROUP_HEADER", GROUP_HEADER),
        ("ROW_HOVER", ROW_HOVER),
        ("LABEL_SELECTED", LABEL_SELECTED),
        ("COMMAND_CHIP.fg", COMMAND_CHIP.0),
        ("COMMAND_CHIP.bg", COMMAND_CHIP.1),
    ];
}

/// The four later `lang` chip tokens, checked against their designed hex values - reconstructed
/// independently from the raw `u32` here rather than by reusing [`hex`] (the same function under
/// test), so a transcription error in [`lang::TS`]/[`lang::VUE`]/[`lang::PY`]/[`lang::GO`] would
/// actually be caught rather than tautologically confirmed.
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

    /// The four language chips design spec D pins by hex. The rest of the chips are checked only
    /// for distinctness, below - the spec names these four.
    #[test]
    fn every_spec_pinned_lang_chip_still_carries_its_spec_hex_pair() {
        for (name, chip, foreground, background) in [
            ("ts", lang::TS, 0x6b9bd1, 0x1b2838),
            ("vue", lang::VUE, 0x5cb87f, 0x16261e),
            ("py", lang::PY, 0xc9b04a, 0x2a2612),
            ("go", lang::GO, 0x5fa8c4, 0x152730),
        ] {
            assert!(
                same_pair(chip, (rgba_from_u32(foreground), rgba_from_u32(background))),
                "{name}'s chip no longer matches its spec hex pair"
            );
        }
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
    fn ui_scale_scales_proportionally_and_leaves_one_hundred_percent_alone() {
        for (percent, expected) in [(100, 12.0), (150, 18.0), (50, 6.0)] {
            assert_eq!(scaled_px(12.0, percent), px(expected), "at {percent}%");
        }
    }
}

/// Real, source-parsing coverage that [`TOKEN_GROUPS`] is honestly *total* - the property every
/// other piece of this rewrite depends on. The registry is what theme-file key validation
/// (`crate::settings::custom_theme`), the built-in theme generator
/// (`crate::settings::builtin_themes`) and the "generate from colour" action all walk, so a token
/// that exists in this file but not in the registry would be a colour no theme could ever change
/// *and* one no generated file would ever mention - silently, with nothing else failing.
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

    #[test]
    fn the_registry_covers_the_whole_palette_not_a_sample_of_it() {
        assert!(
            all_tokens().count() >= 250,
            "only {} registered tokens - this app's palette is ~270",
            all_tokens().count()
        );
    }
}

/// Enforces the reserved-hue allocation rule quoted in full in [`agent`]'s own docs, which is
/// the rule these tests exist to make unbreakable.
#[cfg(test)]
mod agent_tint_allocation_tests {
    use super::*;

    /// The five structural hue families, exactly as §4a's table names them - family label and
    /// every hex the table lists for it.
    const RESERVED_FAMILIES: &[(&str, &[u32])] = &[
        (
            "amber (attention, warnings, planned pauses)",
            &[0xe2a336, 0xd8a94a],
        ),
        ("violet (branch and graph scope)", &[0xc98fbf]),
        ("green (additions and staged state)", &[0x5cb87f, 0x7fc79a]),
        ("red (failure and deletions)", &[0xe0625c, 0xd1706a]),
        ("blue (selection and focus)", &[0x3f5b74, 0x5a9ad4]),
    ];

    /// How far, in degrees of OKLCH hue, an agent tint has to sit from every reserved family
    /// before it reads as its own colour rather than as a shade of that family's meaning.
    const MIN_SEPARATION_DEGREES: f32 = 15.0;

    /// The smallest OKLCH hue distance from `color` to any hex in any reserved family, with the
    /// family that was closest.
    fn nearest_reserved_family(color: Rgba) -> (&'static str, f32) {
        let (_, _, hue) = oklch_of(color);
        RESERVED_FAMILIES
            .iter()
            .flat_map(|(family, hexes)| {
                hexes.iter().map(move |hex| {
                    let (_, _, reserved_hue) = oklch_of(hex_rgba(*hex));
                    (*family, hue_distance(hue, reserved_hue))
                })
            })
            .fold(("", f32::MAX), |nearest, candidate| {
                if candidate.1 < nearest.1 {
                    candidate
                } else {
                    nearest
                }
            })
    }

    #[test]
    fn every_agent_tint_sits_outside_all_five_reserved_hue_families() {
        for (name, (foreground, _)) in agent::TINT_POOL {
            let (family, separation) = nearest_reserved_family(foreground.default);
            assert!(
                separation >= MIN_SEPARATION_DEGREES,
                "agent tint `{name}` ({:?}) is only {separation:.1}° of OKLCH hue from the \
                 reserved {family} family - under the {MIN_SEPARATION_DEGREES}° floor, so this \
                 colour would read as that family's *meaning* rather than as an agent's identity. \
                 This is the failure the allocation rule exists to prevent (haiku-4.5's tint \
                 was the branch violet exactly, and the two met on the same 2px left edge). Pick a \
                 hue outside all five families - see `theme::agent`'s own docs for the table.",
                foreground.key
            );
        }
    }

    #[test]
    fn the_rule_actually_rejects_the_collision_review_caught() {
        let reallocated = [
            (0xc98fbf, "haiku-4.5", "violet"),
            (0xd8a94a, "sonnet-4.5", "amber"),
            (0x6ab97f, "gpt-5-codex", "green"),
        ];
        for (hex, agent_name, expected_family) in reallocated {
            let (family, separation) = nearest_reserved_family(hex_rgba(hex));
            assert!(
                separation < MIN_SEPARATION_DEGREES,
                "#{hex:06x} ({agent_name}'s tint before §4a) measures {separation:.1}° from the \
                 nearest reserved family, which the {MIN_SEPARATION_DEGREES}° floor would ACCEPT - \
                 but this is a collision the design review actually caught and reallocated. A rule \
                 that admits it is not enforcing anything."
            );
            assert!(
                family.starts_with(expected_family),
                "#{hex:06x} ({agent_name}) collided with {expected_family} per §4a, but the \
                 nearest family measured here is {family}"
            );
        }
    }

    #[test]
    fn every_registered_agent_tint_is_listed_in_the_pool() {
        for (name, token) in agent::TOKENS {
            let Some(stripped) = name.strip_suffix(".fg") else {
                continue;
            };
            assert!(
                agent::TINT_POOL
                    .iter()
                    .any(|(_, (foreground, _))| foreground.key == token.key),
                "agent::{stripped} is a registered agent tint but is missing from \
                 agent::TINT_POOL, so the reserved-hue rule never checks it - add it to the pool \
                 (see `theme::agent`'s \"Adding an agent\" docs)"
            );
        }
        assert_eq!(
            agent::TINT_POOL.len(),
            agent::TOKENS.len() / 2,
            "every agent tint is a (fg, bg) pair, so the pool should list exactly half as many \
             entries as the module registers keys"
        );
    }

    #[test]
    fn the_reserved_families_are_themselves_distinct_and_internally_coherent() {
        for (family, hexes) in RESERVED_FAMILIES {
            for hex in *hexes {
                let (_, _, hue) = oklch_of(hex_rgba(*hex));
                for other_hex in *hexes {
                    let (_, _, other_hue) = oklch_of(hex_rgba(*other_hex));
                    assert!(
                        hue_distance(hue, other_hue) < MIN_SEPARATION_DEGREES,
                        "{family}'s own members #{hex:06x} and #{other_hex:06x} are further apart \
                         than the collision floor - they are not one hue family"
                    );
                }
            }
        }
        for (family, hexes) in RESERVED_FAMILIES {
            for (other_family, other_hexes) in RESERVED_FAMILIES {
                if family == other_family {
                    continue;
                }
                for hex in *hexes {
                    let (_, _, hue) = oklch_of(hex_rgba(*hex));
                    for other_hex in *other_hexes {
                        let (_, _, other_hue) = oklch_of(hex_rgba(*other_hex));
                        assert!(
                            hue_distance(hue, other_hue) >= MIN_SEPARATION_DEGREES,
                            "{family} and {other_family} measure as the same hue - the OKLCH \
                             maths behind every assertion in this module is not discriminating"
                        );
                    }
                }
            }
        }
    }
}

/// Proves the claim the whole rev-6 campaign is built on: **this file is the only place a colour
/// literal lives**.
#[cfg(test)]
mod stray_hex_tests {
    /// The real theme layer - the only files allowed to name a colour literally.
    const THEME_LAYER: &[&str] = &["app/src/theme.rs", "app/src/settings/builtin_themes.rs"];

    /// Every `.rs` file under `crates/`, as `(path relative to crates/, contents)`.
    fn workspace_sources() -> Vec<(String, String)> {
        fn walk(
            directory: &std::path::Path,
            root: &std::path::Path,
            out: &mut Vec<(String, String)>,
        ) {
            let entries = std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()));
            for entry in entries {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let relative = path
                        .strip_prefix(root)
                        .expect("every walked path starts at the root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((
                        relative,
                        std::fs::read_to_string(&path).expect("valid utf-8"),
                    ));
                }
            }
        }
        // `CARGO_MANIFEST_DIR` is `<repo>/crates/app`, so its parent is the whole `crates/` tree -
        // every workspace member, not just this one.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/app always has a parent")
            .to_path_buf();
        let mut sources = Vec::new();
        walk(&root, &root, &mut sources);
        sources.sort();
        sources
    }

    /// Does this line contain a `0x` followed by exactly six hex digits - the shape a colour
    /// wears, and the shape a bitmask or a codepoint essentially never does?
    fn names_a_colour_literal(line: &str) -> bool {
        let bytes = line.as_bytes();
        line.match_indices("0x").any(|(index, _)| {
            let digits = bytes[index + 2..]
                .iter()
                .take_while(|byte| byte.is_ascii_hexdigit())
                .count();
            digits == 6
        })
    }

    #[test]
    fn no_production_code_outside_the_theme_layer_names_a_colour_literally() {
        let mut violations = Vec::new();
        let mut scanned = 0usize;
        for (path, source) in workspace_sources() {
            if THEME_LAYER.iter().any(|allowed| path.ends_with(allowed)) {
                continue;
            }
            scanned += 1;
            // Test code may name colours freely - an exact-hex assertion is how several suites in
            // this file pin a token's real value. Only shipped code is under the rule. A
            // `#[cfg(test)] mod ... { }` is written at column 0 throughout this workspace, so its
            // closing brace is the one line that is exactly `}`.
            let mut in_test_module = false;
            for (number, line) in source.lines().enumerate() {
                if line.starts_with("#[cfg(test)]") {
                    in_test_module = true;
                    continue;
                }
                if in_test_module {
                    if line == "}" {
                        in_test_module = false;
                    }
                    continue;
                }
                if names_a_colour_literal(line) {
                    violations.push(format!("  {path}:{}: {}", number + 1, line.trim()));
                }
            }
        }
        assert!(
            scanned > 50,
            "only scanned {scanned} source files - the walk is not finding the workspace, so this \
             test would pass vacuously"
        );
        assert!(
            violations.is_empty(),
            "{} colour literal(s) outside the theme layer:\n{}\n\nA hex value here is a colour no \
             theme file can reach and no generated theme mentions. Declare a real token in \
             `crate::theme` (see its module docs for the naming rule) and reference it by name - \
             `crate::theme` is the one place in this codebase colour literals belong.",
            violations.len(),
            violations.join("\n")
        );
    }
}

/// GitHub issue #208's own coverage inside this module: the shape of the new [`terminal`] group,
/// and the one real special case it adds to [`derived_palette`].
#[cfg(test)]
mod terminal_palette_tests {
    use super::*;

    #[test]
    fn the_terminal_group_is_distinct_from_term_and_fully_registered() {
        let terminal_keys: Vec<&str> = terminal::TOKENS.iter().map(|(_, t)| t.key).collect();
        assert_eq!(
            terminal_keys.len(),
            22,
            "background, foreground, cursor, selection, the ANSI sixteen, and the two pane status \
             lines (spawn error, process exited) rev 6 turned from inline hex into real tokens"
        );
        for key in &terminal_keys {
            assert!(
                token_for_key(key).is_some(),
                "{key} is declared but not reachable through the registry"
            );
        }
        for (_, token) in term::TOKENS {
            assert!(
                !terminal_keys.contains(&token.key),
                "{} is registered in both groups - one would shadow the other",
                token.key
            );
        }
        // The specific confusion this guards: `term.cursor` (the app's own caret colour, painted
        // in the command palette and the File view) and `terminal.cursor` (what a pty's own
        // `NamedColor::Cursor` resolves to) are two real, separately themeable colours.
        assert_ne!(term::CURSOR.key, terminal::CURSOR.key);
    }

    #[test]
    fn the_terminal_selection_default_is_the_editor_selection_flattened() {
        let over = surface::CENTER.default;
        let fill = editor::SELECTION.default;
        let alpha = editor::SELECTION_OPACITY;
        let flatten =
            |top: f32, bottom: f32| ((alpha * top + (1.0 - alpha) * bottom) * 255.0).round();

        let expected = (
            flatten(fill.r, over.r) as u8,
            flatten(fill.g, over.g) as u8,
            flatten(fill.b, over.b) as u8,
        );
        let actual = terminal::SELECTION.default;
        assert_eq!(
            (
                (actual.r * 255.0).round() as u8,
                (actual.g * 255.0).round() as u8,
                (actual.b * 255.0).round() as u8
            ),
            expected
        );
    }

    #[test]
    fn a_dark_derived_theme_keeps_the_standard_ansi_sixteen_exactly() {
        let shift = derive_shift(
            crate::settings::builtin_themes::jerry_dark_swatches(),
            [0x12100e, 0x1e1a16, 0x8fae6b, 0xd98b3a, 0xc4713f], // Ember's own swatches
        );
        let derived: HashMap<&str, Rgba> = derived_palette(shift).into_iter().collect();

        for index in 0..16 {
            let key = terminal::ANSI[index].key;
            assert_eq!(
                crate::settings::custom_theme::rgba_to_hex(derived[key]),
                crate::settings::custom_theme::rgba_to_hex(terminal::ANSI[index].default),
                "{key} was shifted - the ANSI sixteen carry fixed conventional meanings and are \
                 deliberately not derived"
            );
        }
        // ...while the chrome around them genuinely *is* derived, or this test would pass just as
        // well against a shift that did nothing at all.
        assert_ne!(
            crate::settings::custom_theme::rgba_to_hex(derived["terminal.background"]),
            crate::settings::custom_theme::rgba_to_hex(terminal::BACKGROUND.default),
        );
    }

    #[test]
    fn a_light_derived_theme_gets_the_light_ansi_palette_with_black_still_black() {
        let shift = derive_shift(
            crate::settings::builtin_themes::jerry_dark_swatches(),
            [0xf4f1ea, 0xe4e0d6, 0x3f7a52, 0xa8752a, 0x3d6c9c], // Paper's own swatches
        );
        let derived: HashMap<&str, Rgba> = derived_palette(shift).into_iter().collect();

        assert!(
            theme_is_light(derived["surface.window"]),
            "sanity check: these swatches really do derive a light theme"
        );
        for index in 0..16 {
            let key = terminal::ANSI[index].key;
            assert_eq!(
                crate::settings::custom_theme::rgba_to_hex(derived[key]),
                terminal::LIGHT_ANSI[index],
                "{key} must come from the light palette"
            );
        }
        assert_eq!(
            crate::settings::custom_theme::rgba_to_hex(derived["terminal.ansi.0"]),
            0x000000,
            "ANSI black must survive a light theme as black - deriving it would have inverted it \
             to near-white, invisible on the light background this same theme derives"
        );
        assert!(
            theme_is_light(derived["terminal.background"]),
            "and the terminal itself must genuinely go light, or the palette above is being used \
             against the wrong background"
        );
    }

    #[test]
    fn the_dark_and_light_ansi_palettes_are_genuinely_different() {
        let differing = (0..16)
            .filter(|index| {
                crate::settings::custom_theme::rgba_to_hex(terminal::ANSI[*index].default)
                    != terminal::LIGHT_ANSI[*index]
            })
            .count();
        assert!(
            differing >= 8,
            "only {differing} of the sixteen differ between the dark and light palettes"
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

    #[test]
    fn jerry_dark_resolve_is_bit_exact_with_no_lookup_at_all() {
        assert!(
            current_theme_palette().is_none(),
            "the real default before any test touches it"
        );
        assert!(same(surface::WINDOW.resolve(), surface::WINDOW.default));
        assert!(same(syntax::KEYWORD.resolve(), hex_rgba(0xc194d6)));
    }

    #[test]
    fn an_installed_palette_really_changes_what_a_token_resolves_to() {
        let jerry_dark = surface::WINDOW.resolve();
        let _guard = with_palette(&[("surface.window", 0xf4f1ea)]);
        assert!(!same(surface::WINDOW.resolve(), jerry_dark));
        assert!(same(surface::WINDOW.resolve(), hex_rgba(0xf4f1ea)));
    }

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

    #[test]
    fn a_former_alias_can_now_be_moved_without_moving_what_it_used_to_alias() {
        assert!(
            same(syntax::FUNCTION_METHOD.default, syntax::FUNCTION.default),
            "sanity check: the two still share a default, which is what makes this test meaningful"
        );
        let _guard = with_palette(&[("syntax.function_method", 0x50fa7b)]);
        assert!(same(syntax::FUNCTION_METHOD.resolve(), hex_rgba(0x50fa7b)));
        assert!(
            same(syntax::FUNCTION.resolve(), syntax::FUNCTION.default),
            "syntax::FUNCTION_METHOD used to be the very same const - overriding the method bucket \
             must no longer touch it"
        );
    }

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

    #[test]
    fn derive_shift_solves_an_exact_linear_fit_through_the_two_background_swatches() {
        // A synthetic "base" theme (window bg lightness ~10%, panel ~20%) and "target" theme
        // (window bg ~50%, panel ~70%) - the fit should map base 0.10 -> target 0.50 and base
        // 0.20 -> target 0.70 exactly.
        let base = [0x1a1a1a, 0x333333, 0x808080, 0x808080, 0x808080];
        let target = [0x808080, 0xb3b3b3, 0x808080, 0x808080, 0x808080];
        let shift = derive_shift(base, target);

        let remap = |hex_value: u32| -> f32 {
            let (lightness, _, _) = oklch_of(hex_rgba(hex_value));
            (lightness * shift.lightness_scale + shift.lightness_offset).clamp(0.0, 1.0)
        };
        let (target_bg, _, _) = oklch_of(hex_rgba(0x808080));
        let (target_panel, _, _) = oklch_of(hex_rgba(0xb3b3b3));

        assert!((remap(0x1a1a1a) - target_bg).abs() < 0.01);
        assert!((remap(0x333333) - target_panel).abs() < 0.01);
    }

    #[test]
    fn derive_shift_never_produces_nan_when_the_base_swatches_have_equal_lightness() {
        let base = [0x404040, 0x404040, 0x808080, 0x808080, 0x808080];
        let target = [0x202020, 0x606060, 0x101010, 0x505050, 0x909090];
        let shift = derive_shift(base, target);
        assert!(shift.lightness_scale.is_finite());
        assert!(shift.lightness_offset.is_finite());
        assert!(shift.hue.is_finite());
        assert!(shift.chroma_scale.is_finite());
    }

    #[test]
    fn shift_from_seed_rotates_hue_and_scales_chroma_but_never_lightness() {
        let seed = hex_rgba(0xe07a5f); // a warm coral, far from Jerry Dark's accent blue
        let shift = shift_from_seed(seed);
        assert_eq!(shift.lightness_scale, 1.0);
        assert_eq!(shift.lightness_offset, 0.0);

        // The reference accent, run through this shift, must land on the seed's own hue - that is
        // the whole promise of "generate a theme from this colour".
        let (_, rotated_chroma, rotated_hue) =
            oklch_of(apply_shift(hex_rgba(SEED_REFERENCE_ACCENT), shift));
        let (_, seed_chroma, seed_hue) = oklch_of(seed);
        assert!(
            hue_distance(rotated_hue, seed_hue) < 0.5,
            "the app's accent should land on the seed's hue ({rotated_hue} vs {seed_hue})"
        );
        assert!((rotated_chroma - seed_chroma).abs() < 0.01);
    }

    #[test]
    fn a_seed_equal_to_the_reference_accent_derives_the_identity_palette() {
        let shift = shift_from_seed(hex_rgba(SEED_REFERENCE_ACCENT));
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

    /// The sidebar strip's one structural invariant, in every bundled theme (GitHub issue #291).
    ///
    /// A tab only reads as connected if the strip behind it is **darker than the panel**: with
    /// strip and rail on one colour the slab floats. The selected cell fills with
    /// [`surface::RAIL`] and
    /// paints its own rule in the same colour, so a theme that let [`surface::SIDEBAR_STRIP`]
    /// collapse onto `RAIL` would take the strip's entire selection idiom with it - a failure no
    /// contrast floor catches, because both colours would still be perfectly legible.
    ///
    /// Stated as *recession*, not as "darker", because `Paper` is a real bundled light theme whose
    /// derivation legitimately inverts the ramp: there, every surface Jerry Dark makes darker is
    /// made lighter. So the invariant is measured against [`surface::WINDOW`] - the palette's own
    /// "one step back from a panel" - and asks only that the strip sits on that same side of the
    /// rail, by at least as much.
    #[test]
    fn the_sidebar_strip_stays_recessed_below_the_rail_in_every_bundled_theme() {
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let strip = relative_luminance(surface::SIDEBAR_STRIP.resolve());
            let rail = relative_luminance(surface::RAIL.resolve());
            let window = relative_luminance(surface::WINDOW.resolve());
            let recession = window - rail;
            assert!(
                recession != 0.0,
                "{}: premise - this palette must separate the window body from a panel at all, \
                 or there is no direction for `recessed` to mean",
                def.name
            );
            assert!(
                (strip - rail).signum() == recession.signum(),
                "{}: the sidebar strip ({strip:.4}) must be recessed from the rail ({rail:.4}) in \
                 the same direction the window body ({window:.4}) is - on the wrong side of it \
                 the selected cell stops reading as joined to the panel below",
                def.name
            );
            assert!(
                (strip - rail).abs() >= recession.abs(),
                "{}: and by at least as much - a strip that only just clears the rail is the \
                 floating slab \u{a7}4v rejected",
                def.name
            );
        }
    }

    /// Two floors, because `Jerry Dark` and `Paper` are the two palettes actually authored - the
    /// other four are mechanical `derive_shift` transforms and are held to a looser bound so a
    /// derivation artifact does not read as a palette defect.
    #[test]
    fn every_syntax_token_clears_its_themes_own_contrast_floor() {
        const MIN_RATIO_AUTHORED: f32 = 2.5;
        const MIN_RATIO: f32 = 1.5;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let min_ratio = if matches!(def.name, "Jerry Dark" | "Paper") {
                MIN_RATIO_AUTHORED
            } else {
                MIN_RATIO
            };
            let background = surface::CENTER.resolve();
            for (key, token) in syntax_tokens() {
                let ratio = contrast_ratio(token.resolve(), background);
                assert!(
                    ratio >= min_ratio,
                    "{key} only reaches {ratio:.2}:1 against surface::CENTER in {} - below the \
                     real {min_ratio}:1 floor",
                    def.name
                );
            }
        }
    }

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
#[cfg(test)]
mod syntax_bracket_ring_tests {
    use super::syntax_color_math::delta_e;
    use super::syntax_contrast_tests::{contrast_ratio, with_bundled_theme};
    use super::*;
    use crate::code_surface::code_view::HighlightKind;

    /// A colour's CIE-Lab chroma (`sqrt(a*^2 + b* ^2)`) - how *saturated* it is, independent of
    /// how light it is. The one number that exposed the replaced ring as out of family, and the
    /// one every ΔE-only check was blind to.
    fn chroma(color: Rgba) -> f32 {
        let (_, a, b) = super::syntax_color_math::lab(color);
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
    /// alike. Every pair, not only the cyclically adjacent ones - six levels of nesting has to
    /// stay legible, not merely three.
    #[test]
    fn no_two_ring_colours_collide_in_any_bundled_theme() {
        // ΔE is bought with chroma, and this ring is deliberately held below the palette's accents
        // in chroma (a bracket must never shout louder than a string). Six hues at one lightness
        // and one low chroma have a mathematical ceiling on how far apart they can be, so 18 is
        // set from what a reader needs - ~8x the ~2.3 just-noticeable difference - not from what
        // an optimiser can reach. Real measured worst case across every bundled theme: 20.7.
        const MIN_DELTA_E: f32 = 18.0;
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

    #[test]
    fn no_ring_colour_is_confusable_with_a_syntax_accent() {
        // This floor was **raised** from 8.0 when the ring moved off the accent lightness band
        // and down onto the punctuation one (`L 0.560`, see `syntax`' own bracket-ring docs).
        // The previous ring sat at `L 0.700 / C 0.080` against accents at `0.760 / 0.095` and had
        // to buy its separation from hue - which stopped working once the accent set grew to ten
        // families, because there is no longer a set of six hue gaps wide enough to hide in. Held
        // at that tier, `BRACKET_5` measured ΔE 9.9 from `FUNCTION`.
        //
        // Buying the separation from lightness instead is strictly better and the numbers say so:
        // measured worst case across every bundled theme is now **12.6** (`Ember`), against 8.5
        // before, and Jerry Dark itself - the palette anyone actually authored - measures **20.8**
        // against the previous 11.3. Every one of the ten accents is checked here, not the five
        // representatives the restraint palette had.
        const MIN_DELTA_E: f32 = 12.0;
        let accents = [
            ("VARIABLE_PARAMETER", syntax::VARIABLE_PARAMETER),
            ("CONSTANT", syntax::CONSTANT),
            ("TYPE", syntax::TYPE),
            ("STRING", syntax::STRING),
            ("ATTRIBUTE", syntax::ATTRIBUTE),
            ("PROPERTY", syntax::PROPERTY),
            ("FUNCTION", syntax::FUNCTION),
            ("FUNCTION_DEFINITION", syntax::FUNCTION_DEFINITION),
            ("KEYWORD", syntax::KEYWORD),
            ("VARIABLE", syntax::VARIABLE),
            ("ERROR_UNDERLINE", syntax::ERROR_UNDERLINE),
        ];
        /// Measured worst case in Jerry Dark itself is 20.8, against 11.3 for the ring this
        /// replaced - a palette that grew from eight semantic hue families to ten ended up with a
        /// *more* clearly separated ring, which is the whole argument for spending lightness
        /// rather than hue on it.
        const MIN_DELTA_E_AUTHORED: f32 = 18.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let floor = if def.name == "Jerry Dark" {
                MIN_DELTA_E_AUTHORED
            } else {
                MIN_DELTA_E
            };
            for (ring_name, ring_token) in ring_tokens() {
                for (accent_name, accent_token) in accents {
                    let distance = delta_e(ring_token.resolve(), accent_token.resolve());
                    assert!(
                        distance >= floor,
                        "{ring_name} is only ΔE {distance:.1} from {accent_name} in {} - a \
                         coloured bracket must never impersonate a semantic token",
                        def.name
                    );
                }
            }
        }
    }

    /// A *matched* bracket reading like an *unmatched* one would erase the whole
    /// matched/unmatched distinction this feature's honest-degradation design rests on. Since the
    /// redesign there are two tones to stay clear of, not one: plain text, and
    /// `syntax::PUNCTUATION_BRACKET`'s own dimmer unmatched tone.
    #[test]
    fn every_ring_colour_stays_clear_of_both_tones_it_must_not_be_mistaken_for() {
        // Real measured worst case: 16.7, in Jerry Dark. If this fires after a change to the
        // defaults, the generated theme files probably need regenerating (see
        // `crate::settings::builtin_themes`).
        const MIN_DELTA_E: f32 = 14.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            for (reference_name, reference) in [
                ("the unmatched-bracket tone", syntax::PUNCTUATION_BRACKET),
                ("plain text", syntax::TEXT),
            ] {
                for (name, token) in ring_tokens() {
                    let distance = delta_e(token.resolve(), reference.resolve());
                    assert!(
                        distance >= MIN_DELTA_E,
                        "{name} is only ΔE {distance:.1} from {reference_name} in {} - a matched \
                         bracket would be indistinguishable from an unmatched one",
                        def.name
                    );
                }
            }
        }
    }

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
mod syntax_color_math {
    use super::Rgba;

    /// CIE-Lab, the space this crate's *distinctness* checks are stated in. Deliberately kept
    /// alongside the OKLCH maths rather than replaced by it: the ΔE figures recorded in this
    /// module's docs and in `docs/theme-palette-design.md` are Lab ΔE, the familiar "~2.3 is a
    /// just-noticeable difference" scale, and restating them in OKLab units would make every
    /// historical number in those docs unreadable against the new ones.
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

    /// Perceptual distance. ~2.3 is the just-noticeable difference; this crate requires far more.
    pub(super) fn delta_e(a: Rgba, b: Rgba) -> f32 {
        let (la, aa, ba) = lab(a);
        let (lb, ab, bb) = lab(b);
        ((la - lb).powi(2) + (aa - ab).powi(2) + (ba - bb).powi(2)).sqrt()
    }
}

#[cfg(test)]
mod syntax_palette_tests {
    use super::syntax_color_math::delta_e;
    use super::syntax_contrast_tests::{contrast_ratio, with_bundled_theme};
    use super::*;

    /// The only tokens still held at *exactly* plain foreground, and the reason each one is.
    fn plain_foreground_tokens() -> Vec<(&'static str, ColorToken)> {
        vec![("EMBEDDED", syntax::EMBEDDED)]
    }

    /// The de-emphasized punctuation family - deliberately *below* plain text, never above it.
    fn punctuation_tokens() -> Vec<(&'static str, ColorToken)> {
        vec![
            ("OPERATOR", syntax::OPERATOR),
            ("PUNCTUATION_BRACKET", syntax::PUNCTUATION_BRACKET),
            ("PUNCTUATION_DELIMITER", syntax::PUNCTUATION_DELIMITER),
        ]
    }

    /// The real accent tier - one representative per hue family. All ten sit at one OKLCH
    /// lightness and one chroma and differ only in hue, which is the property
    /// `every_accent_shares_one_lightness_and_one_chroma` measures.
    fn accent_tokens() -> Vec<(&'static str, ColorToken)> {
        vec![
            ("VARIABLE_PARAMETER", syntax::VARIABLE_PARAMETER),
            ("CONSTANT", syntax::CONSTANT),
            ("TYPE", syntax::TYPE),
            ("STRING", syntax::STRING),
            ("ATTRIBUTE", syntax::ATTRIBUTE),
            ("PROPERTY", syntax::PROPERTY),
            ("FUNCTION", syntax::FUNCTION),
            ("FUNCTION_DEFINITION", syntax::FUNCTION_DEFINITION),
            ("KEYWORD", syntax::KEYWORD),
            ("VARIABLE", syntax::VARIABLE),
        ]
    }

    #[test]
    fn only_the_deliberately_neutral_tokens_render_at_plain_foreground() {
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let text = syntax::TEXT.resolve();
            for (name, token) in plain_foreground_tokens() {
                let value = token.resolve();
                assert_eq!(
                    (value.r, value.g, value.b),
                    (text.r, text.g, text.b),
                    "{name} must resolve to exactly plain text in {}",
                    def.name
                );
            }
        }
    }

    #[test]
    fn keywords_calls_and_identifiers_all_carry_real_colour_in_every_bundled_theme() {
        const MIN_DELTA_E: f32 = 18.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let text = syntax::TEXT.resolve();
            for (name, token) in accent_tokens()
                .into_iter()
                .chain([("FUNCTION_METHOD", syntax::FUNCTION_METHOD)])
            {
                let distance = delta_e(token.resolve(), text);
                assert!(
                    distance >= MIN_DELTA_E,
                    "{name} is only ΔE {distance:.1} from plain text in {} - it has to read as \
                     genuinely coloured, not as a hex that happens to differ",
                    def.name
                );
            }
        }
    }

    #[test]
    fn the_three_identifier_tokens_are_distinguishable_from_each_other() {
        const MIN_DELTA_E: f32 = 10.0;
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let tokens = [
                ("VARIABLE", syntax::VARIABLE),
                ("VARIABLE_PARAMETER", syntax::VARIABLE_PARAMETER),
                ("PROPERTY", syntax::PROPERTY),
            ];
            for (index, (name_a, token_a)) in tokens.iter().enumerate() {
                for (name_b, token_b) in tokens.iter().skip(index + 1) {
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

    #[test]
    fn a_definition_site_is_clearly_distinguishable_from_a_call_site() {
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let call = syntax::FUNCTION.resolve();
            let definition = syntax::FUNCTION_DEFINITION.resolve();
            assert!(
                delta_e(call, definition) > 12.0,
                "a function definition must not read like a call in {} - got dE {:.1}",
                def.name,
                delta_e(call, definition)
            );
        }
    }

    #[test]
    fn punctuation_is_dimmer_than_plain_text_but_still_clears_three_to_one() {
        let background = surface::CENTER.default;
        let text_ratio = contrast_ratio(syntax::TEXT.default, background);
        for (name, token) in punctuation_tokens() {
            let ratio = contrast_ratio(token.default, background);
            assert!(
                ratio < text_ratio,
                "{name} must be de-emphasized relative to plain text ({ratio:.2} vs {text_ratio:.2})"
            );
            assert!(
                ratio >= 3.0,
                "{name} is de-emphasized, not invisible - {ratio:.2}:1 is below the 3:1 floor"
            );
        }
    }

    #[test]
    fn comments_clear_the_full_body_text_contrast_floor_not_a_relaxed_one() {
        let background = surface::CENTER.default;
        for (name, token) in [
            ("COMMENT", syntax::COMMENT),
            ("COMMENT_DOC", syntax::COMMENT_DOC),
        ] {
            let ratio = contrast_ratio(token.default, background);
            assert!(
                ratio >= 4.5,
                "{name} must be readable prose, not decoration - {ratio:.2}:1 is below 4.5:1"
            );
        }
    }

    #[test]
    fn every_accent_hue_family_stays_a_real_hue_apart() {
        const MIN_SEPARATION: f32 = 25.0;
        let mut families: Vec<(&'static str, f32)> = Vec::new();
        for (name, token) in accent_tokens() {
            let (_, _, hue) = oklch_of(token.default);
            for (other_name, other_hue) in &families {
                let separation = hue_distance(*other_hue, hue);
                assert!(
                    separation >= MIN_SEPARATION,
                    "{name} and {other_name} are only {separation:.1} degrees apart - at one \
                     lightness and one chroma, hue is the *only* thing telling two accents apart"
                );
            }
            families.push((name, hue));
        }
        assert!(
            families.len() <= 10,
            "ten semantic hue families is the ceiling, found {}",
            families.len()
        );
    }

    #[test]
    fn no_syntax_token_invents_a_hue_outside_the_accent_wheel() {
        let accent_hues: Vec<f32> = accent_tokens()
            .into_iter()
            .map(|(_, token)| oklch_of(token.default).2)
            .collect();
        for token in all_tokens() {
            if !token.key.starts_with("syntax.")
                || token.key.starts_with("syntax.bracket_")
                || token.key.contains("diagnostic")
                || token.key.contains("underline")
            {
                continue;
            }
            let (_, chroma, hue) = oklch_of(token.default);
            if chroma <= 0.03 {
                continue; // a neutral, not an accent
            }
            assert!(
                accent_hues
                    .iter()
                    .any(|accent| hue_distance(*accent, hue) < 5.0),
                "{} sits at hue {hue:.0}, which is not one of the accent wheel's own families",
                token.key
            );
        }
    }

    #[test]
    fn every_accent_shares_one_lightness_and_one_chroma() {
        let measured: Vec<(f32, f32)> = accent_tokens()
            .into_iter()
            .map(|(_, token)| {
                let (l, c, _) = oklch_of(token.default);
                (l, c)
            })
            .collect();
        let lightnesses: Vec<f32> = measured.iter().map(|(l, _)| *l).collect();
        let chromas: Vec<f32> = measured.iter().map(|(_, c)| *c).collect();
        let spread = |values: &[f32]| {
            values.iter().cloned().fold(f32::MIN, f32::max)
                - values.iter().cloned().fold(f32::MAX, f32::min)
        };
        assert!(
            spread(&lightnesses) < 0.02,
            "accents must share one OKLCH lightness, spread was {:.3}",
            spread(&lightnesses)
        );
        assert!(
            spread(&chromas) < 0.02,
            "accents must share one OKLCH chroma, spread was {:.3}",
            spread(&chromas)
        );
    }

    #[test]
    fn every_accent_clears_the_body_text_floor_in_every_bundled_theme() {
        for def in crate::settings::state::THEME_DEFS.iter() {
            let _guard = with_bundled_theme(def.name);
            let background = surface::CENTER.resolve();
            for (name, token) in accent_tokens() {
                let ratio = contrast_ratio(token.resolve(), background);
                assert!(
                    ratio >= 4.5,
                    "{name} is {ratio:.2}:1 against {}'s editor background, below the 4.5:1 floor",
                    def.name
                );
            }
        }
    }
}
