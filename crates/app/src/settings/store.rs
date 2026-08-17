//! Config-file-backed settings (`CHANGELOG.md`'s 2026-07-29 entry, change 3: "Settings —
//! narrower, config-file-first, five new pages"). Owns [`Settings`] - the struct loaded from,
//! and saved back to, `~/.config/jerry/settings.toml` - plus the TOML/JSON snippet rendering the
//! config banner/snippet-block widgets (`crate::settings::widgets`) show. Deliberately
//! separate from `crate::settings::state`, which stays about already-live in-memory app state rather
//! than a file on disk.

use std::collections::BTreeMap;
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
    pub terminal: TerminalSettings,
    pub icon_pack: IconPackSettings,
    pub lsp: BTreeMap<String, LspServerSettings>,
    pub sound: SoundSettings,
}

/// The integrated terminal's own behavioural settings (GitHub issue #213: "Allow to select
/// shell") - deliberately its own section rather than another key under [`AppearanceSettings`],
/// which is exclusively about text sizes and painted shapes (`terminal_font_size` lives there
/// because it *is* a font size). What program a shell tab launches is a spawn-time behaviour,
/// not an appearance.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// The program a plain Shell tab (`crate::work_surface::agents::ProcessKind::Shell`) spawns,
    /// or `None` for "whatever the OS says" - `$SHELL` on unix, `%COMSPEC%` on Windows, exactly
    /// the behaviour every install had before this setting existed (see
    /// `crate::terminal::pane::TerminalSpec::shell`). `None` is the zero-config default, so a
    /// user who never touches this keeps their real login shell.
    pub shell: Option<String>,
}

impl TerminalSettings {
    /// The configured shell as a real, usable program name, or `None` when the user hasn't
    /// chosen one. Whitespace-only is `None`, not a program named `" "`: the settings row's own
    /// field is a free-text input, and an accidental space must mean the same thing an empty
    /// field means (use the OS default), never a guaranteed-to-fail spawn.
    pub fn shell_override(&self) -> Option<&str> {
        self.shell
            .as_deref()
            .map(str::trim)
            .filter(|shell| !shell.is_empty())
    }

    /// Normalizes a hand-edited `shell = "  "` (or `shell = ""`) down to a real `None`, so the
    /// in-memory value a freshly loaded file produces is the same one the UI would have written.
    /// Same "a hand-edit gets normalized, not rejected" discipline as
    /// [`AppearanceSettings::sanitize`]/[`EditorSettings::sanitize`]; called from the same place
    /// (see [`Settings::load_or_init_at`]).
    pub fn sanitize(&mut self) {
        self.shell = self.shell_override().map(str::to_string);
    }
}

/// Per-language-server overrides, keyed by the server's own name as this app knows it -
/// `"typescript-language-server"`, `"pyright-langserver"`, `"rust-analyzer"`,
/// `"vue-language-server"`, `"gopls"`, or a companion's own distinct client key
/// (`"typescript-language-server (vue)"`). Hand-edited in `settings.toml`; there is no UI for it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspServerSettings {
    /// Deep-merged **over** whatever `crate::language`'s own registry builds for that server, so a
    /// user setting one key never discards the rest (Pyright's real `pythonPath` resolution, Vue's
    /// real `--tsdk` path). A value the user supplies wins at the leaf; every key they leave alone
    /// keeps the app's own. See `crate::language::merge_initialization_options`.
    pub initialization_options: Option<toml::Value>,
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
    /// GitHub issue #216 ("Scaling issues on Linux"): a forced display scale factor for GPUI's
    /// X11 backend, or `None` - the default - to leave GPUI's own detection alone.
    pub display_scale_override: Option<f32>,
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
    pub bracket_pair_colorization: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            interface_scale_percent: 100,
            display_scale_override: None,
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

