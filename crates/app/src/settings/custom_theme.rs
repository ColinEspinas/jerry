//! User-authored custom themes (GitHub issue #5) - real, additional theme definitions loaded
//! from disk, layered on top of [`crate::settings::state::THEME_DEFS`]' six built-in themes
//! rather than replacing them.
//!
//! ## Real file format
//!
//! Same `~/.config/jerry` config directory `crate::settings::store::settings_toml_path` already
//! uses, and the same TOML format `settings.toml` itself is written in (see that module's own
//! "TOML is the real file" docs for why TOML, not a second format, is the right choice here too)
//! - one file per theme, at `~/.config/jerry/themes/<slug>.toml`:
//!
//! ```toml
//! name = "Midnight Coral"
//! subtitle = "warm accent, dark base"
//! background   = "#0c0d10"
//! panel        = "#181a1e"
//! accent_green = "#5cb87f"
//! accent_amber = "#e2a336"
//! accent_blue  = "#e07a5f"
//! ```
//!
//! The five colour fields are exactly [`crate::settings::state::ThemeDef::swatches`]' own
//! `[background, panel, green-ish, amber-ish, blue-ish]` shape (see `crate::theme::derive_shift`'s
//! docs for what each position means and how the *rest* of the app's ~200 colour tokens are
//! derived from them) - a user hand-authoring one of these files supplies exactly the same five
//! swatches a built-in [`crate::settings::state::ThemeDef`] does, not a from-scratch 200-token
//! palette. This is a deliberate, honestly-scoped choice: [`CustomTheme`] plugs into the exact
//! same derivation machinery every built-in theme already goes through
//! (`crate::theme::set_current_custom_theme`), so a hand-written file re-skins the *whole* app,
//! not just a couple of preview swatches.
//!
//! Hex colours are `#rrggbb` (a leading `#`, exactly six hex digits) - `#rgb` shorthand, alpha
//! channels, and named CSS colours are all rejected with a real, specific
//! [`ThemeFileError::InvalidColor`] rather than guessed at.
//!
//! ## Icon colours ride along for free; there is no separate icon-pack loader
//!
//! This app's own "icons" (`crate::sidebar::render::render_folder_icon`/`render_lang_chip`) are
//! not a glyph/image-asset pack system - they're `div`-composed rectangles and 1-3 letter text
//! labels coloured entirely by ordinary [`crate::theme::ColorToken`]s
//! (`theme::surface::CHIP_NEUTRAL`, `theme::text::FAINT`/`GHOST`, `theme::lang::*`). Since every
//! one of those tokens already resolves through the same live theme selection every other token
//! does, a custom theme automatically re-colours the folder icon and every language chip too -
//! no second mechanism needed. This is the honest scope for "custom icon packs" in this pass -
//! see `BUILD-LOG.md`'s GitHub issue #5 entry for why: there is no image/glyph-asset loading
//! surface anywhere in this app for a real swappable-*pack* system to attach to, so building one
//! would mean inventing a mechanism nothing else in the app has a use for yet.
//!
//! ## Storage: one file per theme, not one combined file
//!
//! Deliberately one file per theme (not a single `themes.toml` list) so [`export_theme_to_path`]
//! can hand a user exactly one shareable file and [`import_theme_file`] can validate and adopt
//! exactly one at a time - matching how someone would actually receive a theme from another user
//! (a single attached/downloaded `.toml` file), and letting [`load_custom_themes_from_dir`] skip
//! one malformed file without losing every other already-working custom theme in the directory.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::settings::state::THEME_DEFS;

/// A defensive upper bound on a single theme file's size - see
/// [`load_custom_themes_from_dir`]'s own docs for why. [`import_theme_file`] enforces this same
/// cap against the *source* file it's about to import, not just files already sitting in a
/// custom-themes directory - a concurrency incident on this branch (two agent sessions
/// accidentally run against the same shared worktree) lost the original fix for this gap before
/// it was ever committed; this is that fix, re-implemented and verified against the real
/// committed diff rather than taken on trust.
const MAX_THEME_FILE_BYTES: u64 = 64 * 1024;

/// Jerry Dark's own five swatches, transcribed verbatim from `assets/themes/jerry-dark.toml` -
/// pinned as this module's own real `u32` values rather than re-derived from
/// `crate::settings::state::THEME_DEFS` at runtime. That distinction matters: `THEME_DEFS` is a
/// `std::sync::LazyLock` whose own initializer parses (and therefore validates - see
/// [`CustomThemeFile::validate_with_builtin_check`]) all six built-in theme files, so a
/// readability check reachable from that validation path that itself read `THEME_DEFS` would
/// deadlock on `std::sync::Once` the first time any code touched `THEME_DEFS` at all (a real,
/// reproduced hang during this branch's own history, not a hypothetical) - `THEME_DEFS`'s
/// initializer would be blocked waiting on a readability check that is itself blocked waiting for
/// `THEME_DEFS` to finish initializing. Using this pinned copy instead sidesteps that entirely.
///
/// `custom_theme::tests::
/// jerry_dark_baseline_swatches_const_matches_the_real_initialized_theme_defs_0_swatches` is the
/// real regression test keeping this copy honest against `THEME_DEFS[0]`'s actual initialized
/// value - safe to call from an ordinary `#[test]` (never from inside another `LazyLock`'s own
/// initializer), so a future edit to Jerry Dark's own swatches can't silently desync the two
/// without a test catching it.
const JERRY_DARK_BASELINE_SWATCHES: [u32; 5] = [0x0e0f11, 0x1a1e21, 0x5cb87f, 0xe2a336, 0x74ade8];

/// Standard ITU-R BT.709 luma weighting - the same "how bright does this actually look" mix most
/// contrast-ratio formulas use, not a naive `(r+g+b)/3` average that would treat blue as visually
/// as bright as green. Scaled to a per-mille (0-1000) `u32` rather than left as a bare `f64` so
/// [`ThemeFileError::LowReadability`] can stay `#[derive(Eq)]` like every other variant (bare
/// `f64` cannot implement `Eq`), and so tests compare exact integers instead of float epsilons.
fn relative_luma_per_mille(hex: u32) -> u32 {
    let r = ((hex >> 16) & 0xff) as f64 / 255.0;
    let g = ((hex >> 8) & 0xff) as f64 / 255.0;
    let b = (hex & 0xff) as f64 / 255.0;
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    (luma * 1000.0).round() as u32
}

/// The real, concrete readability signal this module checks: how far apart `background` and
/// `panel` actually look, in perceptual brightness - a `panel` that's barely distinguishable from
/// `background` (or identical to it) is unreadable regardless of what the three accent colours
/// are. `u32::abs_diff` rather than signed subtraction since a lighter-panel-on-dark-background
/// theme and a darker-panel-on-light-background theme (e.g. "Paper") are equally valid - only the
/// magnitude of the difference matters, not its sign.
fn panel_background_luma_delta_per_mille(swatches: &[u32; 5]) -> u32 {
    relative_luma_per_mille(swatches[0]).abs_diff(relative_luma_per_mille(swatches[1]))
}

/// The minimum real `panel`-vs-`background` luma difference (see
/// [`panel_background_luma_delta_per_mille`]) [`CustomThemeFile::validate_with_builtin_check`]
/// requires before accepting any theme, built-in or custom - half of
/// [`JERRY_DARK_BASELINE_SWATCHES`]' own real shipped contrast. Half, not the full baseline
/// value, so this is a real but generous floor: every one of the six built-in theme files clears
/// it with room to spare (`custom_theme::tests::every_built_in_theme_clears_the_readability_floor`;
/// the tightest of the six, Slate, clears it by roughly 1.4x) while still catching the concrete
/// unreadable-theme case this exists for: a hand-authored `panel` that's the same colour as
/// `background`, or only a couple of hex digits off from it.
fn readability_floor_per_mille() -> u32 {
    panel_background_luma_delta_per_mille(&JERRY_DARK_BASELINE_SWATCHES) / 2
}

/// The real on-disk shape of one `~/.config/jerry/themes/<slug>.toml` file - see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomThemeFile {
    pub name: String,
    #[serde(default)]
    pub subtitle: String,
    pub background: String,
    pub panel: String,
    pub accent_green: String,
    pub accent_amber: String,
    pub accent_blue: String,
    /// GitHub issue #141's real, optional per-scope syntax overrides - a `[syntax]` TOML table
    /// keyed by `crate::code_surface::code_view::HighlightKind::name`, e.g.:
    ///
    /// ```toml
    /// [syntax]
    /// keyword = "#ff79c6"
    /// string = "#f1fa8c"
    /// comment = "#6272a4"
    /// ```
    ///
    /// Deliberately additive, not a replacement for the five swatches above: every existing
    /// hand-authored or plain-TOML-imported theme file simply has no `[syntax]` table at all
    /// (`#[serde(default)]` makes that a real, empty map, not a parse failure), so its syntax
    /// colours keep coming from the same whole-app HSL derivation they always have - this table
    /// only ever *adds* real, individually-picked colours on top, never subtracts the fallback.
    /// Omitted entirely from a written file when empty (`skip_serializing_if`), so re-exporting
    /// an old-format theme produces byte-for-byte the same shape it always did.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub syntax: std::collections::HashMap<String, String>,
}

