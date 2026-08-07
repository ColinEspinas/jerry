//! Config-file-backed settings (`CHANGELOG.md`'s 2026-07-29 entry, change 3: "Settings —
//! narrower, config-file-first, five new pages"). Owns [`Settings`] - the struct loaded from,
//! and saved back to, `~/.config/jerry/settings.toml` - plus the TOML/JSON snippet rendering the
//! config banner/snippet-block widgets (`crate::settings::widgets`) show. Deliberately
//! separate from `crate::settings::state`, which stays about already-live in-memory app state rather
//! than a file on disk.
//!
//! ## Scope: every field here is loaded, saved, and read back by something real
//!
//! [`Settings`] only has a field for a value this phase wires end-to-end: written to the file,
//! read back at startup, and consumed by at least one render call site (see
//! `crate::settings::render`'s per-page docs for which) - no field exists here "for
//! completeness" just because the mockup shows a row for it.
//!
//! There used to be a documented exception here - `FileTreeSettings::max_entries`, the file tree
//! walk's entry cap (GitHub issue #18), a real file-only tunable with no settings page. GitHub
//! issue #160 removed the cap itself ("File tree should load all folders and files"), so the
//! field went with it rather than staying on as a value nothing reads.
//!
//! [`EditorSettings`] (GitHub issue #26) is the same kind of file-only tunable: real, applied
//! defaults for Tab/Shift+Tab indentation (`crate::code_surface::editing`'s
//! `handle_editor_indent_action`/`handle_editor_dedent_action` read it as the fallback once no
//! `.editorconfig` file sets a given property - see `crate::code_surface::indent`'s own docs for
//! the real resolution order), with no settings-page UI of its own yet.
//!
//! ## TOML is the real file; JSON is a read-only alternate view
//!
//! The config banner's `TOML | JSON` segment (`crate::settings::widgets::render_config_banner`)
//! is real, but **TOML is the one on-disk source of truth.** Picking "JSON" re-renders the same
//! already-loaded [`Settings`] value through [`Settings::to_json_string`] for the snippet-block
//! preview and swaps the displayed path to `~/.config/jerry/settings.json` for information
//! purposes only - no second physical file is ever created or kept in sync. Maintaining two
//! independently-editable config files that must always agree is a materially larger feature
//! than previewing one file's contents in another syntax, and `CHANGELOG.md`'s change 3 only
//! asks for the path/snippet to switch, which is what [`CfgFormat`] does.
//!
//! ## What's persisted-only vs. persisted-and-applied
//!
//! [`WindowSettings::controls`] is persisted **and** applied -
//! `crate::root::AdeApp::window_controls_style` reads/writes it directly as the single source of
//! truth for both the title-bar variant and the keycap glyph table.
//!
//! [`AppearanceSettings`]'s scaling fields are each applied through their own narrow mechanism -
//! see each field's own doc comment: `interface_scale_percent` scales text size at a growing
//! (not exhaustive) list of render call sites via `crate::root::AdeApp::ui_text_size`;
//! `editor_font_size` and `editor_zoom_percent` together are Surface C's editor-zoom baseline and
//! multiplier (`crate::root::AdeApp::effective_code_rem_px`); `terminal_font_size` resizes
//! `crate::terminal::pane::TerminalPane`'s live cells, grid, and pty. `follow_system_text_size`
//! stays persisted-only - investigated and found to have no real backing signal available (see
//! `crate::settings::render`'s `toggle_follow_system_text_size` docs for the specific Linux
//! GPUI APIs checked). `caret_style`/`caret_blink` (GitHub issue #27) are both real, applied
//! inputs too - `crate::code_surface::editing::render_editable_file_view_line` reads
//! `caret_style` to choose the painted caret shape (line/block/underline) and `caret_blink`
//! (alongside the real, always-available `gpui::App::reduce_motion` - see
//! `crate::root::caret_blink`'s module docs for why that, not real OS-level
//! prefers-reduced-motion detection, is the honest mechanism this GPUI version exposes) gates
//! whether `crate::root::AdeApp`'s shared blink loop ever starts at all.
//!
//! ## Editor zoom is one global, persisted number now (was three overlapping mechanisms)
//!
//! Before this consolidation, "how big is the editor text" was governed by three separate,
//! overlapping mechanisms: this same persisted `editor_font_size` baseline; an in-memory-only
//! `AdeApp::code_zoom_percent` multiplier that got reset to 100% on every worktree switch
//! (`AdeApp::select_worktree`); and an optional `AdeApp::file_zoom_percent` per-open-file
//! override, gated by a `per_tab_zoom` toggle. None of the last two survived an app restart, and
//! the worktree-reset meant even a single agent's zoom wasn't stable while browsing worktrees.
//! `editor_zoom_percent` replaces both: one real, `settings.toml`-persisted multiplier, applied
//! uniformly to every open file, in every worktree, exactly like `editor_font_size` itself
//! already was. `per_tab_zoom`, `AdeApp::file_zoom_percent`, and the worktree-reset are gone
//! entirely - see `crate::code_surface`'s zoom methods for the surviving mechanism.
//!
//! [`ThemeSettings`] round-trips **and** really re-skins the running app: `crate::root::AdeApp::
//! apply_theme_selection` compiles `name` into a real palette and installs it through
//! `crate::theme::set_current_theme` (see that module's own docs for the runtime colour-token
//! mechanism this drives, and `crate::settings::custom_theme` for the file format), and
//! `follow_system`
//! is real too (`crate::root::AdeApp::sync_theme_to_system_appearance`, a live
//! `Window::observe_window_appearance` subscription). `high_contrast_diff` stays persisted-only -
//! no real diff-colour-intensity mechanism exists yet to apply it through.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::keymap::WindowControlsStyle;

