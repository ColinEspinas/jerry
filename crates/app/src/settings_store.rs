//! Real, config-file-backed settings (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s
//! 2026-07-29 entry, change 3: "Settings — narrower, config-file-first, five new pages"). This
//! module owns [`Settings`] - the one struct that is genuinely loaded from, and saved back to,
//! a real `~/.config/jerry/settings.toml` on this machine - plus the pure TOML/JSON snippet
//! rendering the config banner/snippet-block widgets (`crate::root::settings_widgets`) show.
//! Deliberately separate from `crate::settings` (which stays the live-app-state pure data
//! module it already was for the Agents/Worktrees pages): this module is specifically about a
//! real file on disk, not about deriving rows from already-live in-memory state.
//!
//! ## Scope: every field here is loaded, saved, and read back by something real
//!
//! [`Settings`] only has a field for a value this phase actually wires end-to-end: written to
//! the real file, read back at startup, and consumed by at least one real render call site (see
//! `crate::root::settings_render`'s per-page docs for exactly which). This mirrors
//! `crate::settings`'s own documented refusal to render an Agents/Worktrees toggle with nothing
//! real behind it - the same discipline applied to the config-file surface: no field exists
//! here "for completeness" or because the mockup shows a row for it.
//!
//! ## TOML is the real file; JSON is a read-only alternate view
//!
//! The config banner's `TOML | JSON` segment (`crate::root::settings_widgets::render_config_banner`)
//! is real, but deliberately scoped: **TOML is the one real on-disk source of truth.** Picking
//! "JSON" re-renders the exact same already-loaded [`Settings`] value through
//! [`Settings::to_json_string`] for the snippet-block preview, and swaps the displayed path
//! string to `~/.config/jerry/settings.json` for information purposes only - it does not create,
//! write, or keep in sync a second physical file. This is a deliberate design decision, not a
//! missing feature: maintaining two real, independently-editable config files that must always
//! agree (what happens if a user hand-edits both, differently, while the app isn't running?)
//! is a materially larger feature than "preview this file's contents in another syntax," and
//! nothing in `design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3 asks for real JSON
//! persistence - its own spec text says only that the segment "switches the path and the
//! snippet below," which is exactly what [`CfgFormat`] does.
//!
//! ## What's persisted-only vs. persisted-and-applied
//!
//! [`WindowSettings::controls`] is real, persisted **and** applied -
//! `crate::root::AdeApp::window_controls_style` reads/writes it directly, so it's the real,
//! single source of truth for both the title-bar variant and the keycap glyph table (see
//! `crate::keymap::WindowControlsStyle`'s own docs).
//!
//! [`AppearanceSettings`]'s scaling fields are now also real and applied, each through its own
//! real, narrow mechanism - see each field's own doc comment for which:
//! `interface_scale_percent` scales text size only, at a real, growing (but deliberately not
//! exhaustive) list of render call sites via `crate::root::AdeApp::ui_text_size`;
//! `editor_font_size` is Surface C's real editor-zoom baseline
//! (`crate::root::AdeApp::effective_code_rem_px`); `terminal_font_size` really
//! resizes `crate::terminal_pane::TerminalPane`'s live cells, grid, and pty. `per_tab_zoom`
//! really governs which of two real zoom-persistence modes Surface C's zoom is in.
//! `follow_system_text_size` stays real-but-persisted-only, investigated and found to have no
//! real backing signal available (see `crate::root::settings_render`'s
//! `toggle_follow_system_text_size` docs for the specific, real Linux GPUI APIs checked).
//!
//! [`ThemeSettings`] round-trips correctly (set it, close and reopen the app, it's still set)
//! but nothing in the render pipeline reads it yet to re-skin colours - `crate::theme` is a set
//! of compile-time `const` colour tokens, not yet a runtime-swappable resource; a real
//! theme-swap engine is separately tracked, substantial follow-up work, not a small gap.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::keymap::WindowControlsStyle;