/// Bounds, step and "just switched on" starting value for
/// [`AppearanceSettings::display_scale_override`]. A raw `f32` multiplier rather than a `u16`
/// percentage like [`INTERFACE_SCALE_PERCENT_MIN`]/[`INTERFACE_SCALE_PERCENT_MAX`], because the
/// value is handed to GPUI's `GPUI_X11_SCALE_FACTOR` verbatim and GPUI parses it as a bare
/// `f32`; keeping the stored form identical to the transmitted form means there is no
/// percentage-to-factor conversion to get wrong in either direction.
pub const DISPLAY_SCALE_OVERRIDE_MIN: f32 = 0.5;
pub const DISPLAY_SCALE_OVERRIDE_MAX: f32 = 4.0;
pub const DISPLAY_SCALE_OVERRIDE_STEP: f32 = 0.05;
pub const DISPLAY_SCALE_OVERRIDE_DEFAULT: f32 = 1.0;

/// The one real clamp for [`AppearanceSettings::display_scale_override`], shared by
/// [`AppearanceSettings::sanitize`] (hand-edited file), `crate::settings::render`'s stepper (UI
/// edit) and [`crate::x11_scale_factor_env_value`] (the value actually handed to GPUI), so those
/// three can never disagree about what is in range.
pub fn sanitize_display_scale_override(factor: f32) -> f32 {
    if factor.is_nan() {
        return DISPLAY_SCALE_OVERRIDE_DEFAULT;
    }
    factor.clamp(DISPLAY_SCALE_OVERRIDE_MIN, DISPLAY_SCALE_OVERRIDE_MAX)
}

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
        // `map`, not a blanket clamp: `None` is a real, distinct state ("leave GPUI's own
        // detection alone"), not a zero to be pulled up into range.
        self.display_scale_override = self
            .display_scale_override
            .map(sanitize_display_scale_override);
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

/// The sound design module's own settings (GitHub issue #226) - real, applied and read at
/// `crate::sound::flow::AdeApp::play_agent_status_sounds`/`crate::root::AdeApp::new_with_settings`
/// (the app-start chime), not persisted-only. `enabled` is the master switch every event's own
/// [`SoundEventSettings::enabled`] is gated behind - see [`Self::default`] for why it starts off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundSettings {
    /// Off by default (unlike every other field here): GitHub issue #226 asks for sound design
    /// that a user opts into, not one that plays the first time they launch a build that shipped
    /// it. Every [`SoundEventSettings::enabled`] below defaults to `true` specifically so turning
    /// this master switch on gives all three sounds immediately - the user then disables
    /// individual ones rather than having to opt every one of them in by hand.
    pub enabled: bool,
    /// `#[serde(default = "...")]`, not the blanket struct-level default: each event needs its
    /// *own* built-in sound (see [`SoundEventSettings::default_for`]), which a single shared
    /// `SoundEventSettings::default()` couldn't express - this is what makes a `settings.toml`
    /// with a `[sound]` table that simply omits `app_start` still resolve to the real default
    /// chime rather than an id-less, undecidable row.
    #[serde(default = "default_app_start_event")]
    pub app_start: SoundEventSettings,
    #[serde(default = "default_agent_finished_event")]
    pub agent_finished: SoundEventSettings,
    #[serde(default = "default_agent_needs_input_event")]
    pub agent_needs_input: SoundEventSettings,
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            app_start: default_app_start_event(),
            agent_finished: default_agent_finished_event(),
            agent_needs_input: default_agent_needs_input_event(),
        }
    }
}

fn default_app_start_event() -> SoundEventSettings {
    SoundEventSettings::default_for(crate::sound::SoundEventKind::AppStart)
}

fn default_agent_finished_event() -> SoundEventSettings {
    SoundEventSettings::default_for(crate::sound::SoundEventKind::AgentFinished)
}

fn default_agent_needs_input_event() -> SoundEventSettings {
    SoundEventSettings::default_for(crate::sound::SoundEventKind::AgentNeedsInput)
}

