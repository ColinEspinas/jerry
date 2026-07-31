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
/// [`load_custom_themes_from_dir`]'s own docs for why.
const MAX_THEME_FILE_BYTES: u64 = 64 * 1024;

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
}

/// A [`CustomThemeFile`] that has already been validated - real hex colours, a non-empty name
/// that doesn't collide with a built-in theme - and is safe to hand straight to
/// `crate::theme::set_current_custom_theme` or render as a Themes-page card.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomTheme {
    pub name: String,
    pub subtitle: String,
    /// `[background, panel, green-ish, amber-ish, blue-ish]` - see the module docs.
    pub swatches: [u32; 5],
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
    InvalidColor { field: &'static str, value: String },
    NameCollidesWithBuiltin(String),
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
        Ok(CustomTheme {
            name: name.to_string(),
            subtitle: self.subtitle.trim().to_string(),
            swatches,
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
    let contents =
        std::fs::read_to_string(source_path).map_err(|err| ThemeFileError::Io(err.to_string()))?;
    let mut theme = parse_theme_file_str(&contents)?;
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
/// intentionally overwritten), anything else (a different theme, or a file that doesn't even
/// parse as one) means the slug is taken by something unrelated, so this tries
/// `{slug}-2.toml`, `{slug}-3.toml`, ... until it finds either a free path or one already holding
/// this same theme.
fn non_colliding_dest_path(dest_dir: &Path, name: &str) -> PathBuf {
    let base_slug = slugify(name);
    let mut candidate = dest_dir.join(format!("{base_slug}.toml"));
    let mut suffix = 2;
    while candidate.exists() {
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
}