/// The real, on-disk (`window`) / (`appearance`) / (`theme`) shape of
/// `~/.config/jerry/settings.toml` - see the module docs for exactly which of these fields are
/// merely persisted vs. also applied. `#[serde(default)]` on the struct (and on every nested
/// struct below) means a hand-edited file missing any key - or an entire missing section -
/// parses successfully and falls back to that key's real default, rather than failing to load
/// at all: the whole point of "settings live in a file you can just edit" is defeated if a
/// small hand-edit mistake bricks the app's settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub window: WindowSettings,
    pub appearance: AppearanceSettings,
    pub theme: ThemeSettings,
}

/// `crate::root::AdeApp::window_controls_style`'s real, persisted backing - see
/// [`crate::keymap::WindowControlsStyle`]'s own "Now persisted (R3)" docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    pub controls: WindowControlsStyle,
}

/// The Appearance & scaling settings page's real, persisted fields - see the module docs'
/// "What's persisted-only vs. persisted-and-applied" section for exactly which of these are
/// now also real, applied inputs to rendering (most of them) and which is still honestly
/// persisted-only (`follow_system_text_size`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub interface_scale_percent: u16,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    pub follow_system_text_size: bool,
    pub per_tab_zoom: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            interface_scale_percent: 100,
            editor_font_size: 13.0,
            terminal_font_size: 12.5,
            follow_system_text_size: false,
            per_tab_zoom: true,
        }
    }
}

/// The real, shared bounds for [`AppearanceSettings::editor_font_size`]/
/// [`AppearanceSettings::terminal_font_size`] - the same range `crate::root::settings_render`'s
/// `adjust_editor_font_size`/`adjust_terminal_font_size` steppers already clamp every UI-driven
/// edit to (`.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX)` there, not a second, independently-chosen
/// pair of magic numbers), and the same range [`AppearanceSettings::sanitize`] clamps a freshly
/// *loaded* value to - see that method's own docs for why the load path needs this too, not
/// just the stepper.
pub const FONT_SIZE_MIN: f32 = 9.0;
pub const FONT_SIZE_MAX: f32 = 32.0;

/// The real, shared bounds for [`AppearanceSettings::interface_scale_percent`] - generous
/// headroom around the Appearance page's own four selectable presets (90/100/110/125%), not a
/// tight match to just those four: a hand-edited file choosing some other in-between or
/// slightly-outside percentage (say `140`) is still a real, plausible, non-corrupt value worth
/// keeping, unlike an obviously-wrong one (`5000`) a typo or a bad hand-edit could produce.
pub const INTERFACE_SCALE_PERCENT_MIN: u16 = 50;
pub const INTERFACE_SCALE_PERCENT_MAX: u16 = 300;

impl AppearanceSettings {
    /// Clamps every real, numeric field here into its documented real range - see
    /// [`FONT_SIZE_MIN`]/[`FONT_SIZE_MAX`]/[`INTERFACE_SCALE_PERCENT_MIN`]/
    /// [`INTERFACE_SCALE_PERCENT_MAX`]'s own docs. Ranges are only ever enforced by the
    /// stepper/choice-control mutators that drive a *UI* edit (`crate::root::settings_render`),
    /// never by [`Settings::load_or_init_at`] itself - so a hand-edited `settings.toml` (this
    /// phase's headline feature: the file is real and genuinely meant to be hand-editable) with
    /// an out-of-range value (`editor_font_size = 900.0`, `interface_scale_percent = 5000`)
    /// used to load completely verbatim, with nothing to stop a stepper/segmented-choice control
    /// from rendering with no valid-looking selection at all. Called once, right after a real
    /// file successfully deserializes (see [`Settings::load_or_init_at`]) - never silently, and
    /// never for a value that only ever came from this app's own already-range-checked UI
    /// mutators in the first place, so this is a real, additional safety net for hand edits
    /// specifically, not duplicate work.
    pub fn sanitize(&mut self) {
        self.interface_scale_percent = self
            .interface_scale_percent
            .clamp(INTERFACE_SCALE_PERCENT_MIN, INTERFACE_SCALE_PERCENT_MAX);
        self.editor_font_size = self.editor_font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        self.terminal_font_size = self.terminal_font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
    }
}

