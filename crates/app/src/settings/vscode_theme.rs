//! GitHub issue #141: importing a real VSCode theme JSON file and converting it into this app's
//! own five-swatch [`crate::settings::custom_theme::CustomThemeFile`] format.
//!
//! ## Scope: a real, honest palette conversion - not a rearchitecture
//!
//! `custom_theme`'s own module docs already made and documented a deliberate choice: this app's
//! whole custom-theme system re-skins ~200 tokens from exactly five swatches
//! (`background`/`panel`/three accents) via `crate::theme::derive_shift`, not a per-token
//! override file. A VSCode theme's real `tokenColors` array carries dozens of independent
//! textmate-scope colours that format cannot represent one-for-one - faithfully reproducing every
//! one of them would mean giving `crate::theme::ColorToken` a real identity so a theme file could
//! override individual tokens, a genuine architectural change to the theme system this issue's
//! own "without affecting UI visuals" constraint argues against attempting casually. This module
//! instead does the honest, bounded version: picks five real, representative colours out of a
//! VSCode theme's own `colors` map (falling back to a `tokenColors` scope search, then a Jerry
//! Dark default, for the three accents only - `background` has no default, see
//! [`VscodeThemeError::MissingBackground`]) and runs them through the *exact same*
//! [`crate::settings::custom_theme::CustomThemeFile::validate`] pipeline every hand-authored or
//! plain-TOML-imported theme already goes through - so an imported VSCode theme is genuinely
//! held to the same readability floor, the same built-in-name-collision check, real errors
//! reported the same way.
//!
//! True per-scope syntax-highlight fidelity (the issue's own "bring syntax highlighting to the
//! same level as VSCode" half) is a real, larger follow-up - flagged here rather than silently
//! dropped - and out of scope for this pass.
//!
//! ## JSONC tolerance
//!
//! Real, downloaded VSCode theme files are JSONC (`//` line comments, `/* */` block comments, and
//! trailing commas - none of which plain JSON allows), not strict JSON - [`strip_jsonc_noise`]
//! is a small, real, string-aware stripper run before `serde_json::from_str`, not a guess that
//! `serde_json` alone would happen to tolerate real-world files.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::custom_theme::CustomThemeFile;

/// Jerry Dark's own three accent swatches (`assets/themes/jerry-dark.toml`) - the last-resort
/// default for an accent this VSCode theme's own `colors`/`tokenColors` never name at all. Never
/// used for `background`/`panel` - see [`VscodeThemeError::MissingBackground`]'s own docs for why
/// those two are load-bearing enough to be real errors instead.
const DEFAULT_ACCENT_GREEN: &str = "#5cb87f";
const DEFAULT_ACCENT_AMBER: &str = "#e2a336";
const DEFAULT_ACCENT_BLUE: &str = "#74ade8";

/// Every real, specific way a VSCode theme JSON file can fail to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VscodeThemeError {
    Parse(String),
    /// Neither `colors["editor.background"]` nor a top-level `colors["background"]` parsed as a
    /// real colour - unlike the three accents (which fall back to a Jerry Dark default), a theme
    /// this app cannot even determine a background for is not a real conversion, just a guess
    /// wearing one.
    MissingBackground,
}

impl std::fmt::Display for VscodeThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VscodeThemeError::Parse(msg) => write!(f, "not a valid VSCode theme file: {msg}"),
            VscodeThemeError::MissingBackground => write!(
                f,
                "couldn't find a real `editor.background` (or top-level `background`) colour in \
                 this VSCode theme - nothing to convert"
            ),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct VscodeThemeFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    colors: HashMap<String, String>,
    #[serde(default, rename = "tokenColors")]
    token_colors: Vec<VscodeTokenColorRule>,
}

#[derive(Debug, Default, Deserialize)]
struct VscodeTokenColorRule {
    #[serde(default)]
    scope: Option<ScopeField>,
    #[serde(default)]
    settings: VscodeTokenSettings,
}