/// The real, on-disk (`window`) / (`appearance`) / (`theme`) shape of
/// `~/.config/jerry/settings.toml` - see the module docs for exactly which of these fields are
/// merely persisted vs. also applied. `#[serde(default)]` on the struct (and every nested struct
/// below) means a hand-edited file missing a key, or an entire section, parses successfully and
/// falls back to that key's default rather than failing to load at all.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub window: WindowSettings,
    pub appearance: AppearanceSettings,
    pub theme: ThemeSettings,
    pub keymap: KeymapSettings,
    pub blame: BlameSettings,
    pub editor: EditorSettings,
    pub icon_pack: IconPackSettings,
}

/// `crate::icon_pack`'s persisted backing (GitHub issue #5's "custom icon packs") - `None` means
/// the app's own default, built-in icons (styled shapes/glyphs, no image assets), never a
/// fabricated "empty pack" state. See that module's own docs for how `directory` is resolved
/// into a real icon at render time.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IconPackSettings {
    pub directory: Option<PathBuf>,
}

/// `crate::root::AdeApp::window_controls_style`'s persisted backing - see
/// [`crate::keymap::WindowControlsStyle`]'s own "Now persisted (R3)" docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    pub controls: WindowControlsStyle,
}

/// The Appearance & scaling settings page's persisted fields - see the module docs'
/// "What's persisted-only vs. persisted-and-applied" section for which are also applied inputs
/// to rendering (most of them) and which is persisted-only (`follow_system_text_size`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub interface_scale_percent: u16,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    pub follow_system_text_size: bool,
    /// Surface C's global editor-zoom multiplier, applied on top of [`Self::editor_font_size`]
    /// (`crate::root::AdeApp::effective_code_rem_px`) - see the module docs' "Editor zoom is one
    /// global, persisted number now" section for why this replaced three overlapping mechanisms.
    /// A percentage (`100` = unchanged), not raw pixels, so the toolbar's existing "100%"/"130%"
    /// display and `code_surface::zoom::clamp_zoom_percent`'s step logic carry over unchanged - only
    /// *where* the number lives (persisted here, globally) changed.
    pub editor_zoom_percent: u16,
    /// The code editor's real painted caret shape (GitHub issue #27) - see [`CaretStyle`]'s own
    /// docs for what each variant paints.
    pub caret_style: CaretStyle,
    /// Whether the caret blinks while idle (GitHub issue #27's "no blink" setting). `true`
    /// (blinking) is the default, matching every mainstream editor's own default; `false` keeps
    /// the caret permanently solid whenever it would otherwise be visible - see
    /// `crate::root::caret_blink`'s module docs for the real blink mechanism this gates.
    pub caret_blink: bool,
    /// Whether the code editor draws real vertical indent-guide lines (GitHub issue #122: "Add
    /// settings to display indents in code editor"). `true` by default, matching every mainstream
    /// editor's own default. `crate::code_surface::editing::render_editable_file_view_line` is
    /// the only real consumer - it draws one line per real indent level
    /// (`crate::code_surface::indent::leading_indent_levels`), spaced by a real, measured
    /// monospace character width rather than a hardcoded pixel constant, so the guides never
    /// drift from the file's own actual leading whitespace.
    pub show_indent_guides: bool,
    /// Whether matched bracket pairs are coloured by nesting depth (GitHub issue #168's
    /// bracket-pair colorization - what VSCode calls "Bracket Pair Colorization"). `true` by
    /// default: the feature shipped enabled, so this is a real opt-*out*, not an opt-in that
    /// would silently turn it off for everyone who already has it.
    ///
    /// Unlike every other toggle in this struct, this one is not merely read at paint time - it
    /// is an input to *span production*
    /// (`crate::code_surface::code_view::HighlightOptions::apply`, reached through the
    /// `*_with_options` highlight entry points), because the depth ring is carried as real
    /// `HighlightKind` variants rather than a render-time colour choice. Cached `RenderedLine`s
    /// therefore have to be rebuilt when it changes, which is what
    /// `crate::root::AdeApp::invalidate_syntax_highlighting` exists for - see
    /// `crate::settings::render::AdeApp::toggle_bracket_pair_colorization`.
    pub bracket_pair_colorization: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            interface_scale_percent: 100,
            editor_font_size: 13.0,
            terminal_font_size: 12.5,
            follow_system_text_size: false,
            editor_zoom_percent: EDITOR_ZOOM_PERCENT_DEFAULT,
            caret_style: CaretStyle::default(),
            caret_blink: true,
            show_indent_guides: true,
            bracket_pair_colorization: true,
        }
    }
}