/// The Themes settings page's real, persisted fields - `name` is the currently-selected real
/// [`crate::settings::THEME_DEFS`] entry's name (`"Jerry Dark"` by default, matching that
/// fixture's own `on: true` row). See the module docs for why selecting anything else here
/// persists correctly without yet re-skinning the running app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    pub name: String,
    pub follow_system: bool,
    pub high_contrast_diff: bool,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            name: "Jerry Dark".to_string(),
            follow_system: false,
            high_contrast_diff: false,
        }
    }
}

/// The config banner's real `TOML | JSON` segment state
/// (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3) - a display-only choice, not a
/// [`Settings`] field itself (switching it never touches the real file) - see the module docs'
/// "TOML is the real file; JSON is a read-only alternate view" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CfgFormat {
    #[default]
    Toml,
    Json,
}

impl CfgFormat {
    pub fn label(self) -> &'static str {
        match self {
            CfgFormat::Toml => "TOML",
            CfgFormat::Json => "JSON",
        }
    }
}

/// Real `$HOME`-relative settings path resolution - Unix-only for now
/// (`std::env::var_os("HOME")`, no `dirs` crate dependency, matching the codebase-wide
/// convention of not adding a small-utility crate for something `std` already does directly).
/// Returns `None` on the real (if rare) case `$HOME` isn't set - callers fall back to an
/// unpersisted, in-memory [`Settings::default`] rather than panicking or guessing a path.
/// Windows/macOS home-directory resolution is explicitly out of scope for this phase -
/// `design_handoff_jerry_ade/revision/CHANGELOG.md`'s "Not design changes" note assigns
/// cross-platform work to Revision R11.
pub fn settings_toml_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("jerry")
            .join("settings.toml"),
    )
}

/// The `JSON` segment's real display-only path (`~/.config/jerry/settings.json`) - see the
/// module docs: no file is ever actually written here, this is purely the string
/// [`crate::root::settings_widgets::render_config_banner`] shows next to the segment.
pub fn settings_json_display_path() -> Option<PathBuf> {
    settings_toml_path().map(|path| path.with_extension("json"))
}

impl Settings {
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Real load from `path`. A file that fails to parse outright (not just "missing some
    /// keys" - `#[serde(default)]` already handles that case by itself, see the struct's own
    /// docs) falls back to [`Settings::default`] rather than crashing the app over a hand-edit
    /// mistake - logged via `log::warn!` so the failure is real and visible, never silently
    /// swallowed. If `path` doesn't exist yet, writes a real default file there (via
    /// [`Settings::save_at`]) so "the config file exists on first run" is genuinely true, not
    /// just "would be created eventually" - a save failure (e.g. an unwritable `$HOME`) is also
    /// logged rather than propagated, since falling back to real in-memory defaults is still a
    /// working app.
    ///
    /// A file that *does* parse still gets one more real pass: [`AppearanceSettings::sanitize`]
    /// clamps every numeric field into its documented real range, so a hand-edited, structurally
    /// valid but out-of-range value (`editor_font_size = 900.0`) can never reach the render
    /// pipeline verbatim - see that method's own docs.
    pub fn load_or_init_at(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<Settings>(&contents) {
                Ok(mut settings) => {
                    settings.appearance.sanitize();
                    settings
                }
                Err(err) => {
                    log::warn!(
                        "{} failed to parse ({err}) - using real defaults instead of crashing",
                        path.display()
                    );
                    Settings::default()
                }
            },
            Err(_) => {
                let settings = Settings::default();
                if let Err(err) = settings.save_at(path) {
                    log::warn!(
                        "failed to write default settings to {}: {err}",
                        path.display()
                    );
                }
                settings
            }
        }
    }

    /// Real save to `path` - `toml::to_string_pretty`, creating the parent directory
    /// (`~/.config/jerry/`) first if it doesn't exist yet.
    pub fn save_at(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_toml_string())
    }

    /// The real, production entry point - [`settings_toml_path`]'s real `$HOME`-resolved path,
    /// or an unpersisted in-memory default if that couldn't be resolved (see that function's
    /// own docs).
    pub fn load_or_init() -> Settings {
        match settings_toml_path() {
            Some(path) => Settings::load_or_init_at(&path),
            None => Settings::default(),
        }
    }

    /// The real, production save entry point - a no-op `Ok(())` (not an error) when
    /// [`settings_toml_path`] can't be resolved, matching [`Settings::load_or_init`]'s own
    /// honest fallback: nothing to save to, but that's not this call's failure to report.
    pub fn save(&self) -> std::io::Result<()> {
        match settings_toml_path() {
            Some(path) => self.save_at(&path),
            None => Ok(()),
        }
    }
}