/// A [`CustomThemeFile`] that has already been validated - real hex colours, a non-empty name
/// that doesn't collide with a built-in theme - and is safe to hand straight to
/// `crate::theme::set_current_custom_theme`/[`crate::theme::set_current_syntax_overrides`] or
/// render as a Themes-page card.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomTheme {
    pub name: String,
    pub subtitle: String,
    /// `[background, panel, green-ish, amber-ish, blue-ish]` - see the module docs.
    pub swatches: [u32; 5],
    /// GitHub issue #141: this theme's own real, validated per-scope syntax colours - every
    /// `CustomThemeFile::syntax` entry, parsed into a real
    /// `crate::code_surface::code_view::HighlightKind` key and `Rgba` value. Empty for every
    /// theme with no `[syntax]` table (which is every theme predating this issue) - see
    /// [`CustomThemeFile::syntax`]'s own docs.
    pub syntax_overrides:
        std::collections::HashMap<crate::code_surface::code_view::HighlightKind, gpui::Rgba>,
    /// The real file this theme was loaded from/written to. `None` only for a value built
    /// in-memory without ever touching disk (no real production path constructs one that way,
    /// but keeps this struct honestly total rather than assuming every caller has a path).
    pub source_path: Option<PathBuf>,
}

/// Every real, specific way a theme file can fail to load - shown verbatim to the user rather
/// than collapsed into one generic "invalid theme" message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeFileError {
    Io(String),
    Parse(String),
    EmptyName,
    InvalidColor {
        field: &'static str,
        value: String,
    },
    NameCollidesWithBuiltin(String),
    /// `panel` is too close in perceptual brightness to `background` to read comfortably - see
    /// [`readability_floor_per_mille`]'s own docs for how the floor is derived.
    LowReadability {
        delta_per_mille: u32,
        floor_per_mille: u32,
    },
    /// The *source* file [`import_theme_file`] was asked to import is over
    /// [`MAX_THEME_FILE_BYTES`] - checked before that file is ever read or written, the same real
    /// cap [`load_custom_themes_from_dir`] already enforces against files already on disk.
    TooLarge {
        bytes: u64,
        max_bytes: u64,
    },
    /// GitHub issue #141: a `[syntax]` table key that isn't a real
    /// `crate::code_surface::code_view::HighlightKind::name` - a real, specific rejection rather
    /// than silently ignoring what's most likely a typo in a hand-authored file.
    UnknownSyntaxKey(String),
    /// GitHub issue #141: a `[syntax]` table value that isn't a real `#rrggbb` colour - the same
    /// shape [`InvalidColor`](Self::InvalidColor) enforces for the five base swatches, just with
    /// an owned `key` (a `[syntax]` table key is user-authored text, not one of this struct's own
    /// fixed `&'static str` field names).
    InvalidSyntaxColor {
        key: String,
        value: String,
    },
}

impl std::fmt::Display for ThemeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeFileError::Io(msg) => write!(f, "couldn't read the theme file: {msg}"),
            ThemeFileError::Parse(msg) => write!(f, "not a valid theme file: {msg}"),
            ThemeFileError::EmptyName => write!(f, "a theme file needs a non-empty `name`"),
            ThemeFileError::InvalidColor { field, value } => write!(
                f,
                "`{field}` is not a real colour (\"{value}\") - expected `#rrggbb`"
            ),
            ThemeFileError::NameCollidesWithBuiltin(name) => write!(
                f,
                "\"{name}\" is already a built-in theme name - choose a different name"
            ),
            ThemeFileError::LowReadability {
                delta_per_mille,
                floor_per_mille,
            } => write!(
                f,
                "`panel` is too close in brightness to `background` to read comfortably \
                 ({delta_per_mille} per-mille luma difference, need at least {floor_per_mille})"
            ),
            ThemeFileError::TooLarge { bytes, max_bytes } => write!(
                f,
                "theme file is {bytes} bytes, over the {max_bytes}-byte limit for a theme file"
            ),
            ThemeFileError::UnknownSyntaxKey(key) => write!(
                f,
                "`[syntax]` names \"{key}\", which isn't a real syntax colour this app knows \
                 about - see the docs for the full list of real names"
            ),
            ThemeFileError::InvalidSyntaxColor { key, value } => write!(
                f,
                "`[syntax].{key}` is not a real colour (\"{value}\") - expected `#rrggbb`"
            ),
        }
    }
}