#[derive(Debug, Default, Deserialize)]
struct VscodeTokenSettings {
    #[serde(default)]
    foreground: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeField {
    One(String),
    Many(Vec<String>),
}

impl ScopeField {
    fn starts_with_any(&self, prefixes: &[&str]) -> bool {
        let matches_one = |scope: &str| {
            let scope = scope.trim();
            prefixes.iter().any(|prefix| scope.starts_with(prefix))
        };
        match self {
            ScopeField::One(scope) => scope.split(',').any(matches_one),
            ScopeField::Many(scopes) => scopes.iter().any(|scope| matches_one(scope)),
        }
    }
}

/// Strips `//` line comments, `/* */` block comments, and trailing commas before `}`/`]` from
/// `input` - real JSONC noise `serde_json` itself rejects, but genuinely present in real
/// downloaded VSCode theme files. String-aware: a `//` or `,` inside a real JSON string value
/// (an unlikely but real case - a URL in a `"name"` field, say) is left untouched, tracked by
/// toggling an "inside a string" flag on every unescaped `"` and skipping the character right
/// after a `\`.
fn strip_jsonc_noise(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            ',' => {
                // A trailing comma: everything up to the next `}`/`]` is whitespace or a comment
                // - meaning this comma has nothing real after it in its container. Comments must
                // be skipped here too, not just whitespace: `"x": 1, // trailing` looks past the
                // comment to find the real `}` right after it, rather than stopping at the `/`
                // and wrongly treating the comma as real (a live bug this exact shape caught -
                // the naive whitespace-only lookahead left a real trailing comma in the output
                // once the comment itself was stripped by the rest of this loop).
                let mut j = i + 1;
                loop {
                    while j < chars.len() && chars[j].is_whitespace() {
                        j += 1;
                    }
                    if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'/') {
                        while j < chars.len() && chars[j] != '\n' {
                            j += 1;
                        }
                    } else if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                            j += 1;
                        }
                        j = (j + 2).min(chars.len());
                    } else {
                        break;
                    }
                }
                if matches!(chars.get(j), Some('}') | Some(']')) {
                    i += 1;
                } else {
                    out.push(ch);
                    i += 1;
                }
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// Normalizes a real VSCode colour string into this app's own `#rrggbb` shape:
/// `#rgb`/`#rgba` shorthand doubled, `#rrggbb` passed through, `#rrggbbaa`'s alpha channel
/// dropped (this app's own five swatches carry no alpha - see `custom_theme`'s own module docs
/// on why `#rrggbb` is the one real shape it accepts). `None` for anything else (a named CSS
/// colour, a malformed string) - VSCode themes overwhelmingly use hex, and guessing at named
/// colours is exactly the kind of "vibe match wearing a precise-looking answer" this module's own
/// docs already reject for the load-bearing background field.
fn normalize_vscode_hex(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        3 | 4 => {
            let doubled: String = hex.chars().take(3).flat_map(|c| [c, c]).collect();
            Some(format!("#{}", doubled.to_ascii_lowercase()))
        }
        6 | 8 => Some(format!("#{}", hex[..6].to_ascii_lowercase())),
        _ => None,
    }
}

fn first_valid_color<'a>(
    colors: &HashMap<String, String>,
    keys: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    keys.into_iter().find_map(|key| {
        colors
            .get(key)
            .and_then(|value| normalize_vscode_hex(value))
    })
}

fn first_scope_foreground(
    token_colors: &[VscodeTokenColorRule],
    prefixes: &[&str],
) -> Option<String> {
    token_colors
        .iter()
        .find(|rule| {
            rule.scope
                .as_ref()
                .is_some_and(|scope| scope.starts_with_any(prefixes))
        })
        .and_then(|rule| rule.settings.foreground.as_deref())
        .and_then(normalize_vscode_hex)
}