/// Which real settings page a [`snippet_lines`]/[`config_keys_line`] call is for - deliberately
/// only the three pages `crate::root::settings_render` actually shows a config banner/snippet
/// block on (see that module's docs for why Agents/Worktrees/Keybindings/Language servers -
/// every other page `design_handoff_jerry_ade/revision/Jerry.dc.html`'s own `cfgKeys` fixture
/// lists - don't get one here: none of them are backed by a [`Settings`] field, so a banner
/// claiming a `settings.toml` key namespace for them would be describing a file section that
/// doesn't actually exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    General,
    Appearance,
    Theme,
}

/// The config banner's real, dot-joined key list for `page` - `design_handoff_jerry_ade/
/// revision/CHANGELOG.md`'s change 3 spec ("the page's key list in 9.5px mono ... ellipsised"),
/// rewritten from `Jerry.dc.html`'s own `cfgKeys` fixture to list only the real
/// [`Settings`] field paths this phase actually persists (that fixture's `window.
/// restore_sessions`/`environment.default`, for example, name settings this app doesn't
/// implement yet - see `crate::root::settings_render`'s General page docs).
pub fn config_keys_line(page: ConfigPage) -> &'static str {
    match page {
        ConfigPage::General => "window.controls",
        ConfigPage::Appearance => {
            "appearance.interface_scale_percent \u{b7} appearance.editor_font_size \u{b7} \
             appearance.terminal_font_size \u{b7} appearance.follow_system_text_size \u{b7} \
             appearance.per_tab_zoom"
        }
        ConfigPage::Theme => {
            "theme.name \u{b7} theme.follow_system \u{b7} theme.high_contrast_diff"
        }
    }
}

/// One line of a real config snippet, tagged for part of the design's syntax tint
/// (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 3 names three colors: "section
/// lines `#c294e0`, key lines `#a9b0b7`, comments `#4e545a`") -
/// `crate::root::settings_widgets::render_snippet_block` is the only real consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetLine {
    pub text: String,
    pub kind: SnippetLineKind,
}

/// Only the two kinds [`snippet_lines`] can actually produce - there is no `Comment` variant,
/// even though the design's own 3-colour scheme (above) names one. [`snippet_lines`] is always a
/// direct, live re-serialization of the real, already-loaded [`Settings`] value via
/// `toml::to_string_pretty`/`serde_json::to_string_pretty` (see that function's own docs for why
/// the snippet can never drift from what the real file actually contains), and neither serializer
/// ever emits a comment: there is nowhere a real inline comment could honestly come from here
/// without hand-authoring commentary text this app never actually wrote into the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetLineKind {
    Section,
    Key,
}

#[derive(Serialize)]
struct WindowSnippetDoc<'a> {
    window: &'a WindowSettings,
}

#[derive(Serialize)]
struct AppearanceSnippetDoc<'a> {
    appearance: &'a AppearanceSettings,
}

#[derive(Serialize)]
struct ThemeSnippetDoc<'a> {
    theme: &'a ThemeSettings,
}