/// Parses `#rrggbb` (exactly a `#` plus six hex digits) into a `0xrrggbb` value - the same shape
/// [`crate::settings::state::ThemeDef::swatches`] literals already use, just written as a string
/// a hand-authored file can hold. Deliberately narrower than CSS: no `#rgb` shorthand, no alpha,
/// no named colours - see the module docs for why.
fn parse_hex_color(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

fn format_hex_color(value: u32) -> String {
    format!("#{:06x}", value & 0x00ff_ffff)
}

/// `0xrrggbb` -> a real, opaque `gpui::Rgba` - the same byte-extraction shape
/// `crate::theme::hex` uses for its own `ColorToken` constants, reimplemented here rather than
/// exposed from that module (whose own `hex` is a private `const fn`, and this module has no
/// real need for the rest of `ColorToken`'s machinery - a syntax override is already a real,
/// final colour, not a token to be re-derived).
fn hex_to_rgba(value: u32) -> gpui::Rgba {
    gpui::Rgba {
        r: ((value >> 16) & 0xff) as f32 / 255.0,
        g: ((value >> 8) & 0xff) as f32 / 255.0,
        b: (value & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// The inverse of [`hex_to_rgba`] - used by [`CustomTheme::to_file`] to write a syntax
/// override's real `Rgba` back out as `0xrrggbb`.
fn rgba_to_hex(color: gpui::Rgba) -> u32 {
    let channel = |value: f32| ((value.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

/// Filesystem-safe, lowercase, hyphenated identifier for `name` - used for the `<slug>.toml`
/// filename [`import_theme_file`] derives from a theme's own display name. Never empty: an
/// all-punctuation name falls back to `"theme"`. `pub(crate)`, not private: an adversarial audit
/// caught `crate::settings::render::AdeApp::start_export_custom_theme`'s own suggested-filename
/// computation re-implementing this by hand (missing the empty-segment collapse and the
/// `"theme"` fallback, so the two could suggest different names for the same theme) - reusing
/// this one real implementation is what fixed it.
pub(crate) fn slugify(name: &str) -> String {
    let collapsed: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = collapsed
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "theme".to_string()
    } else {
        slug
    }
}

impl CustomThemeFile {
    /// Validates `self` into a real [`CustomTheme`] - every hex colour must actually parse, the
    /// name must be non-empty (after trimming), and (checked against
    /// [`crate::settings::state::THEME_DEFS`], not a second hand-copied name list) must not
    /// collide with a built-in theme's own name - a custom theme silently shadowing e.g. "Jerry
    /// Dark" would be confusing at best, and would also make `crate::root::AdeApp::
    /// apply_theme_selection`'s "which one did the user mean" lookup ambiguous. A thin wrapper
    /// over [`Self::validate_with_builtin_check`] with the collision check always on - see that
    /// function's own docs for the one real caller that needs it off.
    pub fn validate(&self) -> Result<CustomTheme, ThemeFileError> {
        self.validate_with_builtin_check(true)
    }

    /// The real, shared validation core [`Self::validate`] and [`parse_builtin_theme_file_str`]
    /// both go through - not two independently-maintained checks. `check_builtin_collision` is
    /// `false` for exactly one real caller: [`parse_builtin_theme_file_str`], parsing the six
    /// built-in themes' own embedded files while `crate::settings::state::THEME_DEFS` (a
    /// `std::sync::LazyLock`) is itself still being computed *from* those same files - checking a
    /// built-in theme's name against `THEME_DEFS` at that point would read a value still in the
    /// middle of being initialized. Every other check (non-empty name, real `#rrggbb` colours)
    /// still runs regardless - see `custom_theme::tests::
    /// validate_with_builtin_check_false_skips_only_the_collision_check_not_the_others`. Built-in
    /// name uniqueness is instead guarded the ordinary way, by
    /// `crate::settings::state::tests::every_theme_def_name_is_unique_and_jerry_dark_is_the_default_named_theme`.
    pub(crate) fn validate_with_builtin_check(
        &self,
        check_builtin_collision: bool,
    ) -> Result<CustomTheme, ThemeFileError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(ThemeFileError::EmptyName);
        }
        if check_builtin_collision && THEME_DEFS.iter().any(|def| def.name == name) {
            return Err(ThemeFileError::NameCollidesWithBuiltin(name.to_string()));
        }
        let parse = |field: &'static str, value: &str| -> Result<u32, ThemeFileError> {
            parse_hex_color(value).ok_or_else(|| ThemeFileError::InvalidColor {
                field,
                value: value.to_string(),
            })
        };
        let swatches = [
            parse("background", &self.background)?,
            parse("panel", &self.panel)?,
            parse("accent_green", &self.accent_green)?,
            parse("accent_amber", &self.accent_amber)?,
            parse("accent_blue", &self.accent_blue)?,
        ];
        // Readability floor - deliberately checked against a pinned `JERRY_DARK_BASELINE_SWATCHES`
        // const, never `crate::settings::state::THEME_DEFS`: this function runs for every built-in
        // theme file too (via `parse_builtin_theme_file_str`, called from *inside* `THEME_DEFS`'s
        // own `LazyLock` initializer), so reading `THEME_DEFS` here would deadlock - see
        // `JERRY_DARK_BASELINE_SWATCHES`'s own docs.
        let delta = panel_background_luma_delta_per_mille(&swatches);
        let floor = readability_floor_per_mille();
        if delta < floor {
            return Err(ThemeFileError::LowReadability {
                delta_per_mille: delta,
                floor_per_mille: floor,
            });
        }
        let mut syntax_overrides = std::collections::HashMap::with_capacity(self.syntax.len());
        for (key, value) in &self.syntax {
            let kind = crate::code_surface::code_view::HighlightKind::from_name(key)
                .ok_or_else(|| ThemeFileError::UnknownSyntaxKey(key.clone()))?;
            let hex = parse_hex_color(value).ok_or_else(|| ThemeFileError::InvalidSyntaxColor {
                key: key.clone(),
                value: value.clone(),
            })?;
            syntax_overrides.insert(kind, hex_to_rgba(hex));
        }
        Ok(CustomTheme {
            name: name.to_string(),
            subtitle: self.subtitle.trim().to_string(),
            swatches,
            syntax_overrides,
            source_path: None,
        })
    }
}

/// Parses and validates one built-in theme's embedded TOML text (one of the six real
/// `assets/themes/*.toml` files `crate::settings::state::THEME_DEFS` embeds via `include_str!`)
/// through the exact same [`CustomThemeFile`] deserialization and
/// [`CustomThemeFile::validate_with_builtin_check`] validation core [`parse_theme_file_str`] uses
/// for a user's own disk-loaded file - not a second, parallel parser. Only the built-in-collision
/// half of that check is skipped (see [`CustomThemeFile::validate_with_builtin_check`]'s own
/// docs for why that one specific check is self-referential here).
///
/// Panics on a malformed file. That's a real, deliberate choice, not a shortcut: these six files
/// are compiled into the binary from this repository at build time (not user input reachable at
/// runtime), and `custom_theme::tests::
/// parse_builtin_theme_file_str_parses_every_embedded_built_in_theme_file_into_the_exact_documented_swatches`
/// already proves every one of them parses and validates cleanly - a failure here could only mean
/// a real, committed asset went bad, which should fail loudly (a panic surfaces immediately in
/// `cargo test`/at first real use) rather than silently reducing the Themes page below six cards.
pub(crate) fn parse_builtin_theme_file_str(contents: &str) -> CustomTheme {
    let file: CustomThemeFile = toml::from_str(contents)
        .expect("a built-in theme file under assets/themes/ failed to parse as TOML");
    file.validate_with_builtin_check(false)
        .expect("a built-in theme file under assets/themes/ failed validation")
}

impl CustomTheme {
    /// The inverse of [`CustomThemeFile::validate`] - a real, re-parseable form.
    pub fn to_file(&self) -> CustomThemeFile {
        CustomThemeFile {
            name: self.name.clone(),
            subtitle: self.subtitle.clone(),
            background: format_hex_color(self.swatches[0]),
            panel: format_hex_color(self.swatches[1]),
            accent_green: format_hex_color(self.swatches[2]),
            accent_amber: format_hex_color(self.swatches[3]),
            accent_blue: format_hex_color(self.swatches[4]),
            syntax: self
                .syntax_overrides
                .iter()
                .map(|(kind, color)| {
                    (
                        kind.name().to_string(),
                        format_hex_color(rgba_to_hex(*color)),
                    )
                })
                .collect(),
        }
    }

    /// The same `toml::to_string_pretty` serializer
    /// [`crate::settings::store::Settings::to_toml_string`] uses - the shareable form
    /// [`export_theme_to_path`]/[`import_theme_file`] write to disk.
    ///
    /// `expect`, not `unwrap_or_default()` (an adversarial audit flagged the original
    /// `unwrap_or_default()` as a real "hollow success" shape: a serialization failure would have
    /// silently produced an empty string, which `export_theme_to_path` would then have happily
    /// written as a zero-byte file while still reporting success to the user). `CustomThemeFile`
    /// is a plain struct of five `String` fields wrapped in another plain struct - there is no
    /// `NaN`, no non-string map key, nothing `toml::to_string_pretty` can genuinely fail to
    /// encode here - so a real failure would mean this type grew a field that breaks that
    /// invariant, which is exactly the kind of regression that should panic loudly in tests
    /// rather than quietly ship an empty theme file.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(&self.to_file())
            .expect("CustomThemeFile is a plain struct of Strings - TOML serialization cannot fail")
    }
}

/// The real `themes/` directory sibling of a given `settings_path` (e.g.
/// `~/.config/jerry/settings.toml` -> `~/.config/jerry/themes`) - mirrors
/// `crate::sidebar::fold_state::fold_state_path_for`'s own "derive from the settings path this
/// `AdeApp` instance was actually given, never `$HOME` directly" convention, for the exact same
/// reason: `crate::root::AdeApp::new_with_settings` (every `#[gpui::test]`'s shared entry point,
/// not just production) calls this unconditionally at construction time, so resolving straight
/// from `$HOME` here would mean every test in this crate reads (and a real "Import theme" action
/// would write into) whatever `~/.config/jerry/themes` happens to exist on the machine actually
/// running the tests - a real, previously-caught-in-review test-isolation bug for exactly this
/// reason. Production's own real settings path (`crate::settings::store::settings_toml_path`)
/// still resolves to `~/.config/jerry/settings.toml` off `$HOME` same as ever; this function only
/// asks "what's this *specific* settings path's own sibling themes directory", the same question
/// `fold_state_path_for` answers for its own file.
pub fn custom_themes_dir_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join("themes"),
        None => PathBuf::from("themes"),
    }
}

/// Parses and validates one theme file's raw text - the real shared step both
/// [`load_custom_themes_from_dir`] (an existing file on disk) and [`import_theme_file`] (a
/// freshly picked one, not yet copied anywhere) go through, so the two can never validate
/// differently.
pub fn parse_theme_file_str(contents: &str) -> Result<CustomTheme, ThemeFileError> {
    let file: CustomThemeFile =
        toml::from_str(contents).map_err(|err| ThemeFileError::Parse(err.to_string()))?;
    file.validate()
}

/// Loads every real, validatable `*.toml` file directly inside `dir` (non-recursive) as a
/// [`CustomTheme`]. A file that fails to read, parse, or validate is skipped - not silently: its
/// real error, prefixed with the file name, is appended to the returned tuple's second element
/// so a caller can surface it (`crate::root::AdeApp::custom_theme_load_errors`) rather than a bad
/// hand-edit quietly vanishing.
///
/// Two *different* files that both validate to the same theme `name` (a real, reachable case: a
/// hand-authored file sitting next to one [`import_theme_file`] wrote for a re-import of "the
/// same" theme under a different original filename) are also a real, reported skip, not two
/// identically-named cards - `crate::settings::render::AdeApp::apply_theme_selection`'s own
/// name-keyed lookup can only ever resolve to one of them anyway, and two GPUI elements sharing
/// one `.id()` (`crate::settings::render::AdeApp::render_theme_card`'s
/// `settings-theme-card-{name}`) is a real rendering bug, not a cosmetic one. Files are processed
/// in a sorted-by-path order first, so which one "wins" a name collision is deterministic rather
/// than dependent on `std::fs::read_dir`'s own unspecified iteration order.
pub fn load_custom_themes_from_dir(dir: &Path) -> (Vec<CustomTheme>, Vec<String>) {
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), errors);
    };
    let mut candidate_paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        // Case-insensitive (`Foo.TOML` is a real, common Windows/macOS-authored spelling) - an
        // adversarial audit caught the original exact-lowercase `== Some("toml")` silently
        // ignoring such a file with no reported error at all.
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
        })
        .collect();
    candidate_paths.sort();

    let mut themes: Vec<CustomTheme> = Vec::new();
    for path in candidate_paths {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        // A real, cheap defensive cap (this is real foreground-thread I/O at `AdeApp`
        // construction time, per `crate::root::AdeApp::new_with_settings`'s own docs) - an
        // adversarial audit noted a pathologically large file in this directory would otherwise
        // stall every window's startup with no bound at all. A hand-authored five-swatch theme
        // file is a few hundred bytes; 64 KiB is generous headroom, not a tight fit.
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_THEME_FILE_BYTES => {
                errors.push(format!(
                    "{file_name}: {} bytes exceeds the {MAX_THEME_FILE_BYTES}-byte limit for a theme file - skipping",
                    metadata.len()
                ));
                continue;
            }
            _ => {}
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => match parse_theme_file_str(&contents) {
                Ok(mut theme) => {
                    if let Some(existing) = themes.iter().find(|other| other.name == theme.name) {
                        let existing_file = existing
                            .source_path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        errors.push(format!(
                            "{file_name}: a theme named \"{}\" was already loaded from {existing_file} - \
                             skipping this duplicate",
                            theme.name
                        ));
                        continue;
                    }
                    theme.source_path = Some(path.clone());
                    themes.push(theme);
                }
                Err(err) => errors.push(format!("{file_name}: {err}")),
            },
            Err(err) => errors.push(format!("{file_name}: {err}")),
        }
    }
    themes.sort_by(|a, b| a.name.cmp(&b.name));
    (themes, errors)
}