/// The code editor's real painted caret shape (GitHub issue #27: "caret width and style
/// configurable (line / block / underline) in user settings"). Read by
/// `crate::code_surface::editing::render_editable_file_view_line`, which is the only real
/// consumer - see that function's own docs for the exact quad each variant paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CaretStyle {
    /// A thin vertical bar just before the character at the caret - every mainstream editor's
    /// own default, and this app's pre-issue-#27 behavior.
    #[default]
    #[serde(rename = "line")]
    Line,
    /// A filled block the width of the character at the caret (or [`CARET_BLOCK_FALLBACK_WIDTH`]
    /// at the real end of a line, where there is no character to measure).
    #[serde(rename = "block")]
    Block,
    /// A thin horizontal bar under the character at the caret.
    #[serde(rename = "underline")]
    Underline,
}

impl CaretStyle {
    /// The Appearance settings page's label for this style.
    pub fn label(self) -> &'static str {
        match self {
            CaretStyle::Line => "Line",
            CaretStyle::Block => "Block",
            CaretStyle::Underline => "Underline",
        }
    }
}

/// Shared bounds for [`AppearanceSettings::editor_font_size`]/
/// [`AppearanceSettings::terminal_font_size`] - the same range the Appearance page's steppers
/// clamp UI edits to, and [`AppearanceSettings::sanitize`] clamps freshly loaded values to.
pub const FONT_SIZE_MIN: f32 = 9.0;
pub const FONT_SIZE_MAX: f32 = 32.0;

/// Shared bounds for [`AppearanceSettings::interface_scale_percent`] - generous headroom around
/// the Appearance page's four selectable presets (90/100/110/125%), not a tight match to just
/// those four: a hand-edited file choosing some other in-between or slightly-outside percentage
/// is still plausible and worth keeping, unlike an obviously-wrong one a bad hand-edit produces.
pub const INTERFACE_SCALE_PERCENT_MIN: u16 = 50;
pub const INTERFACE_SCALE_PERCENT_MAX: u16 = 300;

/// Editor-zoom range (70-200%, in steps of 10) and default (100%) - the single real source for
/// these bounds. `crate::code_surface` re-exports these as `AdeApp::ZOOM_*` associated
/// consts (unchanged names, so its own call sites and doc comments didn't need to move) rather
/// than duplicating the literals.
pub const EDITOR_ZOOM_PERCENT_MIN: u16 = 70;
pub const EDITOR_ZOOM_PERCENT_MAX: u16 = 200;
pub const EDITOR_ZOOM_PERCENT_STEP: u16 = 10;
pub const EDITOR_ZOOM_PERCENT_DEFAULT: u16 = 100;

impl AppearanceSettings {
    /// Clamps every numeric field into its documented range. UI mutators
    /// (`crate::settings::render`) already clamp their own edits; this exists so a
    /// hand-edited `settings.toml` with an out-of-range value (`editor_font_size = 900.0`)
    /// can't reach the render pipeline verbatim. Called once, right after a file successfully
    /// deserializes (see [`Settings::load_or_init_at`]).
    pub fn sanitize(&mut self) {
        self.interface_scale_percent = self
            .interface_scale_percent
            .clamp(INTERFACE_SCALE_PERCENT_MIN, INTERFACE_SCALE_PERCENT_MAX);
        self.editor_font_size = self.editor_font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        self.terminal_font_size = self.terminal_font_size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        self.editor_zoom_percent = self
            .editor_zoom_percent
            .clamp(EDITOR_ZOOM_PERCENT_MIN, EDITOR_ZOOM_PERCENT_MAX);
    }
}