/// Renders `page`'s real slice of `settings` (the actual, currently-loaded struct - never
/// mockup fixture text) as real TOML or JSON via [`toml::to_string_pretty`]/
/// [`serde_json::to_string_pretty`], the exact same serializers [`Settings::save_at`]/
/// [`Settings::to_json_string`] use - so this snippet is never a second, independently-
/// formatted representation that could drift from what the real file (or its JSON preview)
/// actually contains. Each field-scoped `*SnippetDoc` wrapper exists only so the real section
/// header (`[window]`, `[appearance]`, `[theme]`) appears in the output - serializing
/// `settings.window` directly would produce the same real key/value pairs but with no section
/// header, since a bare struct at the TOML document root has no name to put in brackets.
pub fn snippet_lines(settings: &Settings, page: ConfigPage, format: CfgFormat) -> Vec<SnippetLine> {
    let text = match (page, format) {
        (ConfigPage::General, CfgFormat::Toml) => toml::to_string_pretty(&WindowSnippetDoc {
            window: &settings.window,
        })
        .unwrap_or_default(),
        (ConfigPage::General, CfgFormat::Json) => serde_json::to_string_pretty(&WindowSnippetDoc {
            window: &settings.window,
        })
        .unwrap_or_default(),
        (ConfigPage::Appearance, CfgFormat::Toml) => {
            toml::to_string_pretty(&AppearanceSnippetDoc {
                appearance: &settings.appearance,
            })
            .unwrap_or_default()
        }
        (ConfigPage::Appearance, CfgFormat::Json) => {
            serde_json::to_string_pretty(&AppearanceSnippetDoc {
                appearance: &settings.appearance,
            })
            .unwrap_or_default()
        }
        (ConfigPage::Theme, CfgFormat::Toml) => toml::to_string_pretty(&ThemeSnippetDoc {
            theme: &settings.theme,
        })
        .unwrap_or_default(),
        (ConfigPage::Theme, CfgFormat::Json) => serde_json::to_string_pretty(&ThemeSnippetDoc {
            theme: &settings.theme,
        })
        .unwrap_or_default(),
    };

    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let kind = if trimmed.starts_with('[') || trimmed.ends_with('{') {
                SnippetLineKind::Section
            } else {
                SnippetLineKind::Key
            };
            SnippetLine {
                text: line.to_string(),
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_matches_the_real_documented_defaults() {
        let settings = Settings::default();
        assert_eq!(settings.window.controls, WindowControlsStyle::System);
        assert_eq!(settings.appearance.interface_scale_percent, 100);
        assert_eq!(settings.appearance.editor_font_size, 13.0);
        assert_eq!(settings.appearance.terminal_font_size, 12.5);
        assert!(!settings.appearance.follow_system_text_size);
        assert!(settings.appearance.per_tab_zoom);
        assert_eq!(settings.theme.name, "Jerry Dark");
        assert!(!settings.theme.follow_system);
        assert!(!settings.theme.high_contrast_diff);
    }

    #[test]
    fn window_controls_style_round_trips_through_the_documented_toml_spellings() {
        assert_eq!(
            toml::to_string(&WindowSettings {
                controls: WindowControlsStyle::System
            })
            .unwrap_or_default(),
            "controls = \"system\"\n"
        );
        assert_eq!(
            toml::to_string(&WindowSettings {
                controls: WindowControlsStyle::MacosStyle
            })
            .unwrap_or_default(),
            "controls = \"macos\"\n"
        );
        assert_eq!(
            toml::to_string(&WindowSettings {
                controls: WindowControlsStyle::WindowsLinuxStyle
            })
            .unwrap_or_default(),
            "controls = \"windows\"\n"
        );
    }

    #[test]
    fn a_settings_value_round_trips_through_real_toml_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut settings = Settings::default();
        settings.window.controls = WindowControlsStyle::MacosStyle;
        settings.appearance.interface_scale_percent = 125;
        settings.appearance.editor_font_size = 15.5;
        settings.theme.name = "Slate".to_string();
        settings.theme.high_contrast_diff = true;

        settings.save_at(&path).expect("save should succeed");
        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(settings, loaded);
    }

    #[test]
    fn a_missing_file_gets_a_real_default_file_written_and_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("settings.toml");
        assert!(!path.exists());

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(loaded, Settings::default());
        assert!(
            path.exists(),
            "a real default file should now exist on disk"
        );
        let contents = std::fs::read_to_string(&path).expect("read back the written file");
        let reparsed: Settings = toml::from_str(&contents).expect("written file should parse");
        assert_eq!(reparsed, Settings::default());
    }

    #[test]
    fn a_hand_edited_partial_file_still_parses_via_serde_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        // Only `window.controls` is present - `appearance`/`theme` are entirely missing
        // sections, and `window` itself is missing nothing else it could have.
        std::fs::write(&path, "[window]\ncontrols = \"windows\"\n").expect("write partial file");

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(
            loaded.window.controls,
            WindowControlsStyle::WindowsLinuxStyle
        );
        assert_eq!(loaded.appearance, AppearanceSettings::default());
        assert_eq!(loaded.theme, ThemeSettings::default());
    }

    #[test]
    fn a_hand_edited_out_of_range_appearance_value_is_clamped_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[appearance]\n\
             interface_scale_percent = 5000\n\
             editor_font_size = 900.0\n\
             terminal_font_size = -12.0\n",
        )
        .expect("write out-of-range file");

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(
            loaded.appearance.interface_scale_percent, INTERFACE_SCALE_PERCENT_MAX,
            "an absurdly large hand-edited percentage must be clamped, not loaded verbatim"
        );
        assert_eq!(loaded.appearance.editor_font_size, FONT_SIZE_MAX);
        assert_eq!(loaded.appearance.terminal_font_size, FONT_SIZE_MIN);
    }

    #[test]
    fn appearance_settings_sanitize_leaves_in_range_values_untouched() {
        let mut appearance = AppearanceSettings {
            interface_scale_percent: 110,
            editor_font_size: 15.0,
            terminal_font_size: 13.5,
            follow_system_text_size: true,
            per_tab_zoom: false,
        };
        let before = appearance.clone();

        appearance.sanitize();

        assert_eq!(appearance, before);
    }

    #[test]
    fn a_file_that_fails_to_parse_falls_back_to_defaults_instead_of_crashing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write garbage file");

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn settings_toml_path_is_rooted_at_a_real_home_directory_config_jerry() {
        let Some(path) = settings_toml_path() else {
            // No real $HOME in this environment - a legitimate, if unusual, honest `None`.
            return;
        };
        assert!(path.ends_with(".config/jerry/settings.toml"));
    }

    #[test]
    fn json_display_path_swaps_only_the_extension() {
        let Some(toml_path) = settings_toml_path() else {
            return;
        };
        let Some(json_path) = settings_json_display_path() else {
            return;
        };
        assert_eq!(json_path, toml_path.with_extension("json"));
    }

    #[test]
    fn cfg_format_labels_are_the_real_segment_captions() {
        assert_eq!(CfgFormat::Toml.label(), "TOML");
        assert_eq!(CfgFormat::Json.label(), "JSON");
        assert_eq!(CfgFormat::default(), CfgFormat::Toml);
    }

    #[test]
    fn snippet_lines_reflect_the_live_settings_value_not_static_fixture_text() {
        let mut settings = Settings::default();
        settings.window.controls = WindowControlsStyle::MacosStyle;

        let lines = snippet_lines(&settings, ConfigPage::General, CfgFormat::Toml);
        let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(joined.contains(&"[window]"));
        assert!(joined.iter().any(|l| l.contains("\"macos\"")));

        // Changing the live value changes the rendered snippet - this is a *view*, not a cache.
        settings.window.controls = WindowControlsStyle::System;
        let lines_after = snippet_lines(&settings, ConfigPage::General, CfgFormat::Toml);
        let joined_after: Vec<&str> = lines_after.iter().map(|l| l.text.as_str()).collect();
        assert!(joined_after.iter().any(|l| l.contains("\"system\"")));
        assert!(!joined_after.iter().any(|l| l.contains("\"macos\"")));
    }

    #[test]
    fn snippet_lines_classify_section_headers_and_key_lines() {
        let settings = Settings::default();
        let lines = snippet_lines(&settings, ConfigPage::Appearance, CfgFormat::Toml);
        assert_eq!(lines[0].kind, SnippetLineKind::Section);
        assert!(lines[0].text.starts_with('['));
        assert!(lines
            .iter()
            .skip(1)
            .all(|line| line.kind == SnippetLineKind::Key));
    }

    #[test]
    fn config_keys_line_only_names_real_persisted_fields() {
        assert_eq!(config_keys_line(ConfigPage::General), "window.controls");
        assert!(
            config_keys_line(ConfigPage::Appearance).contains("appearance.interface_scale_percent")
        );
        assert!(config_keys_line(ConfigPage::Theme).contains("theme.name"));
    }
}