/// Real import: reads and validates the file at `source_path` (any real path on disk, e.g. one a
/// user just picked in a real file-open dialog - `crate::settings::render::AdeApp::
/// import_custom_theme_from_path` is the one real caller), then writes its *validated,
/// re-serialized* form (not a byte-for-byte copy - see [`CustomTheme::to_toml_string`], so a
/// source file with extra whitespace or key ordering still lands as a canonical file) into
/// `dest_dir`. A malformed source file is rejected with a real [`ThemeFileError`] and nothing is
/// written - the whole point of validating before copying.
///
/// The destination filename ([`non_colliding_dest_path`]) is the theme's own slug when that's
/// either free or already holds *this same* theme (an intentional "re-import to update" - the
/// second import of a theme with the same `name` really does overwrite its own previous file) -
/// but never a *different*, unrelated theme's file: an adversarial audit caught the first version
/// of this function joining `dest_dir` with the bare slug unconditionally, so importing a theme
/// whose slug happened to collide with an existing, differently-named file silently destroyed it.
pub fn import_theme_file(
    source_path: &Path,
    dest_dir: &Path,
) -> Result<CustomTheme, ThemeFileError> {
    // The same real cap `load_custom_themes_from_dir` already enforces against files already
    // sitting in a custom-themes directory, applied here against the *source* file before it's
    // ever read into memory or written anywhere - a pathologically large "theme" file picked in a
    // real file-open dialog should be rejected with a real, specific error, not silently read in
    // full (or truncated) first.
    let metadata =
        std::fs::metadata(source_path).map_err(|err| ThemeFileError::Io(err.to_string()))?;
    if metadata.len() > MAX_THEME_FILE_BYTES {
        return Err(ThemeFileError::TooLarge {
            bytes: metadata.len(),
            max_bytes: MAX_THEME_FILE_BYTES,
        });
    }
    let contents =
        std::fs::read_to_string(source_path).map_err(|err| ThemeFileError::Io(err.to_string()))?;
    let theme = parse_theme_file_str(&contents)?;
    validate_and_write(theme.to_file(), dest_dir)
}

/// The real, shared "validate, then write a canonical copy into `dest_dir`" tail
/// [`import_theme_file`] (a plain-TOML source) and
/// `crate::settings::vscode_theme`'s own import glue (a converted-from-JSON source) both need -
/// re-validated here even though a caller may have already validated once (cheap, and keeps this
/// function honest as the one real place a theme actually lands on disk, rather than trusting a
/// caller's own possibly-stale validation).
pub(crate) fn validate_and_write(
    file: CustomThemeFile,
    dest_dir: &Path,
) -> Result<CustomTheme, ThemeFileError> {
    let mut theme = file.validate()?;
    std::fs::create_dir_all(dest_dir).map_err(|err| ThemeFileError::Io(err.to_string()))?;
    let dest_path = non_colliding_dest_path(dest_dir, &theme.name);
    std::fs::write(&dest_path, theme.to_toml_string())
        .map_err(|err| ThemeFileError::Io(err.to_string()))?;
    theme.source_path = Some(dest_path);
    Ok(theme)
}

/// Picks a real, collision-safe destination path for a theme named `name` inside `dest_dir` - see
/// [`import_theme_file`]'s own docs for the bug this exists to fix. Starts from the plain
/// `{slugify(name)}.toml` path; if something is already there, reads and validates it - an exact
/// `name` match means it's genuinely the same theme being re-imported (real "update" behaviour,
/// intentionally overwritten - including when that path is a real symlink to a legitimate theme
/// file elsewhere, a genuine way to share one across machines), anything else (a different theme,
/// a file that doesn't even parse as one, or a *dangling* symlink) means the slug is taken by
/// something unrelated, so this tries `{slug}-2.toml`, `{slug}-3.toml`, ... until it finds either
/// a free path or one already holding this same theme.
///
/// `symlink_metadata`, not `Path::exists` - an adversarial audit caught the original
/// `candidate.exists()` here as a real vulnerability, not just a style nit: `exists()` follows
/// symlinks and reports `false` for a *dangling* `{slug}.toml -> /some/other/path` symlink sitting
/// in `dest_dir`, so this loop would treat that path as free, and [`import_theme_file`]'s own
/// `std::fs::write` (which also follows symlinks) would then write straight through it into
/// whatever it points at - a real clobber of an unrelated file, just staged via a symlink instead
/// of a same-named regular file. `symlink_metadata` reports the symlink's own presence without
/// following it, so a dangling (or unrelated-target) symlink here is treated as a real collision
/// like any other occupied path, while a symlink that genuinely points at a file already holding
/// *this same* theme is still correctly reused below.
fn non_colliding_dest_path(dest_dir: &Path, name: &str) -> PathBuf {
    let base_slug = slugify(name);
    let mut candidate = dest_dir.join(format!("{base_slug}.toml"));
    let mut suffix = 2;
    while candidate.symlink_metadata().is_ok() {
        let same_theme = std::fs::read_to_string(&candidate)
            .ok()
            .and_then(|contents| parse_theme_file_str(&contents).ok())
            .is_some_and(|existing| existing.name == name);
        if same_theme {
            break;
        }
        candidate = dest_dir.join(format!("{base_slug}-{suffix}.toml"));
        suffix += 1;
    }
    candidate
}

/// The real, well-commented starting-point template a user can copy to author their own theme -
/// checked in at `assets/themes/template.toml` and embedded here via `include_str!` so the file a
/// user finds in the repository and the one [`write_template_theme`]'s "New from template"
/// Themes-page action writes are the literal same bytes, never two independently-maintained
/// copies that could drift apart. Deliberately *not* one of [`crate::settings::state::THEME_DEFS`]'
/// own six `include_str!`s: that list names all six built-in theme files explicitly, and this
/// isn't a seventh built-in theme - it lives in the same `assets/themes/` directory purely because
/// that's this repository's one real home for theme-shaped `.toml` files, not because it's wired
/// into `THEME_DEFS`.
pub const CUSTOM_THEME_TEMPLATE_TOML: &str =
    include_str!("../../../../assets/themes/template.toml");

/// Writes the real template ([`CUSTOM_THEME_TEMPLATE_TOML`]) into `dest_dir` - the Themes page's
/// "New from template" action's one real caller
/// (`crate::settings::render::AdeApp::start_create_theme_from_template`). Validates through the
/// same [`parse_theme_file_str`] core [`import_theme_file`] uses (never written unvalidated - if
/// this template itself ever regressed into something that fails its own validation, this would
/// fail loudly rather than silently hand a user a broken file), then writes the template's own
/// literal bytes - comments and all - not a re-serialized [`CustomTheme::to_toml_string`] copy,
/// unlike [`import_theme_file`]: the whole point of a "well-commented template" is that a user who
/// clicks the button gets the same explanatory comments as one who copies the file straight out of
/// the repository, not a canonicalized file stripped down to five bare key-value lines.
///
/// Uses the same [`non_colliding_dest_path`] collision handling [`import_theme_file`] does: a
/// second click reuses (refreshes) the one file this already wrote, since the template's own
/// `name` never changes between clicks, rather than spawning a `-2` sibling every time - *as long
/// as that file's contents are still the pristine, unedited template*. An adversarial audit
/// caught the original version of this function always overwriting that path unconditionally: a
/// user who clicked "New from template", edited the resulting file's colours in place (keeping
/// its default `name`), then clicked "New from template" again - e.g. wanting a *second* fresh
/// theme to start a new one from - would have silently lost those edits with no confirmation,
/// unlike every other destructive action in this module (`execute_remove_custom_theme` requires
/// an arm-then-confirm double click). Fixed by checking the existing file's real bytes first: if
/// they've diverged from [`CUSTOM_THEME_TEMPLATE_TOML`], this leaves the file untouched and hands
/// back the user's own edited theme instead of clobbering it; only a byte-identical (never
/// touched since the last write) file is refreshed.
pub fn write_template_theme(dest_dir: &Path) -> Result<CustomTheme, ThemeFileError> {
    let mut theme = parse_theme_file_str(CUSTOM_THEME_TEMPLATE_TOML)?;
    std::fs::create_dir_all(dest_dir).map_err(|err| ThemeFileError::Io(err.to_string()))?;
    let dest_path = non_colliding_dest_path(dest_dir, &theme.name);
    if let Ok(existing_contents) = std::fs::read_to_string(&dest_path) {
        if existing_contents != CUSTOM_THEME_TEMPLATE_TOML {
            // The user has edited this file since it was created from the template - do not
            // overwrite their real edits. Hand back their existing theme as-is.
            let mut existing = parse_theme_file_str(&existing_contents)?;
            existing.source_path = Some(dest_path);
            return Ok(existing);
        }
    }
    std::fs::write(&dest_path, CUSTOM_THEME_TEMPLATE_TOML)
        .map_err(|err| ThemeFileError::Io(err.to_string()))?;
    theme.source_path = Some(dest_path);
    Ok(theme)
}