impl SoundSettings {
    /// Re-trims/re-defaults each event's own [`SoundEventSettings::sound`] - see that method's
    /// own docs. Deliberately does **not** check that the id still resolves to a real library
    /// sound (a user-imported sound the file for which has since been deleted, say): that check
    /// needs the live library, which this module - like every other settings section - has no
    /// access to and no business loading; `crate::sound::library::resolve`'s own documented
    /// fallback handles a since-deleted id without ever rewriting this file, so a restored file
    /// picks the assignment back up.
    pub fn sanitize(&mut self) {
        self.app_start
            .sanitize(crate::sound::SoundEventKind::AppStart);
        self.agent_finished
            .sanitize(crate::sound::SoundEventKind::AgentFinished);
        self.agent_needs_input
            .sanitize(crate::sound::SoundEventKind::AgentNeedsInput);
    }
}

/// One [`crate::sound::SoundEventKind`]'s own toggle and sound choice. `Default` derives to
/// `{ enabled: false, sound: "" }`, which is never the value actually used in practice - every
/// real default goes through [`Self::default_for`] instead (see [`SoundSettings::default`]'s and
/// its per-field `#[serde(default = "...")]`'s own docs for why a single shared default can't say
/// which built-in sound an event gets). The derive exists only because `#[serde(default)]` on
/// this struct's own fields needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundEventSettings {
    /// On by default - see [`SoundSettings::enabled`]'s own docs for why: the per-event toggle is
    /// how the user narrows down from "everything" after opting in, not how they opt in one at a
    /// time.
    pub enabled: bool,
    /// A [`crate::sound::library::LibrarySound`] id (`crate::sound::library::LibrarySound::id`) -
    /// a filename stem, never a display name. Resolved against the live library at play/preview
    /// time, not validated here (see [`SoundSettings::sanitize`]'s own docs).
    pub sound: String,
}

impl SoundEventSettings {
    fn default_for(event: crate::sound::SoundEventKind) -> Self {
        Self {
            enabled: true,
            sound: event.default_sound_id().to_string(),
        }
    }