/// The Themes settings page's persisted fields - `name` is the currently-selected
/// [`crate::settings::state::THEME_DEFS`] entry's name (`"Jerry Dark"` by default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    pub name: String,
    pub follow_system: bool,
    /// The most recent *dark* theme `name` was ever explicitly set to (defaults to `"Jerry
    /// Dark"`) - real, persisted memory for `follow_system`'s own real OS-dark-signal handling
    /// (`crate::root::AdeApp::apply_follow_system_appearance`): without this, a user on e.g.
    /// "Slate" who turns `follow_system` on and later has their OS switch to light-then-dark
    /// again would land back on the hardcoded default "Jerry Dark" rather than their own real,
    /// previously-chosen dark theme - a real, reported data-loss gap an audit caught. Updated by
    /// `crate::root::AdeApp::set_theme_name` every time a real, non-"Paper" (i.e. not the one
    /// light theme) selection is made, so it always reflects the last dark theme a user actually
    /// picked, whether or not `follow_system` was on at the time.
    pub last_dark_theme: String,
    pub high_contrast_diff: bool,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            name: "Jerry Dark".to_string(),
            follow_system: false,
            last_dark_theme: "Jerry Dark".to_string(),
            high_contrast_diff: false,
        }
    }
}

/// The Keybindings settings page's persisted rebinds - see `crate::keymap_overrides`'s own
/// module docs for the real mechanism this backs (identity, collision detection, and how
/// `overrides` is turned into a real, effective `Vec<gpui::KeyBinding>` on top of
/// `crate::default_key_bindings()`). A flat `Vec`, not a table keyed by some derived id:
/// [`KeybindingOverride`]'s own three identity fields already are the real, stable identity
/// (`keymap_overrides::BindingIdentity`), and a list round-trips through TOML more simply than a
/// table keyed by a composite string would.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeymapSettings {
    pub overrides: Vec<KeybindingOverride>,
}

/// One real, persisted keybinding rebind - see [`KeymapSettings::overrides`]'s own docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingOverride {
    /// The rebound action's real `gpui::Action::name()` (e.g. `"app::TogglePalette"`).
    pub action: String,
    /// The rebound binding's real registered context predicate's `Display` string, or the
    /// literal `"global"` when unscoped - matches `crate::settings::state::KeybindingRow::context`'s own
    /// convention exactly, so the two never need reconciling.
    pub context: String,
    /// The *default* keystroke(s) this override replaces, space-joined via
    /// `gpui::Keystroke::unparse()` - part of the real identity (see `keymap_overrides`'s module
    /// docs), not just informational: needed to disambiguate two default bindings that already
    /// share the same action/context (e.g. `CompletionsAccept` is bound to both `tab` and
    /// `enter`, both under `"file-editor && completions"`).
    pub default_keystrokes: String,
    /// The new keystroke chord, in the same `gpui::Keystroke::parse`-compatible, space-joined
    /// `unparse()` form as [`Self::default_keystrokes`].
    pub keystrokes: String,
}

/// Inline git blame (GitHub issue #29): whether Surface C's File view shows the current line's
/// author/relative-date/summary, dimmed, at the end of the line - see
/// `crate::code_surface::blame_view`'s own module docs for the real off-thread/caching mechanism
/// this gates, and `crate::settings::render`'s General page for the one real, wired toggle row
/// backing this field (`Self::show_inline`).
///
/// There is deliberately no `show_gutter` field here: GitHub issue #29 also asks for a secondary
/// gutter/full-file blame view, which this phase does not implement (see
/// `crate::code_surface::blame_view`'s own "Scope" docs) - a persisted setting with no real
/// feature behind it would be exactly the "looks wired up but isn't" this project's conventions
/// forbid, so it isn't added until the feature it would gate actually exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlameSettings {
    /// Defaults to `true` (issue #29's own suggested default: "inline blame on, full gutter
    /// off") - most users reviewing code want to see who last touched the current line without
    /// an extra keystroke; it can be turned off entirely from the General settings page.
    pub show_inline: bool,
}

impl Default for BlameSettings {
    fn default() -> Self {
        Self { show_inline: true }
    }
}