/// A plausible second swatch when nothing in `colors` names a real sidebar/panel-shaped colour -
/// nudges `background`'s own luma a fixed step in whichever direction gives more room (lighter
/// for a dark theme, darker for a light one), so `panel` is never simply identical to
/// `background` (which would fail the shared readability floor `validate()` already enforces
/// regardless of where a swatch came from).
fn derive_panel_fallback(background_hex: &str) -> String {
    let value = u32::from_str_radix(background_hex.trim_start_matches('#'), 16).unwrap_or(0);
    let (r, g, b) = ((value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff);
    let luma = (r * 299 + g * 587 + b * 114) / 1000;
    let shift: i32 = if luma < 128 { 14 } else { -14 };
    let nudge = |channel: u32| (channel as i32 + shift).clamp(0, 255) as u32;
    format!("#{:06x}", (nudge(r) << 16) | (nudge(g) << 8) | nudge(b))
}

/// Filename stem, e.g. `dracula-pro.json` -> `"Dracula Pro"` - the real fallback display name
/// when a VSCode theme JSON file names no `"name"` field of its own, which real downloaded theme
/// files very often don't (a VSCode extension's theme *label* usually lives in its
/// `package.json`, not the theme file itself).
fn title_case_from_stem(stem: &str) -> String {
    stem.split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Converts real VSCode theme JSON `contents` into this app's own [`CustomThemeFile`] shape -
/// **not yet validated**; the caller runs it through [`CustomThemeFile::validate`] (or
/// [`super::custom_theme::import_theme_file`]'s own validate-then-write path) the same as any
/// other theme file, so a converted-but-unreadable result (e.g. `editor.background` and
/// `sideBar.background` too close in luma) is rejected the same honest way a hand-authored one
/// would be.
///
/// `source_stem` is the source file's own name without extension (e.g. `"dracula"` for
/// `dracula.json`) - the real fallback display name, see [`title_case_from_stem`].
pub fn convert_vscode_theme_str(
    contents: &str,
    source_stem: &str,
) -> Result<CustomThemeFile, VscodeThemeError> {
    let stripped = strip_jsonc_noise(contents);
    let file: VscodeThemeFile =
        serde_json::from_str(&stripped).map_err(|err| VscodeThemeError::Parse(err.to_string()))?;

    let background = first_valid_color(&file.colors, ["editor.background", "background"])
        .ok_or(VscodeThemeError::MissingBackground)?;
    let panel = first_valid_color(
        &file.colors,
        [
            "sideBar.background",
            "activityBar.background",
            "panel.background",
            "editorGroupHeader.tabsBackground",
        ],
    )
    .unwrap_or_else(|| derive_panel_fallback(&background));
    let accent_green = first_valid_color(
        &file.colors,
        [
            "terminal.ansiGreen",
            "gitDecoration.addedResourceForeground",
            "charts.green",
        ],
    )
    .or_else(|| first_scope_foreground(&file.token_colors, &["string", "markup.inserted"]))
    .unwrap_or_else(|| DEFAULT_ACCENT_GREEN.to_string());
    let accent_amber = first_valid_color(
        &file.colors,
        [
            "terminal.ansiYellow",
            "list.warningForeground",
            "charts.yellow",
        ],
    )
    .or_else(|| first_scope_foreground(&file.token_colors, &["keyword", "support.type"]))
    .unwrap_or_else(|| DEFAULT_ACCENT_AMBER.to_string());
    let accent_blue = first_valid_color(
        &file.colors,
        [
            "button.background",
            "activityBarBadge.background",
            "focusBorder",
            "textLink.foreground",
        ],
    )
    .or_else(|| {
        first_scope_foreground(
            &file.token_colors,
            &["entity.name.function", "support.function"],
        )
    })
    .unwrap_or_else(|| DEFAULT_ACCENT_BLUE.to_string());

    let name = file
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| title_case_from_stem(source_stem));

    Ok(CustomThemeFile {
        name,
        subtitle: "imported from a VSCode theme".to_string(),
        background,
        panel,
        accent_green,
        accent_amber,
        accent_blue,
    })
}

/// The real file-path entry point [`super::render::AdeApp::start_import_vscode_theme`] uses:
/// reads `source_path`, converts, and hands back an *unvalidated* [`CustomThemeFile`] plus the
/// stem [`convert_vscode_theme_str`] used - the caller still runs the shared validate-then-write
/// path, same as [`super::custom_theme::import_theme_file`].
pub fn convert_vscode_theme_file(source_path: &Path) -> Result<CustomThemeFile, VscodeThemeError> {
    let contents = std::fs::read_to_string(source_path)
        .map_err(|err| VscodeThemeError::Parse(err.to_string()))?;
    let stem = source_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    convert_vscode_theme_str(&contents, &stem)
}

/// Every real, specific way importing a VSCode theme *file* (as opposed to just converting
/// already-read text, [`convert_vscode_theme_str`]'s own concern) can fail - a real conversion
/// error, or the *converted* result failing the shared
/// [`super::custom_theme::CustomThemeFile::validate`]/write pipeline every other theme import
/// goes through (a genuine readability-floor rejection, or an I/O failure writing the canonical
/// copy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VscodeImportError {
    Convert(VscodeThemeError),
    Theme(super::custom_theme::ThemeFileError),
}