    /// A hand-edited blank/whitespace-only `sound` is normalized back to the event's own default
    /// id rather than left as an empty string a dropdown would have nothing to show as selected -
    /// the same "hand-edit gets normalized, not rejected" discipline
    /// [`TerminalSettings::sanitize`] already documents for a blank `shell`.
    fn sanitize(&mut self, event: crate::sound::SoundEventKind) {
        let trimmed = self.sound.trim();
        self.sound = if trimmed.is_empty() {
            event.default_sound_id().to_string()
        } else {
            trimmed.to_string()
        };
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    pub minimap_enabled: bool,
    pub minimap_scale_percent: u16,
    pub insert_spaces: bool,
    pub tab_width: u8,
    /// Whether accepting a completion also applies the item's own `additionalTextEdits` - in
    /// practice, the `import`/`use` line a language server wants added for a symbol that isn't in
    /// scope yet. Read by `crate::lsp::completion_popup::AdeApp::accept_active_completion`.
    pub auto_import: bool,
    /// Whether language servers are asked to offer completions for symbols this file hasn't
    /// imported yet at all. On by default; off, the popup only ever suggests what is already in
    /// scope.
    pub suggest_auto_imports: bool,
    /// Whether the search panel's real, always-on explicit exclude list
    /// (`crate::search::exclude::DEFAULT_EXCLUDES` - `target`, `node_modules`, `.git`, and a
    /// handful of other common build/dependency directories) is ALSO layered with the worktree's
    /// real `.gitignore` (GitHub issue #394, which reworked #387/#388's own gitignore-only fix
    /// into this layered model after a direct "this should have nothing to do with git?"
    /// pushback - see `crate::search::exclude`'s own module docs for the full story).
    pub respect_gitignore: bool,
    /// Layer one's own real, persisted, user-editable pattern list (GitHub issue #401, a direct
    /// live follow-up to #394/#396: "The things you changed for the search are not configurable?
    /// They are not in settings or something?"). Read by
    /// `crate::search::engine::search_worktree_cancellable` via
    /// `crate::search::engine::SearchRequest::search_excludes`
    /// (`crate::search::render::AdeApp::start_search` populates it fresh from this field on every
    /// real search, same as [`Self::respect_gitignore`]), and edited from the Editor settings
    /// page's own Search section - one row per pattern with a remove affordance
    /// (`crate::settings::render::AdeApp::remove_search_exclude_pattern`), plus a real text input
    /// to add a new one (`crate::settings::render::AdeApp::add_search_exclude_pattern`).
    pub search_excludes: Vec<String>,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            minimap_enabled: true,
            minimap_scale_percent: MINIMAP_SCALE_PERCENT_DEFAULT,
            insert_spaces: true,
            tab_width: EDITOR_TAB_WIDTH_DEFAULT,
            auto_import: true,
            suggest_auto_imports: true,
            respect_gitignore: true,
            search_excludes: crate::search::exclude::default_search_excludes(),
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
        // A hand-edited `search_excludes` gets trimmed/deduped, not rejected - the same "hand-edit
        // gets normalized" discipline [`SoundEventSettings::sanitize`]'s own docs describe for a
        // blank `sound`. Blank entries (`""`, or whitespace-only after a stray trailing comma-style
        // hand-edit) are dropped outright rather than kept as a pattern
        // [`crate::search::glob::Glob::parse`] would silently treat as "no pattern" anyway - a
        // Settings-page row rendering an entirely blank line would be a real, visible bug, not a
        // faithful mirror of the file. Order is preserved and exact duplicates are collapsed to
        // their first occurrence, so a hand-edited `["target", "target"]` shows one row, not two
        // identical, independently-removable ones.
        let mut seen = std::collections::HashSet::new();
        self.search_excludes = std::mem::take(&mut self.search_excludes)
            .into_iter()
            .filter_map(|pattern| {
                let trimmed = pattern.trim().to_string();
                if trimmed.is_empty() || !seen.insert(trimmed.clone()) {
                    None
                } else {
                    Some(trimmed)
                }
            })
            .collect();
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

/// This user's home directory - `$HOME` on unix, `%USERPROFILE%` on Windows (which does not set
/// `HOME`), with no `dirs` crate dependency. An empty value counts as unset. Returns `None` when
/// neither is set, which callers treat as "no persistent config path".
pub fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Home-relative settings path resolution, via [`home_dir`]. Returns `None` if the home directory
/// isn't resolvable - callers fall back to an unpersisted, in-memory [`Settings::default`] rather
/// than panicking or guessing a path.
pub fn settings_toml_path() -> Option<PathBuf> {
    Some(
        home_dir()?
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
    /// ([`AppearanceSettings::sanitize`], [`EditorSettings::sanitize`],
    /// [`TerminalSettings::sanitize`]).
    pub fn load_or_init_at(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<Settings>(&contents) {
                Ok(mut settings) => {
                    settings.appearance.sanitize();
                    settings.editor.sanitize();
                    settings.terminal.sanitize();
                    settings.sound.sanitize();
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

    /// The production entry point - [`settings_toml_path`]'s home-resolved path, or an
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
/// pages `crate::settings::render` shows a config banner/snippet block on. Every other designed
/// page isn't backed by a [`Settings`] field, so a banner for it would describe a file section
/// that doesn't exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    General,
    Appearance,
    Theme,
    Editor,
    Notifications,
}

/// The config banner's dot-joined key list for `page` - narrowed from the design's own list to
/// only the [`Settings`] field paths this app actually persists (the design also names settings
/// this app doesn't implement, e.g. `window.restore_sessions`).
pub fn config_keys_line(page: ConfigPage) -> &'static str {
    match page {
        ConfigPage::General => "window.controls \u{b7} terminal.shell",
        ConfigPage::Appearance => {
            "appearance.interface_scale_percent \u{b7} appearance.display_scale_override \u{b7} \
             appearance.editor_font_size \u{b7} appearance.terminal_font_size \u{b7} \
             appearance.follow_system_text_size \u{b7} \
             appearance.editor_zoom_percent \u{b7} appearance.caret_style \u{b7} \
             appearance.caret_blink \u{b7} appearance.show_indent_guides \u{b7} \
             appearance.bracket_pair_colorization"
        }
        ConfigPage::Theme => {
            "theme.name \u{b7} theme.follow_system \u{b7} theme.high_contrast_diff"
        }
        ConfigPage::Editor => {
            "editor.minimap_enabled \u{b7} editor.minimap_scale_percent \u{b7} \
             editor.respect_gitignore \u{b7} editor.search_excludes"
        }
        ConfigPage::Notifications => {
            "sound.enabled \u{b7} sound.app_start.sound \u{b7} sound.agent_finished.sound \u{b7} \
             sound.agent_needs_input.sound"
        }
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

/// Both sections the General page really owns rows for - `[window]` (window controls) and
/// `[terminal]` (the shell override, GitHub issue #213). One doc rather than two so the snippet
/// block keeps showing the whole of what that page writes, in the same order the real file has
/// it. An unset `shell` renders as a bare `[terminal]` header with no key under it, which is
/// exactly what the real `settings.toml` on disk contains in that state - `toml` omits a `None`
/// value entirely rather than inventing a `shell = ""` the loader would then have to un-invent.
#[derive(Serialize)]
struct GeneralSnippetDoc<'a> {
    window: &'a WindowSettings,
    terminal: &'a TerminalSettings,
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

#[derive(Serialize)]
struct NotificationsSnippetDoc<'a> {
    sound: &'a SoundSettings,
}

/// Renders `page`'s slice of `settings` (the currently-loaded struct, never mockup fixture text)
/// as TOML or JSON via the same serializers [`Settings::save_at`]/[`Settings::to_json_string`]
/// use, so this can't drift from what the file (or its JSON preview) actually contains. Each
/// field-scoped `*SnippetDoc` wrapper exists only so a section header (`[window]`, etc.) appears
/// in the output - a bare struct at the TOML document root has no name to put in brackets.
pub fn snippet_lines(settings: &Settings, page: ConfigPage, format: CfgFormat) -> Vec<SnippetLine> {
    let text = match (page, format) {
        (ConfigPage::General, CfgFormat::Toml) => toml::to_string_pretty(&GeneralSnippetDoc {
            window: &settings.window,
            terminal: &settings.terminal,
        })
        .unwrap_or_default(),
        (ConfigPage::General, CfgFormat::Json) => {
            serde_json::to_string_pretty(&GeneralSnippetDoc {
                window: &settings.window,
                terminal: &settings.terminal,
            })
            .unwrap_or_default()
        }
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
        (ConfigPage::Notifications, CfgFormat::Toml) => {
            toml::to_string_pretty(&NotificationsSnippetDoc {
                sound: &settings.sound,
            })
            .unwrap_or_default()
        }
        (ConfigPage::Notifications, CfgFormat::Json) => {
            serde_json::to_string_pretty(&NotificationsSnippetDoc {
                sound: &settings.sound,
            })
            .unwrap_or_default()
        }
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
        assert!(
            settings.editor.respect_gitignore,
            "matches VS Code's own search.useIgnoreFiles default of true"
        );
        assert_eq!(
            settings.editor.search_excludes,
            crate::search::exclude::default_search_excludes(),
            "GitHub issue #401: a fresh install's real, editable list must start out identical to \
             crate::search::exclude::DEFAULT_EXCLUDES, not some independently-maintained copy"
        );
    }

    #[test]
    fn respect_gitignore_persists_through_a_real_save_and_load_in_both_states() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut settings = Settings::default();
        assert!(settings.editor.respect_gitignore, "the real default");
        settings.editor.respect_gitignore = false;
        settings.save_at(&path).expect("save");

        let reloaded = Settings::load_or_init_at(&path);
        assert!(
            !reloaded.editor.respect_gitignore,
            "a real save must round-trip an explicit `false`, not silently revert to the default"
        );

        let mut settings = reloaded;
        settings.editor.respect_gitignore = true;
        settings.save_at(&path).expect("save");
        assert!(Settings::load_or_init_at(&path).editor.respect_gitignore);
    }

    #[test]
    fn a_settings_file_missing_respect_gitignore_entirely_still_loads_the_real_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[editor]\ntab_width = 2\n").expect("write old file");

        let loaded = Settings::load_or_init_at(&path);
        assert_eq!(loaded.editor.tab_width, 2);
        assert!(
            loaded.editor.respect_gitignore,
            "a missing key must fall back to the real default, not an implicit false"
        );
        assert_eq!(
            loaded.editor.search_excludes,
            crate::search::exclude::default_search_excludes(),
            "a settings.toml written before GitHub issue #401 has no search_excludes key at all - \
             it must fall back to the real default list, not an empty (unfiltered) one"
        );
    }

    #[test]
    fn search_excludes_persists_through_a_real_save_and_load_with_a_custom_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut settings = Settings::default();
        assert_eq!(
            settings.editor.search_excludes,
            crate::search::exclude::default_search_excludes(),
            "the real default"
        );
        // The real shape of a Settings-page edit: the user added `coverage` and removed the
        // default `dist` entry.
        settings.editor.search_excludes.push("coverage".to_string());
        settings.editor.search_excludes.retain(|p| p != "dist");
        settings.save_at(&path).expect("save");

        let reloaded = Settings::load_or_init_at(&path);
        assert!(
            reloaded
                .editor
                .search_excludes
                .iter()
                .any(|p| p == "coverage"),
            "a real save must round-trip a user-added pattern: {:?}",
            reloaded.editor.search_excludes
        );
        assert!(
            !reloaded.editor.search_excludes.iter().any(|p| p == "dist"),
            "a real save must round-trip a user's removal of a default pattern too, not silently \
             restore it: {:?}",
            reloaded.editor.search_excludes
        );
        assert!(
            reloaded
                .editor
                .search_excludes
                .iter()
                .any(|p| p == "target"),
            "every other untouched default entry must still be there: {:?}",
            reloaded.editor.search_excludes
        );
    }

    #[test]
    fn a_hand_edited_search_excludes_with_blanks_and_duplicates_is_sanitized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[editor]\nsearch_excludes = [\"target\", \" \", \"target\", \"  coverage  \", \"\"]\n",
        )
        .expect("write hand-edited file");

        let loaded = Settings::load_or_init_at(&path);
        assert_eq!(
            loaded.editor.search_excludes,
            vec!["target".to_string(), "coverage".to_string()],
            "blank entries dropped, duplicates collapsed, real entries trimmed: {:?}",
            loaded.editor.search_excludes
        );
    }

    #[test]
    fn a_genuinely_empty_search_excludes_list_round_trips_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut settings = Settings::default();
        settings.editor.search_excludes.clear();
        settings.save_at(&path).expect("save");

        let reloaded = Settings::load_or_init_at(&path);
        assert!(
            reloaded.editor.search_excludes.is_empty(),
            "an explicit, deliberate empty list must round-trip as empty, not be re-seeded with \
             the default: {:?}",
            reloaded.editor.search_excludes
        );
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

    #[test]
    fn an_old_settings_toml_missing_the_keymap_section_entirely_still_loads_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[window]\ncontrols = \"system\"\n").expect("write old file");

        let loaded = Settings::load_or_init_at(&path);

        assert!(loaded.keymap.overrides.is_empty());
    }