/// Surface C's minimap - `crate::code_surface::minimap`'s own real, persisted settings
/// (GitHub issue #30's `editor.minimap.enabled`). This is one of two genuinely-backed fields on
/// the `Editor` settings page today (see `crate::settings::state`'s own module docs on why the
/// rest of that page still stays a placeholder) - `minimap_enabled` toggles
/// `crate::code_surface::minimap::AdeApp::render_minimap` on/off directly, and
/// `minimap_scale_percent` is the real multiplier that module's own `panel_width`/`char_width`/
/// `effective_line_height` apply. `insert_spaces`/`tab_width` are GitHub issue #26's real
/// Tab/Shift+Tab indentation defaults - see the module docs' "One documented exception" section
/// for why they have no settings page yet. `tab_width` is both "how many literal spaces one
/// indent level inserts" (when `insert_spaces`) and "how wide one literal `\t` counts as for
/// `Shift+Tab`'s own dedent" - `crate::code_surface::indent::indent_settings_for_path` overrides
/// either field from a real `.editorconfig` file when one applies to the file being edited; these
/// are only the fallback once no `.editorconfig` sets a given property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    pub minimap_enabled: bool,
    pub minimap_scale_percent: u16,
    pub insert_spaces: bool,
    pub tab_width: u8,
    /// Whether accepting a completion also applies the item's own `additionalTextEdits` - in
    /// practice, the `import`/`use` line a language server wants added for a symbol that isn't in
    /// scope yet. Read by `crate::lsp::completion_popup::AdeApp::accept_active_completion`.
    ///
    /// On by default, because the alternative is inserting a name that doesn't resolve. Worth a
    /// switch anyway, and live-requested: a server offers auto-imports for everything its own
    /// index can reach, which in a browser project means `@types/node` - `import { appendFile }
    /// from 'node:fs'` is valid TypeScript there and still cannot be bundled. Off, an accepted
    /// completion inserts the name alone and leaves the import to the user.
    pub auto_import: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            minimap_enabled: true,
            minimap_scale_percent: MINIMAP_SCALE_PERCENT_DEFAULT,
            insert_spaces: true,
            tab_width: EDITOR_TAB_WIDTH_DEFAULT,
            auto_import: true,
        }
    }
}

/// [`EditorSettings::minimap_scale_percent`]'s real bounds/default/step - the same
/// percentage-multiplier convention [`EDITOR_ZOOM_PERCENT_MIN`]/`_MAX`/`_DEFAULT`/`_STEP` already
/// established for [`AppearanceSettings::editor_zoom_percent`].
pub const MINIMAP_SCALE_PERCENT_MIN: u16 = 50;
pub const MINIMAP_SCALE_PERCENT_MAX: u16 = 200;
pub const MINIMAP_SCALE_PERCENT_DEFAULT: u16 = 100;
pub const MINIMAP_SCALE_PERCENT_STEP: u16 = 25;

/// Bounds for [`EditorSettings::tab_width`] - `1` (a real, if unusual, minimum) through `16`
/// (comfortably more than any real project's own configured indent width), matching
/// [`AppearanceSettings::sanitize`]'s own "hand-edited file gets clamped, not rejected" discipline.
pub const EDITOR_TAB_WIDTH_MIN: u8 = 1;
pub const EDITOR_TAB_WIDTH_MAX: u8 = 16;
pub const EDITOR_TAB_WIDTH_DEFAULT: u8 = 4;

impl EditorSettings {
    /// Clamps a hand-edited `minimap_scale_percent`/`tab_width` into their documented ranges -
    /// the same [`AppearanceSettings::sanitize`] discipline applied here, called once at load
    /// time (see [`Settings::load_or_init_at`]).
    pub fn sanitize(&mut self) {
        self.minimap_scale_percent = self
            .minimap_scale_percent
            .clamp(MINIMAP_SCALE_PERCENT_MIN, MINIMAP_SCALE_PERCENT_MAX);
        self.tab_width = self
            .tab_width
            .clamp(EDITOR_TAB_WIDTH_MIN, EDITOR_TAB_WIDTH_MAX);
    }
}

/// The config banner's `TOML | JSON` segment state (`CHANGELOG.md`'s change 3) - a display-only
/// choice, not a [`Settings`] field (switching it never touches the file) - see the module docs'
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

/// `$HOME`-relative settings path resolution - Unix-only for now (`std::env::var_os("HOME")`,
/// no `dirs` crate dependency). Returns `None` if `$HOME` isn't set - callers fall back to an
/// unpersisted, in-memory [`Settings::default`] rather than panicking or guessing a path.
/// Windows/macOS home-directory resolution is out of scope for now (see `BUILD-LOG.md`).
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
/// [`crate::settings::widgets::render_config_banner`] shows next to the segment.
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

    /// Loads from `path`. A file that fails to parse outright falls back to [`Settings::default`]
    /// rather than crashing the app over a hand-edit mistake, logged via `log::warn!`. If `path`
    /// doesn't exist yet, writes a default file there (via [`Settings::save_at`]) so the config
    /// file exists on first run; a save failure there is also logged rather than propagated. A
    /// file that does parse still gets every section's own `sanitize` applied
    /// ([`AppearanceSettings::sanitize`], [`EditorSettings::sanitize`]).
    pub fn load_or_init_at(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<Settings>(&contents) {
                Ok(mut settings) => {
                    settings.appearance.sanitize();
                    settings.editor.sanitize();
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

    /// Saves to `path`, creating the parent directory first if needed. A plain, non-atomic
    /// `std::fs::write` (truncate-then-write), not write-to-temp-then-rename -
    /// `crate::root::AdeApp`'s serial settings-save writer loop (see its `_settings_save_task`
    /// field docs) only ever guarantees at most one `save_at` call in flight at a time, not
    /// crash- or external-reader-safety.
    pub fn save_at(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_toml_string())
    }

    /// The production entry point - [`settings_toml_path`]'s `$HOME`-resolved path, or an
    /// unpersisted in-memory default if that couldn't be resolved.
    pub fn load_or_init() -> Settings {
        match settings_toml_path() {
            Some(path) => Settings::load_or_init_at(&path),
            None => Settings::default(),
        }
    }

    /// The production save entry point - a no-op `Ok(())`, not an error, when
    /// [`settings_toml_path`] can't be resolved, matching [`Settings::load_or_init`]'s fallback.
    pub fn save(&self) -> std::io::Result<()> {
        match settings_toml_path() {
            Some(path) => self.save_at(&path),
            None => Ok(()),
        }
    }
}

/// Which settings page a [`snippet_lines`]/[`config_keys_line`] call is for - only the four
/// pages `crate::settings::render` shows a config banner/snippet block on. Every other page
/// `Jerry.dc.html`'s own `cfgKeys` fixture lists isn't backed by a [`Settings`] field, so a
/// banner for it would describe a file section that doesn't exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    General,
    Appearance,
    Theme,
    Editor,
}