impl std::fmt::Display for VscodeImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VscodeImportError::Convert(err) => write!(f, "{err}"),
            VscodeImportError::Theme(err) => write!(f, "{err}"),
        }
    }
}

/// The real file-path entry point `crate::settings::render::AdeApp::start_import_vscode_theme`
/// uses: reads and converts `source_path`, then runs the result through the exact same
/// validate-then-write-a-canonical-copy path [`super::custom_theme::import_theme_file`] uses for
/// a plain TOML theme, so an imported VSCode theme is a real, indistinguishable
/// [`super::custom_theme::CustomTheme`] afterward - same readability floor, same collision-safe
/// destination naming, same re-import-updates-in-place behaviour.
pub fn import_vscode_theme_file(
    source_path: &Path,
    dest_dir: &Path,
) -> Result<super::custom_theme::CustomTheme, VscodeImportError> {
    let file = convert_vscode_theme_file(source_path).map_err(VscodeImportError::Convert)?;
    super::custom_theme::validate_and_write(file, dest_dir).map_err(VscodeImportError::Theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_noise_removes_line_and_block_comments_and_trailing_commas() {
        let input = r#"{
            // a line comment
            "a": 1, /* a block
                       comment */
            "b": 2,
        }"#;
        let stripped = strip_jsonc_noise(input);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).expect("valid json");
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn strip_jsonc_noise_leaves_a_double_slash_inside_a_real_string_untouched() {
        let input = r#"{"name": "https://example.com/theme"}"#;
        let stripped = strip_jsonc_noise(input);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).expect("valid json");
        assert_eq!(parsed["name"], "https://example.com/theme");
    }

    #[test]
    fn normalize_vscode_hex_handles_every_real_shape() {
        assert_eq!(normalize_vscode_hex("#1a2b3c"), Some("#1a2b3c".to_string()));
        assert_eq!(
            normalize_vscode_hex("#1A2B3C"),
            Some("#1a2b3c".to_string()),
            "must lowercase"
        );
        assert_eq!(
            normalize_vscode_hex("#1a2b3cff"),
            Some("#1a2b3c".to_string()),
            "must drop the alpha channel"
        );
        assert_eq!(
            normalize_vscode_hex("#abc"),
            Some("#aabbcc".to_string()),
            "must double shorthand digits"
        );
        assert_eq!(normalize_vscode_hex("#abcd"), Some("#aabbcc".to_string()));
        assert_eq!(normalize_vscode_hex("not-a-color"), None);
        assert_eq!(
            normalize_vscode_hex("#12345"),
            None,
            "5 digits is not a real shape"
        );
    }

    fn sample_theme_json(background: &str, extra: &str) -> String {
        format!(
            r#"{{
                "name": "Sample Theme",
                "colors": {{
                    "editor.background": "{background}"
                    {extra}
                }},
                "tokenColors": []
            }}"#
        )
    }

    #[test]
    fn a_real_minimal_theme_converts_with_jerry_dark_accent_defaults() {
        let json = sample_theme_json("#101214", "");
        let file = convert_vscode_theme_str(&json, "sample").expect("convert");
        assert_eq!(file.name, "Sample Theme");
        assert_eq!(file.background, "#101214");
        assert_eq!(file.accent_green, DEFAULT_ACCENT_GREEN);
        assert_eq!(file.accent_amber, DEFAULT_ACCENT_AMBER);
        assert_eq!(file.accent_blue, DEFAULT_ACCENT_BLUE);
        // No real sideBar/panel colour supplied - a derived fallback, not identical to
        // `background` (which would fail the shared readability floor).
        assert_ne!(file.panel, file.background);
    }

    #[test]
    fn real_colors_and_terminal_ansi_swatches_are_preferred_over_defaults() {
        let json = r##"{
            "colors": {
                "editor.background": "#101214",
                "sideBar.background": "#1a1e21",
                "terminal.ansiGreen": "#5cb87f",
                "terminal.ansiYellow": "#e2a336",
                "button.background": "#74ade8"
            }
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(file.panel, "#1a1e21");
        assert_eq!(file.accent_green, "#5cb87f");
        assert_eq!(file.accent_amber, "#e2a336");
        assert_eq!(file.accent_blue, "#74ade8");
    }

    #[test]
    fn token_colors_are_a_real_fallback_when_the_colors_map_names_no_accent() {
        let json = r##"{
            "colors": { "editor.background": "#101214" },
            "tokenColors": [
                { "scope": "string.quoted", "settings": { "foreground": "#7fd88f" } },
                { "scope": ["keyword.control", "keyword.other"], "settings": { "foreground": "#e9c46a" } }
            ]
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(file.accent_green, "#7fd88f");
        assert_eq!(file.accent_amber, "#e9c46a");
    }

    #[test]
    fn missing_editor_background_is_a_real_error_not_a_silent_default() {
        let json = r#"{ "colors": {} }"#;
        assert_eq!(
            convert_vscode_theme_str(json, "sample"),
            Err(VscodeThemeError::MissingBackground)
        );
    }

    #[test]
    fn a_top_level_background_key_is_a_real_fallback_for_editor_background() {
        let json = r##"{ "colors": { "background": "#0a0b0c" } }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(file.background, "#0a0b0c");
    }

    #[test]
    fn a_theme_with_no_name_field_falls_back_to_the_real_source_filename() {
        let json = r##"{ "colors": { "editor.background": "#101214" } }"##;
        let file = convert_vscode_theme_str(json, "dracula-pro").expect("convert");
        assert_eq!(file.name, "Dracula Pro");
    }

    #[test]
    fn malformed_json_is_a_real_parse_error() {
        let result = convert_vscode_theme_str("{ not json", "sample");
        assert!(matches!(result, Err(VscodeThemeError::Parse(_))));
    }

    #[test]
    fn a_real_jsonc_file_with_comments_and_trailing_commas_still_converts() {
        let json = r##"{
            // real theme
            "name": "Commented",
            "colors": {
                "editor.background": "#101214", // dark bg
            },
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        assert_eq!(file.name, "Commented");
        assert_eq!(file.background, "#101214");
    }

    #[test]
    fn the_converted_file_passes_the_real_shared_validate_pipeline() {
        let json = r##"{
            "name": "Round Trip Theme",
            "colors": {
                "editor.background": "#0c0d10",
                "sideBar.background": "#181a1e",
                "terminal.ansiGreen": "#5cb87f",
                "terminal.ansiYellow": "#e2a336",
                "button.background": "#e07a5f"
            }
        }"##;
        let file = convert_vscode_theme_str(json, "sample").expect("convert");
        let validated = file
            .validate()
            .expect("must pass the shared validate() pipeline");
        assert_eq!(validated.name, "Round Trip Theme");
    }
}