/// Real export: serializes `theme` (see [`CustomTheme::to_toml_string`]) to `dest_path` - a
/// user-chosen destination (e.g. a real "Save file" dialog's result), not necessarily inside a
/// [`custom_themes_dir_for`] directory at all, since exporting means "give me a shareable file",
/// not "install this theme".
pub fn export_theme_to_path(theme: &CustomTheme, dest_path: &Path) -> io::Result<()> {
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest_path, theme.to_toml_string())
}

/// Removes a loaded custom theme's real backing file - the Themes page's "Remove" action. A
/// no-op `Ok(())` if `theme` has no `source_path` (shouldn't happen for anything the Themes page
/// actually lists, but keeps this honestly total rather than panicking on an `unwrap`).
pub fn remove_custom_theme_file(theme: &CustomTheme) -> io::Result<()> {
    match &theme.source_path {
        Some(path) => std::fs::remove_file(path),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_file() -> CustomThemeFile {
        CustomThemeFile {
            name: "Midnight Coral".to_string(),
            subtitle: "warm accent".to_string(),
            background: "#0c0d10".to_string(),
            panel: "#181a1e".to_string(),
            accent_green: "#5cb87f".to_string(),
            accent_amber: "#e2a336".to_string(),
            accent_blue: "#e07a5f".to_string(),
            syntax: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn a_well_formed_file_validates_into_the_real_swatches_in_order() {
        let theme = valid_file().validate().expect("should validate");
        assert_eq!(theme.name, "Midnight Coral");
        assert_eq!(theme.subtitle, "warm accent");
        assert_eq!(
            theme.swatches,
            [0x0c0d10, 0x181a1e, 0x5cb87f, 0xe2a336, 0xe07a5f]
        );
        assert_eq!(theme.source_path, None);
        assert!(
            theme.syntax_overrides.is_empty(),
            "a file with no [syntax] table must validate with a real, empty override map"
        );
    }

    #[test]
    fn a_real_syntax_table_validates_into_the_real_highlight_kind_keyed_overrides() {
        let mut file = valid_file();
        file.syntax
            .insert("keyword".to_string(), "#ff79c6".to_string());
        file.syntax
            .insert("string".to_string(), "#f1fa8c".to_string());
        let theme = file.validate().expect("should validate");
        assert_eq!(theme.syntax_overrides.len(), 2);
        assert_eq!(
            theme.syntax_overrides[&crate::code_surface::code_view::HighlightKind::Keyword],
            hex_to_rgba(0xff79c6)
        );
        assert_eq!(
            theme.syntax_overrides[&crate::code_surface::code_view::HighlightKind::String],
            hex_to_rgba(0xf1fa8c)
        );
    }

    #[test]
    fn an_unknown_syntax_key_is_a_real_rejection_not_a_silently_ignored_typo() {
        let mut file = valid_file();
        file.syntax
            .insert("keywrod".to_string(), "#ff79c6".to_string());
        assert_eq!(
            file.validate(),
            Err(ThemeFileError::UnknownSyntaxKey("keywrod".to_string()))
        );
    }

    #[test]
    fn an_invalid_syntax_colour_is_a_real_rejection() {
        let mut file = valid_file();
        file.syntax
            .insert("keyword".to_string(), "not-a-color".to_string());
        assert_eq!(
            file.validate(),
            Err(ThemeFileError::InvalidSyntaxColor {
                key: "keyword".to_string(),
                value: "not-a-color".to_string(),
            })
        );
    }

    #[test]
    fn a_theme_with_syntax_overrides_round_trips_through_to_file_and_back() {
        let mut file = valid_file();
        file.syntax
            .insert("keyword".to_string(), "#ff79c6".to_string());
        file.syntax
            .insert("comment".to_string(), "#6272a4".to_string());
        let theme = file.validate().expect("should validate");
        let round_tripped = theme.to_file().validate().expect("should re-validate");
        assert_eq!(round_tripped.syntax_overrides, theme.syntax_overrides);
    }

    #[test]
    fn every_highlight_kind_name_round_trips_through_from_name() {
        for kind in crate::code_surface::code_view::HighlightKind::ALL {
            assert_eq!(
                crate::code_surface::code_view::HighlightKind::from_name(kind.name()),
                Some(kind),
                "HighlightKind::{kind:?}'s own name() must round-trip through from_name()"
            );
        }
        assert_eq!(
            crate::code_surface::code_view::HighlightKind::from_name("not_a_real_kind"),
            None
        );
    }

    #[test]
    fn an_empty_name_is_a_real_rejection_not_a_silent_fallback() {
        let mut file = valid_file();
        file.name = "   ".to_string();
        assert_eq!(file.validate(), Err(ThemeFileError::EmptyName));
    }

    #[test]
    fn a_name_colliding_with_a_builtin_theme_is_rejected() {
        let mut file = valid_file();
        file.name = "Jerry Dark".to_string();
        assert_eq!(
            file.validate(),
            Err(ThemeFileError::NameCollidesWithBuiltin(
                "Jerry Dark".to_string()
            ))
        );
    }

    type FieldSetter = fn(&mut CustomThemeFile, String);

    #[test]
    fn every_real_invalid_color_shape_is_rejected_with_the_offending_field_named() {
        let cases: [(&str, FieldSetter); 5] = [
            ("background", |f, v| f.background = v),
            ("panel", |f, v| f.panel = v),
            ("accent_green", |f, v| f.accent_green = v),
            ("accent_amber", |f, v| f.accent_amber = v),
            ("accent_blue", |f, v| f.accent_blue = v),
        ];
        for (field, set) in cases {
            for bad in ["0c0d10", "#0c0", "#gggggg", "#0c0d1012", "not-a-color", ""] {
                let mut file = valid_file();
                set(&mut file, bad.to_string());
                let err = file.validate().expect_err(&format!(
                    "field {field} with value {bad:?} should have been rejected"
                ));
                assert_eq!(
                    err,
                    ThemeFileError::InvalidColor {
                        field,
                        value: bad.to_string()
                    }
                );
            }
        }
    }

    #[test]
    fn round_trips_through_to_file_and_back() {
        let theme = valid_file().validate().expect("should validate");
        let file = theme.to_file();
        let reparsed = file.validate().expect("re-validated file should parse");
        assert_eq!(theme.name, reparsed.name);
        assert_eq!(theme.subtitle, reparsed.subtitle);
        assert_eq!(theme.swatches, reparsed.swatches);
    }

    #[test]
    fn parse_theme_file_str_rejects_garbage_toml_with_a_real_parse_error() {
        let err = parse_theme_file_str("this is not valid toml {{{").unwrap_err();
        assert!(matches!(err, ThemeFileError::Parse(_)));
    }

    #[test]
    fn parse_theme_file_str_accepts_a_real_well_formed_document() {
        let toml = valid_file().to_toml_string_for_test();
        let theme = parse_theme_file_str(&toml).expect("should parse");
        assert_eq!(theme.name, "Midnight Coral");
    }

    // Test-only helper: `CustomThemeFile` itself has no `to_toml_string` (only the already
    // validated `CustomTheme` does) - this stands in for "a real file on disk", independent of
    // the struct-under-test's own serializer.
    impl CustomThemeFile {
        fn to_toml_string_for_test(&self) -> String {
            toml::to_string_pretty(self).unwrap_or_default()
        }
    }

    #[test]
    fn load_custom_themes_from_dir_skips_one_bad_file_and_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("good.toml"),
            valid_file().to_toml_string_for_test(),
        )
        .expect("write good file");
        std::fs::write(dir.path().join("bad.toml"), "not valid toml {{{").expect("write bad file");
        // Non-`.toml` files are ignored entirely, not treated as errors.
        std::fs::write(dir.path().join("notes.txt"), "hello").expect("write unrelated file");

        let (themes, errors) = load_custom_themes_from_dir(dir.path());

        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].name, "Midnight Coral");
        assert_eq!(
            themes[0].source_path.as_deref(),
            Some(dir.path().join("good.toml").as_path())
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("bad.toml:"));
    }

    #[test]
    fn load_custom_themes_from_dir_on_a_missing_directory_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let (themes, errors) = load_custom_themes_from_dir(&missing);
        assert!(themes.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn load_custom_themes_from_dir_sorts_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut zebra = valid_file();
        zebra.name = "Zebra".to_string();
        let mut apple = valid_file();
        apple.name = "Apple".to_string();
        std::fs::write(dir.path().join("z.toml"), zebra.to_toml_string_for_test()).expect("write");
        std::fs::write(dir.path().join("a.toml"), apple.to_toml_string_for_test()).expect("write");

        let (themes, _) = load_custom_themes_from_dir(dir.path());

        assert_eq!(
            themes.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["Apple", "Zebra"]
        );
    }

    #[test]
    fn load_custom_themes_from_dir_accepts_an_uppercase_toml_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Windows-Authored.TOML"),
            valid_file().to_toml_string_for_test(),
        )
        .expect("write");

        let (themes, errors) = load_custom_themes_from_dir(dir.path());

        assert!(
            errors.is_empty(),
            "a real .TOML file should not be reported as an error"
        );
        assert_eq!(
            themes.len(),
            1,
            "a real .TOML (uppercase) file must still be loaded"
        );
    }

    #[test]
    fn load_custom_themes_from_dir_skips_a_file_over_the_size_cap_with_a_real_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let oversized = "x".repeat((MAX_THEME_FILE_BYTES + 1) as usize);
        std::fs::write(dir.path().join("huge.toml"), oversized).expect("write huge file");

        let (themes, errors) = load_custom_themes_from_dir(dir.path());

        assert!(themes.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("huge.toml:"));
        assert!(errors[0].contains("exceeds"));
    }

    #[test]
    fn import_theme_file_validates_before_writing_and_rejects_a_malformed_source() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        // Missing every required colour field - `toml::from_str::<CustomThemeFile>` fails before
        // `validate()` even runs (`background`/`panel`/... aren't `#[serde(default)]`), so this
        // is a real `Parse` error, not `EmptyName`.
        std::fs::write(&source_path, "name = \"Something\"\n").expect("write malformed source");

        let err = import_theme_file(&source_path, dest_dir.path()).unwrap_err();
        assert!(
            matches!(err, ThemeFileError::Parse(_)),
            "expected a Parse error, got {err:?}"
        );
        assert!(
            std::fs::read_dir(dest_dir.path())
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "a rejected import must not write any file"
        );
    }

    #[test]
    fn import_theme_file_writes_a_real_canonical_file_into_dest_dir() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, valid_file().to_toml_string_for_test()).expect("write");

        let theme = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_eq!(theme.name, "Midnight Coral");
        let expected_path = dest_dir.path().join("midnight-coral.toml");
        assert_eq!(theme.source_path.as_deref(), Some(expected_path.as_path()));
        assert!(expected_path.exists());
        let (loaded, errors) = load_custom_themes_from_dir(dest_dir.path());
        assert!(errors.is_empty());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Midnight Coral");
    }

    /// Regression for a real bug an adversarial audit found: importing a theme whose slug
    /// happens to collide with an *unrelated*, differently-named theme already on disk must
    /// never silently overwrite that file.
    #[test]
    fn import_theme_file_never_clobbers_an_unrelated_theme_with_a_colliding_slug() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");

        // A pre-existing, differently-named theme that happens to slugify to the exact same
        // filename "Midnight Coral" would use.
        let mut ocean = valid_file();
        ocean.name = "Ocean".to_string();
        std::fs::write(
            dest_dir.path().join("midnight-coral.toml"),
            ocean.to_toml_string_for_test(),
        )
        .expect("write pre-existing file");

        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, valid_file().to_toml_string_for_test()).expect("write");

        let imported = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_eq!(imported.name, "Midnight Coral");
        assert_ne!(
            imported.source_path,
            Some(dest_dir.path().join("midnight-coral.toml")),
            "must not have written into Ocean's own file"
        );

        let (loaded, errors) = load_custom_themes_from_dir(dest_dir.path());
        assert!(
            errors.is_empty(),
            "both real files should still load cleanly: {errors:?}"
        );
        let names: Vec<&str> = loaded.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"Ocean"),
            "Ocean's file must survive completely untouched, got: {names:?}"
        );
        assert!(names.contains(&"Midnight Coral"));
    }

    /// Regression for a real, concurrency-incident-lost fix: a *dangling* symlink planted at the
    /// exact path `import_theme_file` would otherwise write to must not be followed. Before this
    /// fix, `non_colliding_dest_path`'s `candidate.exists()` reported `false` for a dangling
    /// symlink (it follows symlinks and the *target* doesn't exist), so the loop treated the path
    /// as free and `std::fs::write` then wrote straight through the symlink into whatever it
    /// pointed at.
    #[cfg(unix)]
    #[test]
    fn import_theme_file_does_not_follow_a_dangling_symlink_planted_at_the_slug_path() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        // The symlink's target deliberately does not exist - a dangling symlink.
        let attacker_target = outside.path().join("does-not-exist-yet.toml");
        std::os::unix::fs::symlink(
            &attacker_target,
            dest_dir.path().join("midnight-coral.toml"),
        )
        .expect("create dangling symlink");

        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, valid_file().to_toml_string_for_test()).expect("write");

        let imported = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_ne!(
            imported.source_path,
            Some(dest_dir.path().join("midnight-coral.toml")),
            "must not have written through the dangling symlink"
        );
        assert!(
            !attacker_target.exists(),
            "must never have created the symlink's target by following it"
        );
    }

    /// A companion guard, not itself a regression test for the dangling-symlink fix (a symlink to
    /// a real, *existing* file was already correctly treated as a collision by the old
    /// `candidate.exists()` check too - `exists()` only misreports a *dangling* symlink as absent,
    /// which is what
    /// [`import_theme_file_does_not_follow_a_dangling_symlink_planted_at_the_slug_path`] actually
    /// regression-tests). Kept anyway as real, independent coverage that a symlink pointing at a
    /// real, *unrelated* (differently-named) theme file is treated as a collision (redirected to a
    /// new candidate name), exactly like an ordinary same-named regular file already is, and that
    /// the unrelated file it points at survives untouched.
    #[cfg(unix)]
    #[test]
    fn import_theme_file_does_not_follow_a_symlink_to_an_unrelated_theme_file() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");

        let mut ocean = valid_file();
        ocean.name = "Ocean".to_string();
        let real_file = outside.path().join("ocean.toml");
        std::fs::write(&real_file, ocean.to_toml_string_for_test()).expect("write real file");
        std::os::unix::fs::symlink(&real_file, dest_dir.path().join("midnight-coral.toml"))
            .expect("create symlink to unrelated theme");

        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, valid_file().to_toml_string_for_test()).expect("write");

        let imported = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_eq!(imported.name, "Midnight Coral");
        assert_ne!(
            imported.source_path,
            Some(dest_dir.path().join("midnight-coral.toml")),
            "must not have written through the symlink into Ocean's real file"
        );
        assert_eq!(
            std::fs::read_to_string(&real_file).expect("Ocean's file should still be readable"),
            ocean.to_toml_string_for_test(),
            "Ocean's real file must survive completely untouched"
        );
    }

    /// The flip side: a symlink that genuinely points at a file already holding *this same*
    /// theme (a real, legitimate way to share one across machines) is a real re-import-to-update,
    /// not a collision - it must be reused, not redirected to a `-2` sibling.
    #[cfg(unix)]
    #[test]
    fn non_colliding_dest_path_reuses_a_symlink_that_points_at_a_file_holding_the_same_theme() {
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let real_file_dir = tempfile::tempdir().expect("tempdir");
        let real_file = real_file_dir.path().join("shared-theme.toml");
        std::fs::write(&real_file, valid_file().to_toml_string_for_test())
            .expect("write real file");
        let symlink_path = dest_dir.path().join("midnight-coral.toml");
        std::os::unix::fs::symlink(&real_file, &symlink_path).expect("create symlink");

        let candidate = non_colliding_dest_path(dest_dir.path(), "Midnight Coral");

        assert_eq!(
            candidate, symlink_path,
            "a symlink to a file already holding this exact theme is a legitimate re-import \
             target, not a collision"
        );
    }

    /// A second import of the *same* theme (by name) is a real, intentional update - it must
    /// land back at the exact same path, not spawn a `-2` sibling.
    #[test]
    fn import_theme_file_reimporting_the_same_theme_overwrites_its_own_file_not_a_new_one() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, valid_file().to_toml_string_for_test()).expect("write");

        let first = import_theme_file(&source_path, dest_dir.path()).expect("first import");

        let mut updated = valid_file();
        updated.background = "#111111".to_string();
        std::fs::write(&source_path, updated.to_toml_string_for_test()).expect("write update");
        let second = import_theme_file(&source_path, dest_dir.path()).expect("second import");

        assert_eq!(first.source_path, second.source_path);
        assert_eq!(second.swatches[0], 0x111111);
        let (loaded, _) = load_custom_themes_from_dir(dest_dir.path());
        assert_eq!(
            loaded.len(),
            1,
            "must not have left a stale duplicate file behind"
        );
    }

    #[test]
    fn import_theme_file_rejects_a_name_colliding_with_a_builtin_and_writes_nothing() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        let mut file = valid_file();
        file.name = "Slate".to_string();
        std::fs::write(&source_path, file.to_toml_string_for_test()).expect("write");

        let err = import_theme_file(&source_path, dest_dir.path()).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::NameCollidesWithBuiltin("Slate".to_string())
        );
        assert!(
            std::fs::read_dir(dest_dir.path())
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "a rejected import must not write any file"
        );
    }

    #[test]
    fn import_theme_file_reports_a_real_io_error_for_a_missing_source() {
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let missing_source = dest_dir.path().join("does-not-exist.toml");
        let err = import_theme_file(&missing_source, dest_dir.path()).unwrap_err();
        assert!(matches!(err, ThemeFileError::Io(_)));
    }

    /// Regression for a real, concurrency-incident-lost fix: [`load_custom_themes_from_dir`]
    /// already capped a directory's own files at [`MAX_THEME_FILE_BYTES`], but
    /// [`import_theme_file`] (the user-facing "import this file" action) had no matching check
    /// against the *source* file it's about to import - an oversized source would previously have
    /// been read into memory in full before validation ever rejected it as malformed TOML. This
    /// must reject it before reading or writing anything at all.
    #[test]
    fn import_theme_file_rejects_an_oversized_source_before_reading_or_writing_it() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("huge.toml");
        let oversized_len = MAX_THEME_FILE_BYTES + 1;
        std::fs::write(&source_path, "x".repeat(oversized_len as usize))
            .expect("write huge source");

        let err = import_theme_file(&source_path, dest_dir.path()).unwrap_err();

        assert_eq!(
            err,
            ThemeFileError::TooLarge {
                bytes: oversized_len,
                max_bytes: MAX_THEME_FILE_BYTES,
            }
        );
        assert!(
            std::fs::read_dir(dest_dir.path())
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "a rejected oversized import must not write any file"
        );
    }

    #[test]
    fn export_then_import_round_trips_a_real_theme_through_disk() {
        let theme = valid_file().validate().expect("should validate");
        let export_dir = tempfile::tempdir().expect("tempdir");
        let export_path = export_dir.path().join("shared-theme.toml");
        export_theme_to_path(&theme, &export_path).expect("export should succeed");

        let import_dest = tempfile::tempdir().expect("tempdir");
        let imported =
            import_theme_file(&export_path, import_dest.path()).expect("import should succeed");

        assert_eq!(imported.name, theme.name);
        assert_eq!(imported.subtitle, theme.subtitle);
        assert_eq!(imported.swatches, theme.swatches);
    }

    /// Real proof (not an assumption) that `toml`/`serde`'s parsing genuinely ignores
    /// `#`-prefixed comments: [`CUSTOM_THEME_TEMPLATE_TOML`] is mostly comment lines, and this
    /// feeds its *real, checked-in* contents (not a hand-copied excerpt) through the exact same
    /// [`parse_theme_file_str`] a user's own disk file goes through.
    #[test]
    fn the_real_template_file_parses_and_validates_as_a_well_formed_theme() {
        let theme = parse_theme_file_str(CUSTOM_THEME_TEMPLATE_TOML)
            .expect("the real, checked-in template file must parse and validate cleanly");
        assert_eq!(theme.name, "My Custom Theme");
        assert_eq!(theme.subtitle, "replace this with a short description");
        assert_eq!(
            theme.swatches,
            [0x0c0d10, 0x181a1e, 0x5cb87f, 0xe2a336, 0xe07a5f]
        );
    }

    #[test]
    fn write_template_theme_writes_the_real_template_bytes_verbatim_comments_included() {
        let dest_dir = tempfile::tempdir().expect("tempdir");

        let written = write_template_theme(dest_dir.path()).expect("should write");

        assert_eq!(written.name, "My Custom Theme");
        let dest_path = written.source_path.expect("should record its own path");
        let on_disk = std::fs::read_to_string(&dest_path).expect("should read back");
        assert_eq!(
            on_disk, CUSTOM_THEME_TEMPLATE_TOML,
            "the written file must be the template's own literal bytes - comments included - \
             not a re-serialized, comment-stripped copy"
        );
        assert!(
            on_disk.contains("READABILITY FLOOR"),
            "a real explanatory comment must have survived the write"
        );

        // The written file is itself a real, loadable custom theme, not just bytes on disk.
        let (loaded, errors) = load_custom_themes_from_dir(dest_dir.path());
        assert!(
            errors.is_empty(),
            "the written template must load cleanly: {errors:?}"
        );
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "My Custom Theme");
    }

    #[test]
    fn write_template_theme_a_second_time_refreshes_the_same_file_not_a_new_one() {
        let dest_dir = tempfile::tempdir().expect("tempdir");

        let first = write_template_theme(dest_dir.path()).expect("first write");
        let second = write_template_theme(dest_dir.path()).expect("second write");

        assert_eq!(
            first.source_path, second.source_path,
            "writing the template twice must refresh the same file, not spawn a -2 sibling"
        );
        let (loaded, _) = load_custom_themes_from_dir(dest_dir.path());
        assert_eq!(
            loaded.len(),
            1,
            "must not have left a stale duplicate file behind"
        );
    }

    /// Adversarial-audit regression: a user who edits the template-created file in place (keeping
    /// its default `name`) must not have those edits silently destroyed by a second "New from
    /// template" click.
    #[test]
    fn write_template_theme_never_clobbers_a_file_the_user_has_since_edited() {
        let dest_dir = tempfile::tempdir().expect("tempdir");

        let first = write_template_theme(dest_dir.path()).expect("first write");
        let dest_path = first.source_path.clone().expect("should record its path");

        // The user edits the file in place, keeping the same `name`.
        let edited_toml = CUSTOM_THEME_TEMPLATE_TOML.replace("#0c0d10", "#123456");
        assert_ne!(
            edited_toml, CUSTOM_THEME_TEMPLATE_TOML,
            "sanity check: the edit must actually change the file's bytes"
        );
        std::fs::write(&dest_path, &edited_toml).expect("simulate a user edit");

        let second = write_template_theme(dest_dir.path()).expect("second write");

        assert_eq!(
            second.source_path, first.source_path,
            "must still resolve to the same file, not a -2 sibling"
        );
        assert_eq!(
            second.swatches[0], 0x123456,
            "the user's edited colour must be preserved, not overwritten with the pristine \
             template's own #0c0d10"
        );
        let on_disk = std::fs::read_to_string(&dest_path).expect("read back");
        assert_eq!(
            on_disk, edited_toml,
            "the file on disk must be untouched by the second click"
        );
    }

    #[test]
    fn remove_custom_theme_file_deletes_the_real_backing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("midnight-coral.toml");
        std::fs::write(&path, valid_file().to_toml_string_for_test()).expect("write");
        let mut theme = valid_file().validate().expect("should validate");
        theme.source_path = Some(path.clone());

        remove_custom_theme_file(&theme).expect("remove should succeed");

        assert!(!path.exists());
    }

    #[test]
    fn remove_custom_theme_file_with_no_source_path_is_a_harmless_no_op() {
        let theme = valid_file().validate().expect("should validate");
        assert_eq!(theme.source_path, None);
        assert!(remove_custom_theme_file(&theme).is_ok());
    }

    #[test]
    fn slugify_produces_filesystem_safe_lowercase_hyphenated_names() {
        assert_eq!(slugify("Midnight Coral"), "midnight-coral");
        assert_eq!(slugify("  Spaces   Everywhere  "), "spaces-everywhere");
        // Only the non-ASCII letters (`Ü`/`ï`/`ö`/`é`) become separators - the plain ASCII
        // letters already inside "Ünïcödé" (`n`, `c`, `d`) survive as their own segments.
        assert_eq!(slugify("Ünïcödé Theme!!"), "n-c-d-theme");
        assert_eq!(slugify("...."), "theme");
        assert_eq!(slugify("UPPER_CASE-name"), "upper-case-name");
    }

    #[test]
    fn custom_themes_dir_for_is_a_real_sibling_of_the_given_settings_path() {
        let settings_path = Path::new("/home/user/.config/jerry/settings.toml");
        assert_eq!(
            custom_themes_dir_for(settings_path),
            Path::new("/home/user/.config/jerry/themes")
        );
    }

    #[test]
    fn custom_themes_dir_for_falls_back_to_a_bare_name_for_a_settings_path_with_no_parent() {
        assert_eq!(
            custom_themes_dir_for(Path::new("settings.toml")),
            Path::new("themes")
        );
    }

    /// GitHub issue #5 follow-up: the six built-in themes moved from a hardcoded Rust
    /// `THEME_DEFS` const array to real `assets/themes/*.toml` files, embedded via `include_str!`
    /// and parsed through [`parse_builtin_theme_file_str`] - this pins the exact hex swatches and
    /// names/subtitles those files must produce, transcribed verbatim from the old array, so a
    /// single-digit typo made while writing one of those files would fail a test rather than
    /// silently changing the app's real default appearance.
    #[test]
    fn parse_builtin_theme_file_str_parses_every_embedded_built_in_theme_file_into_the_exact_documented_swatches(
    ) {
        let cases: [(&str, &str, &str, [u32; 5]); 6] = [
            (
                include_str!("../../../../assets/themes/jerry-dark.toml"),
                "Jerry Dark",
                "default",
                [0x0e0f11, 0x1a1e21, 0x5cb87f, 0xe2a336, 0x74ade8],
            ),
            (
                include_str!("../../../../assets/themes/jerry-dim.toml"),
                "Jerry Dim",
                "lower contrast",
                [0x15181b, 0x20252a, 0x6ab97f, 0xd8a94a, 0x7f9ad4],
            ),
            (
                include_str!("../../../../assets/themes/slate.toml"),
                "Slate",
                "cool greys",
                [0x0d1117, 0x161b22, 0x57a773, 0xc9a227, 0x6b9bd1],
            ),
            (
                include_str!("../../../../assets/themes/ember.toml"),
                "Ember",
                "warm",
                [0x12100e, 0x1e1a16, 0x8fae6b, 0xd98b3a, 0xc4713f],
            ),
            (
                include_str!("../../../../assets/themes/moss.toml"),
                "Moss",
                "green-tinted",
                [0x0f1310, 0x1a201b, 0x7fc79a, 0xc8b45a, 0x6f9bb5],
            ),
            (
                include_str!("../../../../assets/themes/paper.toml"),
                "Paper",
                "light \u{b7} beta",
                [0xf4f1ea, 0xe4e0d6, 0x3f7a52, 0xa8752a, 0x3d6c9c],
            ),
        ];
        for (contents, name, subtitle, swatches) in cases {
            let theme = parse_builtin_theme_file_str(contents);
            assert_eq!(theme.name, name, "name mismatch for {contents:?}");
            assert_eq!(theme.subtitle, subtitle, "subtitle mismatch for {name}");
            assert_eq!(theme.swatches, swatches, "swatch mismatch for {name}");
            assert_eq!(
                theme.source_path, None,
                "a built-in theme parsed straight from an embedded string was never read from a \
                 real path on disk"
            );
        }
    }

    /// Proves the built-in files really do go through the *same* deserialization/validation core
    /// a user-supplied file does, not a second parser: feeding one's raw embedded contents to the
    /// ordinary user-facing [`parse_theme_file_str`] (which - unlike
    /// [`parse_builtin_theme_file_str`] - does check for a built-in-name collision) is correctly
    /// rejected, since by definition its name already is a real `THEME_DEFS` entry.
    #[test]
    fn a_built_in_theme_files_raw_contents_are_rejected_by_the_ordinary_user_facing_parser_as_a_builtin_collision(
    ) {
        let contents = include_str!("../../../../assets/themes/slate.toml");
        let err = parse_theme_file_str(contents).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::NameCollidesWithBuiltin("Slate".to_string())
        );
    }

    /// [`CustomThemeFile::validate_with_builtin_check`]'s `false` branch (the one
    /// [`parse_builtin_theme_file_str`] uses to avoid the self-referential "is this built-in name
    /// already in `THEME_DEFS`" check while `THEME_DEFS` is itself still being built from these
    /// same files) must still run every *other* real check - a bad hex colour or an empty name in
    /// a built-in file would be a real bug in that file, not something to silently wave through.
    #[test]
    fn validate_with_builtin_check_false_skips_only_the_collision_check_not_the_others() {
        let mut colliding_but_valid = valid_file();
        colliding_but_valid.name = "Jerry Dark".to_string();
        assert!(
            colliding_but_valid
                .validate_with_builtin_check(false)
                .is_ok(),
            "the collision check must be skippable"
        );
        assert_eq!(
            colliding_but_valid.validate_with_builtin_check(true),
            Err(ThemeFileError::NameCollidesWithBuiltin(
                "Jerry Dark".to_string()
            )),
            "sanity check: the same file must still collide when the check runs"
        );

        let mut bad_color = valid_file();
        bad_color.name = "Jerry Dark".to_string();
        bad_color.background = "not-a-color".to_string();
        assert_eq!(
            bad_color.validate_with_builtin_check(false),
            Err(ThemeFileError::InvalidColor {
                field: "background",
                value: "not-a-color".to_string()
            }),
            "an invalid colour must still be rejected even with the collision check off"
        );

        let mut empty_name = valid_file();
        empty_name.name = "   ".to_string();
        assert_eq!(
            empty_name.validate_with_builtin_check(false),
            Err(ThemeFileError::EmptyName),
            "an empty name must still be rejected even with the collision check off"
        );
    }

    /// Regression for a real, concurrency-incident-lost fix: a `panel` swatch with zero real
    /// contrast against `background` (here, literally the same colour) must be rejected - the
    /// concrete unreadable-theme case the readability floor exists to catch.
    #[test]
    fn a_panel_swatch_with_no_real_contrast_against_background_is_rejected() {
        let mut file = valid_file();
        file.panel = file.background.clone();
        let err = file.validate().unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::LowReadability {
                delta_per_mille: 0,
                floor_per_mille: readability_floor_per_mille(),
            }
        );
    }

    /// [`JERRY_DARK_BASELINE_SWATCHES`] is a pinned copy of Jerry Dark's own swatches, kept
    /// deliberately independent of `crate::settings::state::THEME_DEFS` at runtime (see that
    /// const's own docs for the reentrant `LazyLock` deadlock this avoids). This is the real
    /// regression test that keeps the pinned copy honest: a future edit to Jerry Dark's own
    /// swatches (`assets/themes/jerry-dark.toml`) that forgets to update this copy fails here,
    /// loudly, in an ordinary `#[test]` - never from inside another `LazyLock`'s own initializer,
    /// which is the one context this exact comparison must never run in.
    #[test]
    fn jerry_dark_baseline_swatches_const_matches_the_real_initialized_theme_defs_0_swatches() {
        assert_eq!(
            JERRY_DARK_BASELINE_SWATCHES,
            crate::settings::state::THEME_DEFS[0].swatches
        );
    }

    /// Real regression: the readability floor (derived from [`JERRY_DARK_BASELINE_SWATCHES`])
    /// must not accidentally reject any of the six real, shipped built-in theme files -
    /// `parse_builtin_theme_file_str` already panics on any validation failure, including a
    /// `LowReadability` rejection, so reaching the end of this loop at all is the real assertion.
    #[test]
    fn every_built_in_theme_clears_the_readability_floor() {
        let files = [
            include_str!("../../../../assets/themes/jerry-dark.toml"),
            include_str!("../../../../assets/themes/jerry-dim.toml"),
            include_str!("../../../../assets/themes/slate.toml"),
            include_str!("../../../../assets/themes/ember.toml"),
            include_str!("../../../../assets/themes/moss.toml"),
            include_str!("../../../../assets/themes/paper.toml"),
        ];
        let floor = readability_floor_per_mille();
        for contents in files {
            // `parse_builtin_theme_file_str` already panics on any validation failure - including
            // a `LowReadability` rejection - so it alone would prove this passes, but a bare panic
            // there names neither the theme nor the actual margin. Recomputing the real delta here
            // too and asserting it directly gives a real, specific failure message instead.
            let theme = parse_builtin_theme_file_str(contents);
            let delta = panel_background_luma_delta_per_mille(&theme.swatches);
            assert!(
                delta >= floor,
                "{}: panel/background luma delta {delta} is below the readability floor {floor}",
                theme.name
            );
        }
    }

    /// Pins the readability floor's actual real, computed magnitude - not just its existence.
    /// Every other test in this suite only proves the floor rejects a *zero*-contrast panel and
    /// accepts the six real built-in themes; none of them would catch a future edit that quietly
    /// weakens [`readability_floor_per_mille`] (e.g. dividing by `50` instead of `2`) while still
    /// passing every other assertion. This is the one test that would.
    #[test]
    fn readability_floor_per_mille_is_pinned_to_a_real_computed_value() {
        assert_eq!(readability_floor_per_mille(), 28);
    }

    /// A real near-miss, not just the zero-contrast extreme
    /// [`a_panel_swatch_with_no_real_contrast_against_background_is_rejected`] already covers:
    /// `panel` here is a visibly different hex string from `background` (a hand-author could
    /// easily mistake this for "different enough"), but still well under the floor - real
    /// per-mille values transcribed from a real luma computation, not hand-waved.
    #[test]
    fn a_panel_swatch_only_a_few_hex_digits_off_from_background_is_still_rejected() {
        let mut file = valid_file();
        file.panel = "#0e0f12".to_string();
        let err = file.validate().unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::LowReadability {
                delta_per_mille: 8,
                floor_per_mille: 28,
            }
        );
    }
}