/// The config banner's dot-joined key list for `page` - rewritten from `Jerry.dc.html`'s own
/// `cfgKeys` fixture to list only the [`Settings`] field paths this app actually persists (that
/// fixture also names settings this app doesn't implement, e.g. `window.restore_sessions`).
pub fn config_keys_line(page: ConfigPage) -> &'static str {
    match page {
        ConfigPage::General => "window.controls",
        ConfigPage::Appearance => {
            "appearance.interface_scale_percent \u{b7} appearance.editor_font_size \u{b7} \
             appearance.terminal_font_size \u{b7} appearance.follow_system_text_size \u{b7} \
             appearance.editor_zoom_percent \u{b7} appearance.caret_style \u{b7} \
             appearance.caret_blink \u{b7} appearance.show_indent_guides \u{b7} \
             appearance.bracket_pair_colorization"
        }
        ConfigPage::Theme => {
            "theme.name \u{b7} theme.follow_system \u{b7} theme.high_contrast_diff"
        }
        ConfigPage::Editor => "editor.minimap_enabled \u{b7} editor.minimap_scale_percent",
    }
}

/// One line of a config snippet, tagged for the design's syntax tint (`CHANGELOG.md`'s change 3
/// names three colours: section, key, comment) - `crate::settings::widgets::render_snippet_block`
/// is the only consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetLine {
    pub text: String,
    pub kind: SnippetLineKind,
}

/// Only the two kinds [`snippet_lines`] can actually produce - there is no `Comment` variant,
/// even though the design's own colour scheme names one: [`snippet_lines`] always re-serializes
/// the live [`Settings`] value via `toml`/`serde_json`, and neither serializer ever emits a
/// comment, so there's nowhere one could honestly come from here.
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

#[derive(Serialize)]
struct EditorSnippetDoc<'a> {
    editor: &'a EditorSettings,
}

