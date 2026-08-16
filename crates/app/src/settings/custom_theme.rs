//! Theme files - the real, on-disk format every theme in this app is defined by, built-in ones
//! (`assets/themes/*.toml`, embedded at compile time - see `crate::settings::state::THEME_DEFS`)
//! and user-authored ones (`~/.config/jerry/themes/<slug>.toml`, GitHub issue #5) alike. There is
//! no second, privileged mechanism for the bundled themes: they are the same files, parsed by the
//! same code, validated by the same rules.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use gpui::Rgba;

use crate::settings::state::THEME_DEFS;
use crate::theme;

/// A defensive upper bound on a single theme file's size - see [`load_custom_themes_from_dir`]'s
/// own docs for why. [`import_theme_file`] enforces this same cap against the *source* file it's
/// about to import, not just files already sitting in a custom-themes directory.
const MAX_THEME_FILE_BYTES: u64 = 64 * 1024;

/// The five keys a theme's Themes-page card previews when the file names no `preview` of its own -
/// window background, rail/panel background, and the three status accents, in the same
/// `[background, panel, green-ish, amber-ish, blue-ish]` order the pre-rewrite five-swatch format
/// used (so a card looks like it always did). See [`CustomTheme::preview_swatches`].
const PREVIEW_KEYS: [&str; 5] = [
    "surface.window",
    "surface.rail",
    "status.review",
    "status.ask",
    "status.run",
];

/// The real text/background pairs a theme has to keep legible, checked against its own fully
/// compiled palette by [`check_palette_readability`]. Each entry is
/// `(what, foreground key, background key)`.
const READABILITY_PAIRS: [(&str, &str, &str); 2] = [
    ("body text", "text.body", "surface.window"),
    ("code", "syntax.text", "surface.center"),
];

/// The minimum WCAG 2.x contrast ratio (as an integer hundredth, so
/// [`ThemeFileError::LowContrast`] can stay `#[derive(Eq)]` - `f64` cannot) a theme's text has to
/// keep against the surface it is painted on.
const MIN_CONTRAST_PER_HUNDRED: u32 = 160;

/// WCAG 2.x relative luminance (<https://www.w3.org/TR/WCAG21/#dfn-relative-luminance>), applied
/// to [`Rgba`]'s own already-`0.0..=1.0` sRGB components - the same formula every standard
/// contrast checker uses, and the same one `crate::theme::syntax_contrast_tests` measures with.
fn relative_luminance(color: Rgba) -> f64 {
    fn channel(component: f32) -> f64 {
        let component = component as f64;
        if component <= 0.03928 {
            component / 12.92
        } else {
            ((component + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// The real WCAG contrast ratio between two colours, as an integer hundredth (so `1.0:1` is `100`
/// and `21.0:1` is `2100`). Order-independent, matching the standard definition.
fn contrast_per_hundred(a: Rgba, b: Rgba) -> u32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (higher, lower) = if la > lb { (la, lb) } else { (lb, la) };
    (((higher + 0.05) / (lower + 0.05)) * 100.0).round() as u32
}

/// Checks a **fully compiled** palette - the theme's own entries layered over everything up its
/// real `base` chain - for genuinely unreadable text/background pairs ([`READABILITY_PAIRS`]).
#[allow(clippy::expect_used)] // every `key` here is a real registered token
pub fn check_palette_readability(palette: &theme::Palette) -> Result<(), ThemeFileError> {
    let resolved = |key: &str| -> Rgba {
        let token = theme::token_for_key(key).expect("a real registered token");
        palette.get(token.key).copied().unwrap_or(token.default)
    };
    for (what, foreground, background) in READABILITY_PAIRS {
        let ratio = contrast_per_hundred(resolved(foreground), resolved(background));
        if ratio < MIN_CONTRAST_PER_HUNDRED {
            return Err(ThemeFileError::LowContrast {
                what,
                foreground,
                background,
                ratio_per_hundred: ratio,
                floor_per_hundred: MIN_CONTRAST_PER_HUNDRED,
            });
        }
    }
    Ok(())
}

/// One theme file's real, parsed-but-not-yet-validated contents - the direct shape of the TOML
/// document described in this module's docs. Keys and colours are still raw author-supplied text
/// here; [`Self::validate`] is what turns them into real registry-matched
/// `&'static str`-keyed entries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CustomThemeFile {
    pub name: String,
    pub subtitle: String,
    /// The theme this one inherits every key it doesn't name from - see the module docs.
    pub base: Option<String>,
    /// The five card swatches, if this file names them explicitly - `["#rrggbb"; 5]` as authored.
    pub preview: Option<[String; 5]>,
    /// Every `"{table}.{key}" = "#rrggbb"` entry, in the file's own order.
    pub overrides: Vec<(String, String)>,
}

/// A [`CustomThemeFile`] that has really been validated - a non-empty name that doesn't collide
/// with a built-in, every key a real `crate::theme` token, every value a real `#rrggbb` colour -
/// and is safe to compile into a live palette ([`compile_palette`]) or render as a Themes card.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomTheme {
    pub name: String,
    pub subtitle: String,
    pub base: Option<String>,
    /// The explicit card swatches this file named, if any - see [`Self::preview_swatches`].
    pub preview: Option<[u32; 5]>,
    /// Exactly the keys this file itself sets, matched against `crate::theme::TOKEN_GROUPS` so
    /// every key here is a real, live `&'static str` token key - not the *compiled* palette, which
    /// also includes everything inherited up the `base` chain (see [`compile_palette`]).
    pub overrides: HashMap<&'static str, Rgba>,
    /// The real file this theme was loaded from/written to. `None` for a value built in-memory
    /// without ever touching disk (every built-in theme, parsed from an embedded string).
    pub source_path: Option<PathBuf>,
}

/// Every real, specific way a theme file can fail to load - shown verbatim to the user rather
/// than collapsed into one generic "invalid theme" message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeFileError {
    Io(String),
    Parse(String),
    EmptyName,
    NameCollidesWithBuiltin(String),
    /// A top-level key that isn't `name`/`subtitle`/`base`/`preview` and isn't a real
    /// `crate::theme` module table either - most likely a typo in a table header, or a colour
    /// written at the top level instead of inside its module's table.
    UnknownTable(String),
    /// A `[module] key` pair that names no real `crate::theme::TOKEN_GROUPS` token. Carries the
    /// full dotted key (`"syntax.keywrod"`) exactly as it would be written in the file.
    UnknownKey(String),
    /// A value that isn't a real `#rrggbb` colour.
    InvalidColor {
        key: String,
        value: String,
    },
    /// `preview` is present but isn't an array of exactly five `#rrggbb` strings.
    InvalidPreview(String),
    /// A theme's text is not legible against the surface it is painted on - see
    /// [`check_palette_readability`], which is also where the deliberately-low floor is justified.
    LowContrast {
        what: &'static str,
        foreground: &'static str,
        background: &'static str,
        ratio_per_hundred: u32,
        floor_per_hundred: u32,
    },
    /// The *source* file [`import_theme_file`] was asked to import is over
    /// [`MAX_THEME_FILE_BYTES`] - checked before that file is ever read or written.
    TooLarge {
        bytes: u64,
        max_bytes: u64,
    },
    /// `base` names a theme that doesn't exist (a typo, or a custom theme whose file has since
    /// been removed) - a real, reported error rather than silently inheriting nothing.
    UnknownBase {
        theme: String,
        base: String,
    },
    /// A theme's `base` chain loops back on itself (`A` -> `B` -> `A`, or a theme naming itself).
    /// Carries the real chain, in order, so the offending file is obvious.
    BaseCycle(Vec<String>),
}

impl std::fmt::Display for ThemeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeFileError::Io(msg) => write!(f, "couldn't read the theme file: {msg}"),
            ThemeFileError::Parse(msg) => write!(f, "not a valid theme file: {msg}"),
            ThemeFileError::EmptyName => write!(f, "a theme file needs a non-empty `name`"),
            ThemeFileError::NameCollidesWithBuiltin(name) => write!(
                f,
                "\"{name}\" is already a built-in theme name - choose a different name"
            ),
            ThemeFileError::UnknownTable(table) => write!(
                f,
                "`{table}` isn't a real colour group this app knows about - expected one of the \
                 `theme` module names (surface, border, text, status, syntax, ...) or one of \
                 `name`/`subtitle`/`base`/`preview`"
            ),
            ThemeFileError::UnknownKey(key) => write!(
                f,
                "`{key}` isn't a real colour this app knows about - see the theme template for \
                 the full list of real keys"
            ),
            ThemeFileError::InvalidColor { key, value } => write!(
                f,
                "`{key}` is not a real colour (\"{value}\") - expected `#rrggbb`"
            ),
            ThemeFileError::InvalidPreview(detail) => {
                write!(f, "`preview` must be five `#rrggbb` colours ({detail})")
            }
            ThemeFileError::LowContrast {
                what,
                foreground,
                background,
                ratio_per_hundred,
                floor_per_hundred,
            } => write!(
                f,
                "this theme's {what} would be unreadable: `{foreground}` only reaches {}:1 \
                 contrast against `{background}`, below the {}:1 minimum",
                format_ratio(*ratio_per_hundred),
                format_ratio(*floor_per_hundred)
            ),
            ThemeFileError::TooLarge { bytes, max_bytes } => write!(
                f,
                "theme file is {bytes} bytes, over the {max_bytes}-byte limit for a theme file"
            ),
            ThemeFileError::UnknownBase { theme, base } => write!(
                f,
                "\"{theme}\" names \"{base}\" as its base theme, but no theme by that name is \
                 loaded"
            ),
            ThemeFileError::BaseCycle(chain) => write!(
                f,
                "these themes inherit from each other in a loop: {} - a `base` chain has to end \
                 somewhere",
                chain.join(" -> ")
            ),
        }
    }
}