    #[test]
    fn a_hand_written_lsp_preferences_section_round_trips_through_toml_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "[lsp.\"typescript-language-server\".initialization_options.preferences]\n\
             autoImportSpecifierExcludeRegexes = [\"^node:\"]\n\
             includePackageJsonAutoImports = \"off\"\n",
        )
        .expect("write a hand-edited settings.toml");

        let loaded = Settings::load_or_init_at(&path);
        let server = loaded
            .lsp
            .get("typescript-language-server")
            .expect("the hand-written section must load");
        let options = server
            .initialization_options
            .as_ref()
            .expect("its initialization options must load");

        let json = serde_json::to_value(options).expect("a real TOML value crosses to JSON");
        assert_eq!(
            json["preferences"]["autoImportSpecifierExcludeRegexes"],
            serde_json::json!(["^node:"])
        );
        assert_eq!(
            json["preferences"]["includePackageJsonAutoImports"],
            serde_json::json!("off")
        );

        loaded.save_at(&path).expect("save should succeed");
        assert_eq!(Settings::load_or_init_at(&path), loaded);
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

    /// Save then load has to be the identity on every field a user can change - a field that
    /// silently reverts to its default on the next launch is not a real setting. Asserted as a
    /// whole-value equality rather than field by field, so a newly added field is covered here
    /// the moment it is mutated below.
    #[test]
    fn a_settings_value_round_trips_through_real_toml_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        let mut settings = Settings::default();
        settings.window.controls = WindowControlsStyle::MacosStyle;
        settings.appearance.interface_scale_percent = 125;
        settings.appearance.editor_font_size = 15.5;
        assert!(
            settings.appearance.show_indent_guides,
            "premise: the real default is guides on, so `false` below is a real change"
        );
        settings.appearance.show_indent_guides = false;
        settings.theme.name = "Slate".to_string();
        settings.theme.high_contrast_diff = true;

        settings.save_at(&path).expect("save should succeed");
        let loaded = Settings::load_or_init_at(&path);

        assert_eq!(settings, loaded);
    }

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

    #[test]
    fn a_hand_edited_out_of_range_display_scale_override_is_clamped_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        for (written, expected) in [
            ("12.0", DISPLAY_SCALE_OVERRIDE_MAX),
            ("-3.0", DISPLAY_SCALE_OVERRIDE_MIN),
            ("0.0", DISPLAY_SCALE_OVERRIDE_MIN),
            ("nan", DISPLAY_SCALE_OVERRIDE_DEFAULT),
        ] {
            std::fs::write(
                &path,
                format!("[appearance]\ndisplay_scale_override = {written}\n"),
            )
            .expect("write out-of-range file");

            assert_eq!(
                Settings::load_or_init_at(&path)
                    .appearance
                    .display_scale_override,
                Some(expected),
                "a hand-edited `display_scale_override = {written}` must be clamped into range, \
                 not handed to GPUI verbatim"
            );
        }
    }

    #[test]
    fn the_display_scale_override_defaults_to_none_and_round_trips_through_a_real_file() {
        assert_eq!(
            Settings::default().appearance.display_scale_override,
            None,
            "auto-detection is the default; an override is opt-in"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        Settings::default().save_at(&path).expect("write defaults");
        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !written.contains("display_scale_override"),
            "an unset override must not appear in the file at all: {written}"
        );

        let mut configured = Settings::default();
        configured.appearance.display_scale_override = Some(1.25);
        configured.save_at(&path).expect("write configured");
        assert_eq!(
            Settings::load_or_init_at(&path)
                .appearance
                .display_scale_override,
            Some(1.25)
        );
    }

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
            display_scale_override: Some(1.25),
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
            return;
        };
        let home = home_dir().expect("settings_toml_path resolved, so home_dir must have too");
        assert_eq!(
            path,
            home.join(".config").join("jerry").join("settings.toml")
        );
    }

    #[test]
    fn home_dir_reads_the_variable_this_platform_actually_sets() {
        // Windows sets `%USERPROFILE%` and leaves `HOME` unset, which is exactly what made the
        // config file unopenable there; unix is the mirror image.
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let expected = std::env::var_os(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        assert_eq!(home_dir(), expected);
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
    fn the_shell_override_defaults_to_none_and_round_trips_through_a_real_file() {
        assert_eq!(
            Settings::default().terminal.shell,
            None,
            "an install that has never touched this setting must keep using the real OS default"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        // The default file the app writes on first run must still be a real, complete file -
        // `to_toml_string` swallows a serialization error into an empty string, so a `None`
        // that `toml` refused to serialize would silently blank the whole config.
        let defaults = Settings::default();
        defaults.save_at(&path).expect("write defaults");
        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(
            written.contains("[window]") && written.contains("[terminal]"),
            "the written file must really contain every section, got: {written}"
        );
        assert!(
            !written.contains("shell ="),
            "an unset shell must be omitted from the file entirely, not written as an empty \
             string: {written}"
        );
        assert_eq!(Settings::load_or_init_at(&path), defaults);

        let mut configured = Settings::default();
        configured.terminal.shell = Some("fish".to_string());
        configured.save_at(&path).expect("write configured");
        assert_eq!(
            Settings::load_or_init_at(&path).terminal.shell.as_deref(),
            Some("fish")
        );
    }

    #[test]
    fn a_hand_edited_blank_shell_loads_as_the_real_system_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");

        std::fs::write(&path, "[terminal]\nshell = \"  \"\n").expect("write");
        assert_eq!(
            Settings::load_or_init_at(&path).terminal.shell,
            None,
            "a whitespace-only shell must load as no override at all"
        );

        std::fs::write(&path, "[terminal]\nshell = \"  /usr/bin/fish \"\n").expect("write");
        assert_eq!(
            Settings::load_or_init_at(&path).terminal.shell.as_deref(),
            Some("/usr/bin/fish"),
            "surrounding whitespace is trimmed, the real program name is kept verbatim"
        );
    }

    #[test]
    fn the_general_snippet_shows_the_real_shell_override() {
        let mut settings = Settings::default();
        settings.terminal.shell = Some("pwsh".to_string());

        let lines = snippet_lines(&settings, ConfigPage::General, CfgFormat::Toml);
        let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(joined.contains(&"[terminal]"));
        assert!(
            joined.iter().any(|line| line.contains("\"pwsh\"")),
            "the snippet must show the real configured value, got {joined:?}"
        );
    }

    #[test]
    fn config_keys_line_only_names_real_persisted_fields() {
        assert_eq!(
            config_keys_line(ConfigPage::General),
            "window.controls \u{b7} terminal.shell"
        );
        assert!(
            config_keys_line(ConfigPage::Appearance).contains("appearance.interface_scale_percent")
        );
        assert!(config_keys_line(ConfigPage::Theme).contains("theme.name"));
        assert!(config_keys_line(ConfigPage::Notifications).contains("sound.enabled"));
    }

    #[test]
    fn sound_settings_default_is_off_with_every_event_pointing_at_a_distinct_builtin() {
        let sound = Settings::default().sound;
        assert!(
            !sound.enabled,
            "the sound module must be off by default (GitHub issue #226)"
        );
        assert!(sound.app_start.enabled);
        assert!(sound.agent_finished.enabled);
        assert!(sound.agent_needs_input.enabled);
        assert_eq!(sound.app_start.sound, "soft-chime");
        assert_eq!(sound.agent_finished.sound, "marimba-pop");
        assert_eq!(sound.agent_needs_input.sound, "gentle-ping");
    }

    #[test]
    fn sound_settings_round_trip_through_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let mut settings = Settings::default();
        settings.sound.enabled = true;
        settings.sound.agent_needs_input.sound = "warm-bell".to_string();
        settings.sound.agent_finished.enabled = false;
        settings.save_at(&path).expect("save");

        let loaded = Settings::load_or_init_at(&path);
        assert!(loaded.sound.enabled);
        assert_eq!(loaded.sound.agent_needs_input.sound, "warm-bell");
        assert!(!loaded.sound.agent_finished.enabled);
    }

    #[test]
    fn a_hand_edited_blank_sound_choice_is_sanitized_back_to_the_events_own_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[sound.app_start]\nsound = \"   \"\n").expect("write");
        let loaded = Settings::load_or_init_at(&path);
        assert_eq!(loaded.sound.app_start.sound, "soft-chime");
    }

    #[test]
    fn a_settings_file_with_no_sound_section_loads_the_real_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[editor]\ntab_width = 2\n").expect("write");
        let loaded = Settings::load_or_init_at(&path);
        assert_eq!(loaded.sound, SoundSettings::default());
    }
}