/// Renders `page`'s slice of `settings` (the currently-loaded struct, never mockup fixture text)
/// as TOML or JSON via the same serializers [`Settings::save_at`]/[`Settings::to_json_string`]
/// use, so this can't drift from what the file (or its JSON preview) actually contains. Each
/// field-scoped `*SnippetDoc` wrapper exists only so a section header (`[window]`, etc.) appears
/// in the output - a bare struct at the TOML document root has no name to put in brackets.
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
        (ConfigPage::Editor, CfgFormat::Toml) => toml::to_string_pretty(&EditorSnippetDoc {
            editor: &settings.editor,
        })
        .unwrap_or_default(),
        (ConfigPage::Editor, CfgFormat::Json) => serde_json::to_string_pretty(&EditorSnippetDoc {
            editor: &settings.editor,
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
        assert_eq!(settings.appearance.editor_zoom_percent, 100);
        assert_eq!(settings.theme.name, "Jerry Dark");
        assert!(!settings.theme.follow_system);
        assert_eq!(settings.theme.last_dark_theme, "Jerry Dark");
        assert!(!settings.theme.high_contrast_diff);
        assert!(
            settings.keymap.overrides.is_empty(),
            "a fresh install has no real rebinds yet"
        );
        assert!(
            settings.editor.minimap_enabled,
            "the minimap is on by default"
        );
        assert_eq!(
            settings.editor.minimap_scale_percent,
            MINIMAP_SCALE_PERCENT_DEFAULT
        );
        assert!(settings.editor.insert_spaces);
        assert_eq!(settings.editor.tab_width, EDITOR_TAB_WIDTH_DEFAULT);
    }

    #[test]
    fn a_hand_edited_editor_tab_width_round_trips_and_an_absurd_one_is_clamped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[editor]\ninsert_spaces = false\ntab_width = 2\n").expect("write");
        let loaded = Settings::load_or_init_at(&path);
        assert!(!loaded.editor.insert_spaces);
        assert_eq!(loaded.editor.tab_width, 2);

        std::fs::write(&path, "[editor]\ntab_width = 200\n").expect("write");
        assert_eq!(
            Settings::load_or_init_at(&path).editor.tab_width,
            EDITOR_TAB_WIDTH_MAX,
            "an absurd tab width is clamped down, not honoured"
        );

        std::fs::write(&path, "[editor]\ntab_width = 0\n").expect("write");
        assert_eq!(
            Settings::load_or_init_at(&path).editor.tab_width,
            EDITOR_TAB_WIDTH_MIN,
            "a zero tab width is clamped up, not honoured"
        );
    }

    /// GitHub issue #160 deleted `[file_tree] max_entries` outright. A `settings.toml` written by
    /// an older build still has that section in it, and it must load as a plain no-op rather than
    /// failing the whole file back to defaults and quietly losing every other setting with it.
    #[test]
    fn a_settings_file_still_carrying_the_removed_file_tree_cap_loads_the_rest_of_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[file_tree]\nmax_entries = 50000\n\n[editor]\ntab_width = 2\n",
        )
        .expect("write");

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(
            loaded.editor.tab_width, 2,
            "the removed section must be ignored, not treated as a parse failure that discards \
             the whole file"
        );
    }

    #[test]
    fn a_hand_edited_minimap_scale_round_trips_and_an_out_of_range_one_is_clamped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[editor]\nminimap_enabled = false\nminimap_scale_percent = 150\n",
        )
        .expect("write");
        let loaded = Settings::load_or_init_at(&path);
        assert!(!loaded.editor.minimap_enabled);
        assert_eq!(loaded.editor.minimap_scale_percent, 150);

        std::fs::write(
            &path,
            "[editor]\nminimap_enabled = true\nminimap_scale_percent = 9000\n",
        )
        .expect("write");
        assert_eq!(
            Settings::load_or_init_at(&path)
                .editor
                .minimap_scale_percent,
            MINIMAP_SCALE_PERCENT_MAX,
            "an absurdly large scale is clamped down, not honoured"
        );

        std::fs::write(
            &path,
            "[editor]\nminimap_enabled = true\nminimap_scale_percent = 1\n",
        )
        .expect("write");
        assert_eq!(
            Settings::load_or_init_at(&path)
                .editor
                .minimap_scale_percent,
            MINIMAP_SCALE_PERCENT_MIN,
            "an absurdly small scale is clamped up, not honoured"
        );
    }

    /// An old `settings.toml` written before the minimap existed has no `[editor]` section at
    /// all - `#[serde(default)]` must fall back to the real defaults (minimap on) rather than
    /// failing the whole parse, the same real fallback
    /// `an_old_settings_toml_missing_the_keymap_section_entirely_still_loads_cleanly` already
    /// proves for `[keymap]`.
    #[test]
    fn an_old_settings_toml_missing_the_editor_section_entirely_still_loads_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[window]\ncontrols = \"system\"\n").expect("write old file");

        let loaded = Settings::load_or_init_at(&path);

        assert!(loaded.editor.minimap_enabled);
        assert_eq!(
            loaded.editor.minimap_scale_percent,
            MINIMAP_SCALE_PERCENT_DEFAULT
        );
    }

    /// An old `settings.toml` written before keybinding rebinding existed has no `[keymap]`
    /// section at all - `#[serde(default)]` on [`Settings`] must fall back to an empty
    /// `overrides` list rather than failing the whole parse.
    #[test]
    fn an_old_settings_toml_missing_the_keymap_section_entirely_still_loads_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[window]\ncontrols = \"system\"\n").expect("write old file");

        let loaded = Settings::load_or_init_at(&path);

        assert!(loaded.keymap.overrides.is_empty());
    }

    #[test]
    fn a_real_keybinding_override_round_trips_through_toml_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut settings = Settings::default();
        settings.keymap.overrides.push(KeybindingOverride {
            action: "app::TogglePalette".to_string(),
            context: "global".to_string(),
            default_keystrokes: "ctrl-p".to_string(),
            keystrokes: "ctrl-shift-p".to_string(),
        });

        settings.save_at(&path).expect("save should succeed");
        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(settings, loaded);
        assert_eq!(loaded.keymap.overrides.len(), 1);
        assert_eq!(loaded.keymap.overrides[0].keystrokes, "ctrl-shift-p");
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
        settings.appearance.show_indent_guides = false;
        settings.theme.name = "Slate".to_string();
        settings.theme.high_contrast_diff = true;

        settings.save_at(&path).expect("save should succeed");
        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(settings, loaded);
    }

    /// GitHub issue #122's `show_indent_guides` toggle, round-tripped through real TOML
    /// save/load exactly like [`a_settings_value_round_trips_through_real_toml_save_and_load`]
    /// above covers several other `AppearanceSettings` fields - a dedicated test so a future
    /// change to that shared fixture can't silently stop covering this one field.
    #[test]
    fn show_indent_guides_round_trips_through_real_toml_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        assert!(
            Settings::default().appearance.show_indent_guides,
            "sanity check: the real default is guides on"
        );

        let mut settings = Settings::default();
        settings.appearance.show_indent_guides = false;
        settings.save_at(&path).expect("save should succeed");
        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(settings, loaded);
        assert!(!loaded.appearance.show_indent_guides);
    }

    /// GitHub issue #168's `bracket_pair_colorization` toggle, round-tripped through real TOML
    /// save/load - same dedicated-test discipline as [`show_indent_guides_round_trips_through_real_toml_save_and_load`]
    /// above, and additionally pinning that a file written *before* this key existed still loads
    /// with the feature on. That backward-compatibility case is the one that matters here: this
    /// shipped enabled, so a missing key has to mean "on", never "off".
    #[test]
    fn bracket_pair_colorization_round_trips_and_defaults_on_for_an_older_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        assert!(
            Settings::default().appearance.bracket_pair_colorization,
            "sanity check: the real default is on - this is an opt-out, not an opt-in"
        );

        let mut settings = Settings::default();
        settings.appearance.bracket_pair_colorization = false;
        settings.save_at(&path).expect("save should succeed");
        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(settings, loaded);
        assert!(!loaded.appearance.bracket_pair_colorization);

        // A real settings file written before this key existed - an `[appearance]` table that
        // simply doesn't name it.
        let older = dir.path().join("older.toml");
        std::fs::write(
            &older,
            "[appearance]\ninterface_scale_percent = 110\neditor_font_size = 15.0\n",
        )
        .expect("write");
        let upgraded = Settings::load_or_init_at(&older);
        assert!(
            upgraded.appearance.bracket_pair_colorization,
            "a file predating this key must keep the feature on, not silently lose it"
        );
        assert_eq!(upgraded.appearance.interface_scale_percent, 110);
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
             terminal_font_size = -12.0\n\
             editor_zoom_percent = 9000\n",
        )
        .expect("write out-of-range file");

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(
            loaded.appearance.interface_scale_percent, INTERFACE_SCALE_PERCENT_MAX,
            "an absurdly large hand-edited percentage must be clamped, not loaded verbatim"
        );
        assert_eq!(loaded.appearance.editor_font_size, FONT_SIZE_MAX);
        assert_eq!(loaded.appearance.terminal_font_size, FONT_SIZE_MIN);
        assert_eq!(
            loaded.appearance.editor_zoom_percent, EDITOR_ZOOM_PERCENT_MAX,
            "an absurdly large hand-edited zoom percentage must be clamped too"
        );
    }

    /// The real regression guard for removing `per_tab_zoom`/`AdeApp::file_zoom_percent`: an old
    /// `settings.toml` written before this consolidation has a real `per_tab_zoom` key under
    /// `[appearance]` that no longer maps to any field. `serde`'s default (non-`deny_unknown_
    /// fields`) behavior is to ignore unrecognized keys rather than fail the whole parse, so this
    /// must still load cleanly and fall back to the new field's real default - not crash, and not
    /// silently produce a `Settings::default()` fallback (which would also discard every *other*
    /// real value the same file had set).
    #[test]
    fn an_old_settings_toml_with_the_removed_per_tab_zoom_key_still_loads_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[appearance]\n\
             interface_scale_percent = 110\n\
             editor_font_size = 15.0\n\
             terminal_font_size = 13.0\n\
             follow_system_text_size = false\n\
             per_tab_zoom = true\n",
        )
        .expect("write old-format file");

        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(loaded.appearance.interface_scale_percent, 110);
        assert_eq!(loaded.appearance.editor_font_size, 15.0);
        assert_eq!(
            loaded.appearance.editor_zoom_percent, EDITOR_ZOOM_PERCENT_DEFAULT,
            "a field genuinely absent from an old file must fall back to its real default"
        );
    }

    #[test]
    fn appearance_settings_sanitize_leaves_in_range_values_untouched() {
        let mut appearance = AppearanceSettings {
            interface_scale_percent: 110,
            editor_font_size: 15.0,
            terminal_font_size: 13.5,
            follow_system_text_size: true,
            editor_zoom_percent: 130,
            caret_style: CaretStyle::Block,
            caret_blink: false,
            show_indent_guides: false,
            bracket_pair_colorization: false,
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