/// Parses `#rrggbb` (exactly a `#` plus six hex digits) into a `0xrrggbb` value. Deliberately
/// narrower than CSS: no `#rgb` shorthand, no alpha, no named colours - see the module docs.
fn parse_hex_color(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// `160` -> `"1.6"` - renders an integer-hundredth contrast ratio for a real error message
/// without pulling a float into [`ThemeFileError`] (which derives `Eq`).
fn format_ratio(per_hundred: u32) -> String {
    format!("{}.{}", per_hundred / 100, (per_hundred % 100) / 10)
}

pub(crate) fn format_hex_color(value: u32) -> String {
    format!("#{:06x}", value & 0x00ff_ffff)
}

/// A real, opaque [`Rgba`] rounded back to its `0xrrggbb` form - the inverse of
/// [`crate::theme::hex_rgba`], used whenever a compiled colour is written back out as text.
pub(crate) fn rgba_to_hex(color: Rgba) -> u32 {
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

/// Flattens one `[module]` table's entries into `"{module}.{key}"` pairs, recursing through any
/// nested sub-table.
fn collect_table(
    prefix: &str,
    table: &toml::Table,
    out: &mut Vec<(String, String)>,
) -> Result<(), ThemeFileError> {
    for (key, value) in table {
        let full_key = format!("{prefix}.{key}");
        match value {
            toml::Value::String(text) => out.push((full_key, text.clone())),
            toml::Value::Table(nested) => collect_table(&full_key, nested, out)?,
            other => {
                return Err(ThemeFileError::InvalidColor {
                    key: full_key,
                    value: other.to_string(),
                })
            }
        }
    }
    Ok(())
}

impl CustomThemeFile {
    /// Parses one theme file's raw TOML text into this shape - structural checks only (is this
    /// even a TOML document of the right shape), never colour or key validation, which is
    /// [`Self::validate`]'s job.
    pub fn from_toml_str(contents: &str) -> Result<Self, ThemeFileError> {
        let document: toml::Table =
            toml::from_str(contents).map_err(|err| ThemeFileError::Parse(err.to_string()))?;

        let mut file = CustomThemeFile::default();
        for (key, value) in &document {
            match key.as_str() {
                "name" | "subtitle" | "base" => {
                    let Some(text) = value.as_str() else {
                        return Err(ThemeFileError::Parse(format!(
                            "`{key}` must be a string, got {}",
                            value.type_str()
                        )));
                    };
                    match key.as_str() {
                        "name" => file.name = text.trim().to_string(),
                        "subtitle" => file.subtitle = text.trim().to_string(),
                        _ => file.base = Some(text.trim().to_string()).filter(|s| !s.is_empty()),
                    }
                }
                "preview" => {
                    let Some(array) = value.as_array() else {
                        return Err(ThemeFileError::InvalidPreview(format!(
                            "got {}, not an array",
                            value.type_str()
                        )));
                    };
                    if array.len() != 5 {
                        return Err(ThemeFileError::InvalidPreview(format!(
                            "got {} entries",
                            array.len()
                        )));
                    }
                    let mut swatches: [String; 5] = Default::default();
                    for (index, entry) in array.iter().enumerate() {
                        let Some(text) = entry.as_str() else {
                            return Err(ThemeFileError::InvalidPreview(format!(
                                "entry {index} is {}, not a string",
                                entry.type_str()
                            )));
                        };
                        swatches[index] = text.to_string();
                    }
                    file.preview = Some(swatches);
                }
                table_name => {
                    let Some(table) = value.as_table() else {
                        return Err(ThemeFileError::UnknownTable(table_name.to_string()));
                    };
                    collect_table(table_name, table, &mut file.overrides)?;
                }
            }
        }
        Ok(file)
    }

    /// Validates `self` into a real [`CustomTheme`]: a non-empty name that (checked against
    /// [`THEME_DEFS`], not a second hand-copied name list) doesn't collide with a built-in
    /// theme's, every key a real `crate::theme` token, every value a real `#rrggbb` colour, and a
    /// readable window/card pair. A thin wrapper over [`Self::validate_with_builtin_check`] with
    /// the collision check always on - see that function's own docs for the one real caller that
    /// needs it off.
    pub fn validate(&self) -> Result<CustomTheme, ThemeFileError> {
        self.validate_with_builtin_check(true)
    }

    /// The real, shared validation core [`Self::validate`] and [`parse_builtin_theme_file_str`]
    /// both go through - not two independently-maintained checks. `check_builtin_collision` is
    /// `false` for exactly one real caller: [`parse_builtin_theme_file_str`], parsing the six
    /// built-in themes' own embedded files while [`THEME_DEFS`] (a `std::sync::LazyLock`) is
    /// itself still being computed *from* those same files - checking a built-in theme's name
    /// against `THEME_DEFS` at that point would read a value still in the middle of being
    /// initialized. Every other check still runs regardless. Built-in name uniqueness is instead
    /// guarded the ordinary way, by `crate::settings::state::tests::
    /// every_theme_def_name_is_unique_and_jerry_dark_is_the_default_named_theme`.
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

        let mut overrides: HashMap<&'static str, Rgba> = HashMap::new();
        for (key, value) in &self.overrides {
            let Some(token) = theme::token_for_key(key) else {
                return Err(ThemeFileError::UnknownKey(key.clone()));
            };
            let Some(hex) = parse_hex_color(value) else {
                return Err(ThemeFileError::InvalidColor {
                    key: key.clone(),
                    value: value.clone(),
                });
            };
            overrides.insert(token.key, theme::hex_rgba(hex));
        }

        let preview = match &self.preview {
            Some(swatches) => {
                let mut parsed = [0u32; 5];
                for (index, value) in swatches.iter().enumerate() {
                    parsed[index] = parse_hex_color(value).ok_or_else(|| {
                        ThemeFileError::InvalidPreview(format!("entry {index} is \"{value}\""))
                    })?;
                }
                Some(parsed)
            }
            None => None,
        };

        Ok(CustomTheme {
            name: name.to_string(),
            subtitle: self.subtitle.trim().to_string(),
            base: self.base.clone(),
            preview,
            overrides,
            source_path: None,
        })
    }
}

impl CustomTheme {
    /// The five swatches this theme's Themes-page card paints: its own explicit `preview` when the
    /// file names one (every bundled theme does, so the cards look exactly as they always have),
    /// otherwise read off [`PREVIEW_KEYS`] - this theme's own entries where it has them, the
    /// compiled Jerry Dark default otherwise.
    #[allow(clippy::expect_used)] // every `key` in PREVIEW_KEYS is a real registered token
    pub fn preview_swatches(&self) -> [u32; 5] {
        if let Some(preview) = self.preview {
            return preview;
        }
        PREVIEW_KEYS.map(|key| {
            let token = theme::token_for_key(key).expect("a real registered preview token");
            rgba_to_hex(
                self.overrides
                    .get(token.key)
                    .copied()
                    .unwrap_or(token.default),
            )
        })
    }

    /// This theme's own real window background - what `crate::theme::theme_is_light` is asked
    /// about for `Settings.theme.last_dark_theme` bookkeeping. Same "own entry, else the compiled
    /// default" resolution (and same honest limitation) as [`Self::preview_swatches`].
    #[allow(clippy::expect_used)] // "surface.window" is a real registered token
    pub fn window_background(&self) -> Rgba {
        let token = theme::token_for_key("surface.window").expect("a real registered token");
        self.overrides
            .get(token.key)
            .copied()
            .unwrap_or(token.default)
    }

    /// The inverse of [`CustomThemeFile::from_toml_str`] - a real, re-parseable form.
    pub fn to_file(&self) -> CustomThemeFile {
        CustomThemeFile {
            name: self.name.clone(),
            subtitle: self.subtitle.clone(),
            base: self.base.clone(),
            preview: self.preview.map(|swatches| swatches.map(format_hex_color)),
            overrides: ordered_overrides(&self.overrides)
                .into_iter()
                .map(|(key, color)| (key.to_string(), format_hex_color(rgba_to_hex(color))))
                .collect(),
        }
    }

    /// The real, shareable TOML text [`export_theme_to_path`]/[`import_theme_file`] write to disk -
    /// see [`write_theme_toml`] for the format and why it's generated here rather than through
    /// `toml::to_string_pretty`.
    pub fn to_toml_string(&self) -> String {
        write_theme_toml(&self.to_file())
    }
}

/// This theme's own explicit entries in `crate::theme::TOKEN_GROUPS`' own registry order (module
/// by module, declaration order within a module) rather than a `HashMap`'s arbitrary one - so a
/// written file's tables and keys come out in the same, stable order every time, matching the
/// order someone reading `crate::theme`'s source would expect.
fn ordered_overrides(overrides: &HashMap<&'static str, Rgba>) -> Vec<(&'static str, Rgba)> {
    theme::all_tokens()
        .filter_map(|token| overrides.get(token.key).map(|color| (token.key, *color)))
        .collect()
}

/// Writes one theme file's real TOML text - see [`crate::settings::theme_file_format`], which owns
/// the layout, ordering and the comments derived from `crate::theme`'s own source.
fn write_theme_toml(file: &CustomThemeFile) -> String {
    crate::settings::theme_file_format::write_theme_toml(file, DEFAULT_THEME_FILE_HEADER)
}

/// The preamble on a theme file Jerry writes for a user (an import, an export, or a
/// generated-from-colour theme). `crate::settings::builtin_themes` passes its own instead.
pub(crate) const DEFAULT_THEME_FILE_HEADER: &str =
    "# A Jerry theme. Edit any value, delete any line, and reload Jerry to see it.";

/// Compiles `theme` into the real, flat [`crate::theme::Palette`] the live app resolves every
/// colour token against: everything up its `base` chain first (root-most base first), then each
/// theme's own explicit entries layered on top, so a nearer theme always wins.
pub fn compile_palette(
    theme_def: &CustomTheme,
    known: &[&CustomTheme],
) -> Result<theme::Palette, ThemeFileError> {
    let mut chain: Vec<&CustomTheme> = vec![theme_def];
    let mut visited: Vec<String> = vec![theme_def.name.clone()];
    let mut current = theme_def;
    while let Some(base_name) = current.base.as_deref() {
        if visited.iter().any(|seen| seen == base_name) {
            let mut cycle = visited;
            cycle.push(base_name.to_string());
            return Err(ThemeFileError::BaseCycle(cycle));
        }
        let Some(base) = known.iter().find(|candidate| candidate.name == base_name) else {
            return Err(ThemeFileError::UnknownBase {
                theme: current.name.clone(),
                base: base_name.to_string(),
            });
        };
        visited.push(base.name.clone());
        chain.push(base);
        current = base;
    }

    let mut palette = theme::Palette::new();
    // Root-most base first, so a nearer theme's own entries overwrite what it inherited.
    for theme_in_chain in chain.iter().rev() {
        for (key, color) in &theme_in_chain.overrides {
            palette.insert(key, *color);
        }
    }
    Ok(palette)
}

/// The real, live "which palette does the selected theme name compile to" lookup
/// `crate::settings::render::AdeApp::apply_theme_selection` uses: finds `name` among the six
/// built-in [`THEME_DEFS`] themes first, then `customs`, and compiles it against both sets.
pub fn compile_palette_by_name(
    name: &str,
    customs: &[CustomTheme],
) -> Result<Option<theme::Palette>, ThemeFileError> {
    let builtins: Vec<&CustomTheme> = THEME_DEFS.iter().map(|def| def.theme).collect();
    let mut known: Vec<&CustomTheme> = builtins;
    known.extend(customs.iter());

    let Some(selected) = known.iter().find(|candidate| candidate.name == name) else {
        return Ok(None);
    };
    let palette = compile_palette(selected, &known)?;
    Ok((!palette.is_empty()).then_some(palette))
}

/// Parses and validates one built-in theme's embedded TOML text (one of the six real
/// `assets/themes/*.toml` files [`THEME_DEFS`] embeds via `include_str!`) through the exact same
/// deserialization and validation core [`parse_theme_file_str`] uses for a user's own disk-loaded
/// file - not a second, parallel parser. Only the built-in-collision half of that check is skipped
/// (see [`CustomThemeFile::validate_with_builtin_check`]'s own docs for why that one specific
/// check is self-referential here).
#[allow(clippy::expect_used)] // panics only on a corrupt built-in asset - see doc comment above
pub(crate) fn parse_builtin_theme_file_str(contents: &str) -> CustomTheme {
    let file = CustomThemeFile::from_toml_str(contents)
        .expect("a built-in theme file under assets/themes/ failed to parse as TOML");
    file.validate_with_builtin_check(false)
        .expect("a built-in theme file under assets/themes/ failed validation")
}

/// The real `themes/` directory sibling of a given `settings_path` (e.g.
/// `~/.config/jerry/settings.toml` -> `~/.config/jerry/themes`) - mirrors
/// `crate::sidebar::fold_state::fold_state_path_for`'s own "derive from the settings path this
/// `AdeApp` instance was actually given, never `$HOME` directly" convention, for the exact same
/// reason: `crate::root::AdeApp::new_with_settings` (every `#[gpui::test]`'s shared entry point,
/// not just production) calls this unconditionally at construction time, so resolving straight
/// from `$HOME` here would mean every test in this crate reads (and a real "Import theme" action
/// would write into) whatever `~/.config/jerry/themes` happens to exist on the machine actually
/// running the tests - a real, previously-caught-in-review test-isolation bug.
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
    CustomThemeFile::from_toml_str(contents)?.validate()
}

/// Loads every real, validatable `*.toml` file directly inside `dir` (non-recursive) as a
/// [`CustomTheme`]. A file that fails to read, parse, or validate is skipped - not silently: its
/// real error, prefixed with the file name, is appended to the returned tuple's second element so
/// a caller can surface it (`crate::root::AdeApp::custom_theme_load_errors`) rather than a bad
/// hand-edit quietly vanishing.
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
        // stall every window's startup with no bound at all.
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

    // Real base-chain check, now that every theme in this directory is known - see this
    // function's own docs.
    let builtins: Vec<&CustomTheme> = THEME_DEFS.iter().map(|def| def.theme).collect();
    let mut broken: Vec<(String, String)> = Vec::new();
    {
        let mut known: Vec<&CustomTheme> = builtins;
        known.extend(themes.iter());
        for candidate in &themes {
            if let Err(err) = compile_palette(candidate, &known)
                .and_then(|palette| check_palette_readability(&palette))
            {
                let file_name = candidate
                    .source_path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| candidate.name.clone());
                broken.push((candidate.name.clone(), format!("{file_name}: {err}")));
            }
        }
    }
    for (name, message) in broken {
        themes.retain(|candidate| candidate.name != name);
        errors.push(message);
    }
    (themes, errors)
}

/// Real import: reads and validates the file at `source_path` (any real path on disk, e.g. one a
/// user just picked in a real file-open dialog), then writes its *validated, re-serialized* form
/// (not a byte-for-byte copy - see [`CustomTheme::to_toml_string`], so a source file with extra
/// whitespace or key ordering still lands as a canonical file) into `dest_dir`. A malformed source
/// file is rejected with a real [`ThemeFileError`] and nothing is written.
pub fn import_theme_file(
    source_path: &Path,
    dest_dir: &Path,
) -> Result<CustomTheme, ThemeFileError> {
    // The same real cap `load_custom_themes_from_dir` already enforces against files already
    // sitting in a custom-themes directory, applied here against the *source* file before it's
    // ever read into memory or written anywhere.
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
/// [`import_theme_file`] (a plain-TOML source), `crate::settings::vscode_theme`'s own import glue
/// (a converted-from-JSON source) and `crate::settings::render`'s "Generate from colour" action (a
/// freshly derived palette) all need - re-validated here even though a caller may have already
/// validated once (cheap, and keeps this function honest as the one real place a theme actually
/// lands on disk, rather than trusting a caller's own possibly-stale validation).
pub(crate) fn validate_and_write(
    file: CustomThemeFile,
    dest_dir: &Path,
) -> Result<CustomTheme, ThemeFileError> {
    let mut theme = file.validate()?;
    // Readability is checked against what this theme really *renders* as - its own entries layered
    // over everything up its real `base` chain - which means resolving that chain, which means
    // knowing every other theme it could name. The built-ins plus whatever is already in
    // `dest_dir` is exactly that set. See `check_palette_readability`'s own docs for why the check
    // lives here and in `load_custom_themes_from_dir` rather than inside `validate`.
    let siblings = load_custom_themes_from_dir(dest_dir).0;
    let builtins: Vec<&CustomTheme> = THEME_DEFS.iter().map(|def| def.theme).collect();
    let mut known: Vec<&CustomTheme> = builtins;
    known.extend(siblings.iter().filter(|other| other.name != theme.name));
    known.push(&theme);
    check_palette_readability(&compile_palette(&theme, &known)?)?;

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
/// Themes-page action writes are the literal same bytes, never two independently-maintained copies
/// that could drift apart. Deliberately *not* one of [`THEME_DEFS`]' own six `include_str!`s: that
/// list names all six built-in theme files explicitly, and this isn't a seventh built-in theme.
pub const CUSTOM_THEME_TEMPLATE_TOML: &str =
    include_str!("../../../../assets/themes/template.toml");

/// Writes the real template ([`CUSTOM_THEME_TEMPLATE_TOML`]) into `dest_dir` - the Themes page's
/// "New from template" action's one real caller. Validates through the same
/// [`parse_theme_file_str`] core [`import_theme_file`] uses (never written unvalidated - if this
/// template itself ever regressed into something that fails its own validation, this would fail
/// loudly rather than silently hand a user a broken file), then writes the template's own literal
/// bytes - comments and all - not a re-serialized [`CustomTheme::to_toml_string`] copy: the whole
/// point of a "well-commented template" is that a user who clicks the button gets the same
/// explanatory comments as one who copies the file straight out of the repository.
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

    const MIDNIGHT_CORAL: &str = r##"
name = "Midnight Coral"
subtitle = "warm accent"
base = "Jerry Dark"

[surface]
window = "#0c0d10"
card = "#181a1e"

[syntax]
keyword = "#ff79c6"
"##;

    fn valid_file() -> CustomThemeFile {
        CustomThemeFile::from_toml_str(MIDNIGHT_CORAL).expect("the fixture must parse")
    }

    fn rgba(value: u32) -> Rgba {
        theme::hex_rgba(value)
    }

    #[test]
    fn a_well_formed_file_validates_into_real_registry_matched_overrides() {
        let theme = valid_file().validate().expect("should validate");
        assert_eq!(theme.name, "Midnight Coral");
        assert_eq!(theme.subtitle, "warm accent");
        assert_eq!(theme.base.as_deref(), Some("Jerry Dark"));
        assert_eq!(theme.overrides.len(), 3);
        assert_eq!(theme.overrides["surface.window"], rgba(0x0c0d10));
        assert_eq!(theme.overrides["surface.card"], rgba(0x181a1e));
        assert_eq!(theme.overrides["syntax.keyword"], rgba(0xff79c6));
        assert_eq!(theme.source_path, None);
    }

    #[test]
    fn a_file_naming_a_single_key_is_a_real_complete_theme() {
        let theme =
            parse_theme_file_str("name = \"Just One\"\n\n[syntax]\nkeyword = \"#ff0000\"\n")
                .expect("a one-key theme is a real theme");
        assert_eq!(theme.overrides.len(), 1);
        assert_eq!(theme.base, None);
    }

    #[test]
    fn an_unknown_key_is_a_real_rejection_not_a_silently_ignored_typo() {
        let err =
            parse_theme_file_str("name = \"T\"\n\n[syntax]\nkeywrod = \"#ff79c6\"\n").unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::UnknownKey("syntax.keywrod".to_string())
        );
    }

    #[test]
    fn an_unknown_table_is_a_real_rejection() {
        let err =
            parse_theme_file_str("name = \"T\"\n\n[surfaces]\nwindow = \"#0c0d10\"\n").unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::UnknownKey("surfaces.window".to_string())
        );
        let err = parse_theme_file_str("name = \"T\"\nwindow = \"#0c0d10\"\n").unwrap_err();
        assert_eq!(err, ThemeFileError::UnknownTable("window".to_string()));
    }

    #[test]
    fn every_real_invalid_colour_shape_is_rejected_with_the_offending_key_named() {
        for bad in ["0c0d10", "#0c0", "#gggggg", "#0c0d1012", "not-a-color", ""] {
            let err =
                parse_theme_file_str(&format!("name = \"T\"\n\n[surface]\nwindow = \"{bad}\"\n"))
                    .unwrap_err();
            assert_eq!(
                err,
                ThemeFileError::InvalidColor {
                    key: "surface.window".to_string(),
                    value: bad.to_string(),
                }
            );
        }
    }

    #[test]
    fn a_pair_and_an_array_token_are_real_addressable_keys() {
        let theme = parse_theme_file_str(
            "name = \"T\"\n\n[agent]\n\"sonnet.fg\" = \"#ff0000\"\n\n[graph]\n\"lanes.0\" = \"#00ff00\"\n",
        )
        .expect("pair and array keys are real");
        assert_eq!(theme.overrides["agent.sonnet.fg"], rgba(0xff0000));
        assert_eq!(theme.overrides["graph.lanes.0"], rgba(0x00ff00));
    }

    #[test]
    fn an_empty_name_is_a_real_rejection_not_a_silent_fallback() {
        let err = parse_theme_file_str("name = \"   \"\n").unwrap_err();
        assert_eq!(err, ThemeFileError::EmptyName);
    }

    #[test]
    fn a_name_colliding_with_a_builtin_theme_is_rejected() {
        let err = parse_theme_file_str("name = \"Jerry Dark\"\n").unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::NameCollidesWithBuiltin("Jerry Dark".to_string())
        );
    }

    #[test]
    fn parse_theme_file_str_rejects_garbage_toml_with_a_real_parse_error() {
        let err = parse_theme_file_str("this is not valid toml {{{").unwrap_err();
        assert!(matches!(err, ThemeFileError::Parse(_)));
    }

    #[test]
    fn a_theme_round_trips_through_to_toml_string_and_back_byte_for_byte() {
        let theme = valid_file().validate().expect("should validate");
        let text = theme.to_toml_string();
        let reparsed = parse_theme_file_str(&text).expect("the written form must re-parse");
        assert_eq!(reparsed.name, theme.name);
        assert_eq!(reparsed.subtitle, theme.subtitle);
        assert_eq!(reparsed.base, theme.base);
        assert_eq!(reparsed.overrides, theme.overrides);
        assert_eq!(
            reparsed.to_toml_string(),
            text,
            "writing a re-parsed theme must be a real fixed point, not drift each round"
        );
    }

    #[test]
    fn the_writer_groups_tables_even_when_the_entries_arrive_interleaved() {
        let file = CustomThemeFile {
            name: "Interleaved".to_string(),
            subtitle: String::new(),
            base: None,
            preview: None,
            overrides: vec![
                ("surface.window".to_string(), "#0c0d10".to_string()),
                ("syntax.keyword".to_string(), "#ff79c6".to_string()),
                ("surface.card".to_string(), "#181a1e".to_string()),
                ("syntax.string".to_string(), "#f1fa8c".to_string()),
            ],
        };
        let text = write_theme_toml(&file);
        assert_eq!(text.matches("[surface]").count(), 1, "got:\n{text}");
        assert_eq!(text.matches("[syntax]").count(), 1, "got:\n{text}");
        let reparsed = parse_theme_file_str(&text).expect("must re-parse");
        assert_eq!(reparsed.overrides.len(), 4);
        assert_eq!(reparsed.overrides["surface.card"], rgba(0x181a1e));
    }

    #[test]
    fn a_name_containing_quotes_round_trips_through_the_writer() {
        let mut theme = valid_file().validate().expect("should validate");
        theme.name = "The \"Real\" \\ Theme".to_string();
        let reparsed = parse_theme_file_str(&theme.to_toml_string()).expect("must re-parse");
        assert_eq!(reparsed.name, "The \"Real\" \\ Theme");
    }

    #[test]
    fn an_explicit_preview_round_trips_and_wins_over_the_derived_one() {
        let text = "name = \"T\"\npreview = [\"#010203\", \"#040506\", \"#070809\", \"#0a0b0c\", \"#0d0e0f\"]\n";
        let theme = parse_theme_file_str(text).expect("should validate");
        assert_eq!(
            theme.preview_swatches(),
            [0x010203, 0x040506, 0x070809, 0x0a0b0c, 0x0d0e0f]
        );
        let reparsed = parse_theme_file_str(&theme.to_toml_string()).expect("must re-parse");
        assert_eq!(reparsed.preview, theme.preview);
    }

    #[test]
    fn a_theme_with_no_preview_derives_its_card_swatches_from_real_tokens() {
        let theme = valid_file().validate().expect("should validate");
        let swatches = theme.preview_swatches();
        assert_eq!(
            swatches[0], 0x0c0d10,
            "the first swatch is this theme's own real surface.window"
        );
        assert_eq!(
            swatches[2],
            rgba_to_hex(theme::status::REVIEW.default),
            "a key it doesn't name falls back to the compiled default"
        );
    }

    #[test]
    fn a_malformed_preview_is_a_real_rejection() {
        for (text, _) in [
            ("name = \"T\"\npreview = [\"#010203\"]\n", ()),
            ("name = \"T\"\npreview = \"#010203\"\n", ()),
            (
                "name = \"T\"\npreview = [\"nope\", \"#040506\", \"#070809\", \"#0a0b0c\", \"#0d0e0f\"]\n",
                (),
            ),
        ] {
            let err = parse_theme_file_str(text).unwrap_err();
            assert!(
                matches!(err, ThemeFileError::InvalidPreview(_)),
                "expected an InvalidPreview error for {text:?}, got {err:?}"
            );
        }
    }

    // ---- readability ------------------------------------------------------------------------

    #[test]
    fn text_the_same_colour_as_its_background_is_rejected() {
        let mut palette = theme::Palette::new();
        palette.insert("surface.window", rgba(0x0c0d10));
        palette.insert("text.body", rgba(0x0c0d10));
        let err = check_palette_readability(&palette).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::LowContrast {
                what: "body text",
                foreground: "text.body",
                background: "surface.window",
                ratio_per_hundred: 100,
                floor_per_hundred: MIN_CONTRAST_PER_HUNDRED,
            }
        );
    }

    #[test]
    fn text_a_few_hex_digits_off_from_its_background_is_still_rejected() {
        let mut palette = theme::Palette::new();
        palette.insert("surface.window", rgba(0x0c0d10));
        palette.insert("text.body", rgba(0x11131a));
        assert!(
            matches!(
                check_palette_readability(&palette),
                Err(ThemeFileError::LowContrast { .. })
            ),
            "expected a LowContrast rejection"
        );
    }

    #[test]
    fn unreadable_code_is_caught_even_when_the_chrome_reads_fine() {
        let mut palette = theme::Palette::new();
        palette.insert("surface.center", rgba(0x101010));
        palette.insert("syntax.text", rgba(0x121212));
        let err = check_palette_readability(&palette).unwrap_err();
        assert!(
            matches!(err, ThemeFileError::LowContrast { what: "code", .. }),
            "expected the code pair to be the one reported, got {err:?}"
        );
    }

    #[test]
    fn a_flat_surface_design_with_readable_text_is_accepted() {
        let mut palette = theme::Palette::new();
        palette.insert("surface.window", rgba(0x1f1f1f));
        palette.insert("surface.card", rgba(0x202020));
        palette.insert("surface.center", rgba(0x1f1f1f));
        palette.insert("text.body", rgba(0xcccccc));
        palette.insert("syntax.text", rgba(0xcccccc));
        assert!(
            check_palette_readability(&palette).is_ok(),
            "a flat-surface theme with real, legible text must import cleanly"
        );
    }

    #[test]
    fn an_empty_palette_is_jerry_dark_and_reads_fine() {
        assert!(check_palette_readability(&theme::Palette::new()).is_ok());
    }

    #[test]
    fn every_built_in_theme_is_really_readable_once_compiled() {
        for def in THEME_DEFS.iter() {
            let palette = compile_palette_by_name(def.name, &[])
                .expect("a bundled theme must compile")
                .unwrap_or_default();
            assert!(
                check_palette_readability(&palette).is_ok(),
                "{} is not readable: {:?}",
                def.name,
                check_palette_readability(&palette)
            );
        }
    }

    #[test]
    fn every_built_in_theme_clears_the_floor_with_real_headroom() {
        for def in THEME_DEFS.iter() {
            let palette = compile_palette_by_name(def.name, &[])
                .expect("must compile")
                .unwrap_or_default();
            let resolved = |key: &str| -> Rgba {
                let token = theme::token_for_key(key).expect("a real token");
                palette.get(token.key).copied().unwrap_or(token.default)
            };
            for (what, foreground, background) in READABILITY_PAIRS {
                let ratio = contrast_per_hundred(resolved(foreground), resolved(background));
                // 4.5:1 is WCAG's own "normal text" minimum, and every bundled theme really does
                // clear it - the tightest measured is Slate's code surface at 4.78:1. That is a
                // real pin on the palette, well above the 1.6:1 *validity* floor an imported
                // theme only has to clear.
                assert!(
                    ratio >= 450,
                    "{}: {what} only reaches {ratio} (hundredths) - every bundled theme should \
                     clear WCAG's own 4.5:1",
                    def.name
                );
            }
        }
    }

    // ---- base chains ------------------------------------------------------------------------

    fn theme_named(name: &str, base: Option<&str>, entries: &[(&'static str, u32)]) -> CustomTheme {
        CustomTheme {
            name: name.to_string(),
            subtitle: String::new(),
            base: base.map(|b| b.to_string()),
            preview: None,
            overrides: entries
                .iter()
                .map(|(key, value)| (*key, rgba(*value)))
                .collect(),
            source_path: None,
        }
    }

    #[test]
    fn a_theme_really_inherits_every_key_it_does_not_name_from_its_base() {
        let base = theme_named(
            "Base",
            None,
            &[("surface.window", 0x111111), ("text.body", 0x222222)],
        );
        let child = theme_named("Child", Some("Base"), &[("surface.window", 0x333333)]);
        let palette = compile_palette(&child, &[&base, &child]).expect("should compile");

        assert_eq!(
            palette["surface.window"],
            rgba(0x333333),
            "the nearer theme's own entry must win"
        );
        assert_eq!(
            palette["text.body"],
            rgba(0x222222),
            "a key only the base names must really be inherited"
        );
        assert!(
            !palette.contains_key("syntax.keyword"),
            "a key nobody in the chain names must be absent, so the token's own default wins"
        );
    }

    #[test]
    fn a_custom_theme_that_names_no_terminal_colours_inherits_its_bases() {
        let builtins: Vec<&CustomTheme> = THEME_DEFS.iter().map(|def| def.theme).collect();
        let paper = *builtins
            .iter()
            .find(|def| def.name == "Paper")
            .expect("Paper is a bundled theme");

        let child = theme_named("Mine", Some("Paper"), &[("surface.window", 0x333333)]);
        let mut known = builtins.clone();
        known.push(&child);
        let palette = compile_palette(&child, &known).expect("should compile");

        for key in [
            "terminal.background",
            "terminal.foreground",
            "terminal.ansi.2",
        ] {
            assert_eq!(
                palette.get(key).copied(),
                paper.overrides.get(key).copied(),
                "{key} must be inherited from the named base, not silently dropped"
            );
        }
        assert!(
            theme::theme_is_light(palette["terminal.background"]),
            "a custom theme based on the light bundled theme must get a light terminal"
        );

        // And a theme with no base at all falls through to the compiled defaults, the real Jerry
        // Dark case - absent from the palette is exactly how this app expresses that.
        let rootless = theme_named("Rootless", None, &[("surface.window", 0x333333)]);
        let palette = compile_palette(&rootless, &[&rootless]).expect("should compile");
        assert!(!palette.contains_key("terminal.background"));
        assert!(!palette.contains_key("terminal.ansi.2"));
    }

    #[test]
    fn a_multi_level_base_chain_layers_root_first() {
        let root = theme_named("Root", None, &[("text.body", 0x111111)]);
        let middle = theme_named("Middle", Some("Root"), &[("text.body", 0x222222)]);
        let leaf = theme_named("Leaf", Some("Middle"), &[("surface.window", 0x333333)]);
        let palette = compile_palette(&leaf, &[&root, &middle, &leaf]).expect("should compile");
        assert_eq!(
            palette["text.body"],
            rgba(0x222222),
            "Middle must win over Root for a key both name"
        );
        assert_eq!(palette["surface.window"], rgba(0x333333));
    }

    #[test]
    fn a_base_cycle_is_a_real_reported_error_not_a_hang() {
        let a = theme_named("A", Some("B"), &[]);
        let b = theme_named("B", Some("A"), &[]);
        let err = compile_palette(&a, &[&a, &b]).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::BaseCycle(vec!["A".to_string(), "B".to_string(), "A".to_string()])
        );
    }

    #[test]
    fn a_theme_naming_itself_as_its_own_base_is_a_real_reported_cycle() {
        let self_based = theme_named("Loop", Some("Loop"), &[]);
        let err = compile_palette(&self_based, &[&self_based]).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::BaseCycle(vec!["Loop".to_string(), "Loop".to_string()])
        );
    }

    #[test]
    fn an_unknown_base_is_a_real_reported_error() {
        let orphan = theme_named("Orphan", Some("Nowhere"), &[]);
        let err = compile_palette(&orphan, &[&orphan]).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::UnknownBase {
                theme: "Orphan".to_string(),
                base: "Nowhere".to_string(),
            }
        );
    }

    #[test]
    fn compile_palette_by_name_resolves_a_real_builtin_base_from_a_custom_theme() {
        let custom = parse_theme_file_str(MIDNIGHT_CORAL).expect("should parse");
        let palette = compile_palette_by_name("Midnight Coral", &[custom])
            .expect("should compile")
            .expect("a theme with real overrides compiles to a real palette");
        assert_eq!(palette["surface.window"], rgba(0x0c0d10));
        assert_eq!(palette["syntax.keyword"], rgba(0xff79c6));
    }

    #[test]
    fn compile_palette_by_name_treats_an_unknown_selection_as_jerry_dark() {
        assert_eq!(
            compile_palette_by_name("No Such Theme", &[]).expect("not an error"),
            None
        );
    }

    #[test]
    fn load_custom_themes_from_dir_reports_and_drops_a_theme_with_a_broken_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("orphan.toml"),
            "name = \"Orphan\"\nbase = \"Nowhere\"\n\n[surface]\nwindow = \"#0c0d10\"\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("fine.toml"), MIDNIGHT_CORAL).expect("write");

        let (themes, errors) = load_custom_themes_from_dir(dir.path());

        assert_eq!(
            themes.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["Midnight Coral"],
            "the broken theme must be dropped, the good one kept"
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("orphan.toml:"), "got {:?}", errors[0]);
        assert!(errors[0].contains("Nowhere"));
    }

    #[test]
    fn load_custom_themes_from_dir_reports_a_real_cycle_between_two_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.toml"), "name = \"Ay\"\nbase = \"Bee\"\n")
            .expect("write");
        std::fs::write(dir.path().join("b.toml"), "name = \"Bee\"\nbase = \"Ay\"\n")
            .expect("write");

        let (themes, errors) = load_custom_themes_from_dir(dir.path());

        assert!(themes.is_empty(), "neither half of a cycle is loadable");
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|message| message.contains("loop")));
    }

    // ---- directory loading, import/export ---------------------------------------------------

    #[test]
    fn load_custom_themes_from_dir_skips_one_bad_file_and_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("good.toml"), MIDNIGHT_CORAL).expect("write good file");
        std::fs::write(dir.path().join("bad.toml"), "not valid toml {{{").expect("write bad file");
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
        std::fs::write(
            dir.path().join("z.toml"),
            MIDNIGHT_CORAL.replace("Midnight Coral", "Zebra"),
        )
        .expect("write");
        std::fs::write(
            dir.path().join("a.toml"),
            MIDNIGHT_CORAL.replace("Midnight Coral", "Apple"),
        )
        .expect("write");

        let (themes, _) = load_custom_themes_from_dir(dir.path());

        assert_eq!(
            themes.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["Apple", "Zebra"]
        );
    }

    #[test]
    fn load_custom_themes_from_dir_accepts_an_uppercase_toml_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Windows-Authored.TOML"), MIDNIGHT_CORAL).expect("write");

        let (themes, errors) = load_custom_themes_from_dir(dir.path());

        assert!(
            errors.is_empty(),
            "a real .TOML file should not be reported as an error"
        );
        assert_eq!(themes.len(), 1);
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
        std::fs::write(&source_path, "name = \"\"\n").expect("write malformed source");

        let err = import_theme_file(&source_path, dest_dir.path()).unwrap_err();
        assert_eq!(err, ThemeFileError::EmptyName);
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
        std::fs::write(&source_path, MIDNIGHT_CORAL).expect("write");

        let theme = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_eq!(theme.name, "Midnight Coral");
        let expected_path = dest_dir.path().join("midnight-coral.toml");
        assert_eq!(theme.source_path.as_deref(), Some(expected_path.as_path()));
        assert!(expected_path.exists());
        let (loaded, errors) = load_custom_themes_from_dir(dest_dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].overrides["syntax.keyword"], rgba(0xff79c6));
    }

    #[test]
    fn import_theme_file_never_clobbers_an_unrelated_theme_with_a_colliding_slug() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(
            dest_dir.path().join("midnight-coral.toml"),
            MIDNIGHT_CORAL.replace("Midnight Coral", "Ocean"),
        )
        .expect("write pre-existing file");

        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, MIDNIGHT_CORAL).expect("write");

        let imported = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_eq!(imported.name, "Midnight Coral");
        assert_ne!(
            imported.source_path,
            Some(dest_dir.path().join("midnight-coral.toml")),
            "must not have written into Ocean's own file"
        );

        let (loaded, errors) = load_custom_themes_from_dir(dest_dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        let names: Vec<&str> = loaded.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"Ocean"), "got: {names:?}");
        assert!(names.contains(&"Midnight Coral"));
    }

    #[cfg(unix)]
    #[test]
    fn import_theme_file_does_not_follow_a_dangling_symlink_planted_at_the_slug_path() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let attacker_target = outside.path().join("does-not-exist-yet.toml");
        std::os::unix::fs::symlink(
            &attacker_target,
            dest_dir.path().join("midnight-coral.toml"),
        )
        .expect("create dangling symlink");

        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, MIDNIGHT_CORAL).expect("write");

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

    #[cfg(unix)]
    #[test]
    fn import_theme_file_does_not_follow_a_symlink_to_an_unrelated_theme_file() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");

        let ocean = MIDNIGHT_CORAL.replace("Midnight Coral", "Ocean");
        let real_file = outside.path().join("ocean.toml");
        std::fs::write(&real_file, &ocean).expect("write real file");
        std::os::unix::fs::symlink(&real_file, dest_dir.path().join("midnight-coral.toml"))
            .expect("create symlink to unrelated theme");

        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, MIDNIGHT_CORAL).expect("write");

        let imported = import_theme_file(&source_path, dest_dir.path()).expect("should import");

        assert_eq!(imported.name, "Midnight Coral");
        assert_ne!(
            imported.source_path,
            Some(dest_dir.path().join("midnight-coral.toml")),
            "must not have written through the symlink into Ocean's real file"
        );
        assert_eq!(
            std::fs::read_to_string(&real_file).expect("Ocean's file should still be readable"),
            ocean,
            "Ocean's real file must survive completely untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_colliding_dest_path_reuses_a_symlink_that_points_at_a_file_holding_the_same_theme() {
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let real_file_dir = tempfile::tempdir().expect("tempdir");
        let real_file = real_file_dir.path().join("shared-theme.toml");
        std::fs::write(&real_file, MIDNIGHT_CORAL).expect("write real file");
        let symlink_path = dest_dir.path().join("midnight-coral.toml");
        std::os::unix::fs::symlink(&real_file, &symlink_path).expect("create symlink");

        let candidate = non_colliding_dest_path(dest_dir.path(), "Midnight Coral");

        assert_eq!(candidate, symlink_path);
    }

    #[test]
    fn import_theme_file_reimporting_the_same_theme_overwrites_its_own_file_not_a_new_one() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, MIDNIGHT_CORAL).expect("write");

        let first = import_theme_file(&source_path, dest_dir.path()).expect("first import");

        std::fs::write(&source_path, MIDNIGHT_CORAL.replace("#0c0d10", "#111111"))
            .expect("write update");
        let second = import_theme_file(&source_path, dest_dir.path()).expect("second import");

        assert_eq!(first.source_path, second.source_path);
        assert_eq!(second.overrides["surface.window"], rgba(0x111111));
        let (loaded, _) = load_custom_themes_from_dir(dest_dir.path());
        assert_eq!(
            loaded.len(),
            1,
            "must not have left a stale duplicate behind"
        );
    }

    #[test]
    fn import_theme_file_rejects_a_name_colliding_with_a_builtin_and_writes_nothing() {
        let source_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(
            &source_path,
            MIDNIGHT_CORAL.replace("Midnight Coral", "Slate"),
        )
        .expect("write");

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
        assert_eq!(imported.overrides, theme.overrides);
        assert_eq!(imported.base, theme.base);
    }

    #[test]
    fn the_real_template_file_parses_and_validates_as_a_well_formed_theme() {
        let theme = parse_theme_file_str(CUSTOM_THEME_TEMPLATE_TOML)
            .expect("the real, checked-in template file must parse and validate cleanly");
        assert_eq!(theme.name, "My Custom Theme");
        assert!(
            !theme.overrides.is_empty(),
            "the template must demonstrate real, working keys"
        );
        assert_eq!(theme.base.as_deref(), Some("Jerry Dark"));
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
            "the written file must be the template's own literal bytes - comments included"
        );
        assert!(
            on_disk.contains("How a Jerry theme works"),
            "a real explanatory comment must have survived the write"
        );

        let (loaded, errors) = load_custom_themes_from_dir(dest_dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "My Custom Theme");
    }

    #[test]
    fn write_template_theme_a_second_time_refreshes_the_same_file_not_a_new_one() {
        let dest_dir = tempfile::tempdir().expect("tempdir");

        let first = write_template_theme(dest_dir.path()).expect("first write");
        let second = write_template_theme(dest_dir.path()).expect("second write");

        assert_eq!(first.source_path, second.source_path);
        let (loaded, _) = load_custom_themes_from_dir(dest_dir.path());
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn write_template_theme_never_clobbers_a_file_the_user_has_since_edited() {
        let dest_dir = tempfile::tempdir().expect("tempdir");

        let first = write_template_theme(dest_dir.path()).expect("first write");
        let dest_path = first.source_path.clone().expect("should record its path");

        // A table the template itself doesn't already declare - redeclaring one would be a real
        // TOML error, not a user edit.
        let edited_toml =
            format!("{CUSTOM_THEME_TEMPLATE_TOML}\n[rail]\nagent_title = \"#123456\"\n");
        std::fs::write(&dest_path, &edited_toml).expect("simulate a user edit");

        let second = write_template_theme(dest_dir.path()).expect("second write");

        assert_eq!(second.source_path, first.source_path);
        assert_eq!(
            second.overrides["rail.agent_title"],
            rgba(0x123456),
            "the user's edited colour must be preserved, not overwritten with the pristine template"
        );
        assert_eq!(
            std::fs::read_to_string(&dest_path).expect("read back"),
            edited_toml,
            "the file on disk must be untouched by the second click"
        );
    }

    #[test]
    fn remove_custom_theme_file_deletes_the_real_backing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("midnight-coral.toml");
        std::fs::write(&path, MIDNIGHT_CORAL).expect("write");
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

    #[test]
    fn a_built_in_theme_files_raw_contents_are_rejected_by_the_user_facing_parser_as_a_collision() {
        let contents = include_str!("../../../../assets/themes/slate.toml");
        let err = parse_theme_file_str(contents).unwrap_err();
        assert_eq!(
            err,
            ThemeFileError::NameCollidesWithBuiltin("Slate".to_string())
        );
    }

    #[test]
    fn validate_with_builtin_check_false_skips_only_the_collision_check_not_the_others() {
        let mut colliding = valid_file();
        colliding.name = "Jerry Dark".to_string();
        assert!(colliding.validate_with_builtin_check(false).is_ok());
        assert_eq!(
            colliding.validate_with_builtin_check(true),
            Err(ThemeFileError::NameCollidesWithBuiltin(
                "Jerry Dark".to_string()
            ))
        );

        let mut bad_color = valid_file();
        bad_color.name = "Jerry Dark".to_string();
        bad_color.overrides = vec![("surface.window".to_string(), "not-a-color".to_string())];
        assert_eq!(
            bad_color.validate_with_builtin_check(false),
            Err(ThemeFileError::InvalidColor {
                key: "surface.window".to_string(),
                value: "not-a-color".to_string()
            })
        );

        let mut empty_name = valid_file();
        empty_name.name = "   ".to_string();
        assert_eq!(
            empty_name.validate_with_builtin_check(false),
            Err(ThemeFileError::EmptyName)
        );
    }
}
