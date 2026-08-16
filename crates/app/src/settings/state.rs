//! Pure data model for the Settings surface (`docs/design/settings.md`). Maps already-real app
//! state (agent binaries on `$PATH`, the worktree
//! list, the registered global keybindings) to what a settings row should show, with no `gpui`
//! dependency so it's directly unit-testable; `crate::root` turns the result into `gpui::Div`
//! trees. Config-file-backed values live in `crate::settings::store` instead - this module is
//! about live app state, not disk state.
//!
//! ## Which pages are real
//!
//! General, Agents, Worktrees, Appearance, Themes, Keybindings, Editor, Language servers, and
//! Notifications render real, live-derived content (see [`SettingsPage::is_implemented`]).
//! Integrations and About are honest nav-only placeholders that say so out loud rather than
//! faking content. Editor is a partial exception, not a full one: its one
//! real row is the minimap (`crate::code_surface::minimap`, GitHub issue #30's
//! `editor.minimap.enabled`) - indentation/soft-wrap/whitespace-display still have no real
//! backing anywhere in this codebase, so those stay left off the page entirely rather than
//! growing controls bound to nothing, the same "only what's real" discipline every other page
//! here already follows. Notifications (GitHub issue #226) is the newest of the real pages -
//! sound design, off by default, real settings-backed toggles and a real, importable sound
//! library - see `crate::sound`'s own module docs and `crate::settings::render`'s
//! `render_settings_notifications_page`.
//!
//! ## Why the Agents/Worktrees "Behaviour"/"Policy" toggle sections are left out
//!
//! The design's Agents and Worktrees pages show toggles like
//! "Plan before editing" or a "Worktree root" path field, but nothing in this app persists a
//! value per agent or per worktree (even `crate::settings::store::Settings` is a flat, global
//! struct with nowhere to hang a per-agent bool). Rendering them anyway would be a control bound
//! to nothing, so only the two sections backed by real, already-loaded state - the Installed
//! agents card and the Disk worktrees card - are built.

use std::path::PathBuf;

use crate::rail::state::WorktreeNote;
use crate::work_surface::agents::AgentKind;

/// Every Settings page, in the order the four nav groups present them (see [`nav_groups`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Agents,
    Worktrees,
    Appearance,
    Theme,
    Keymap,
    Editor,
    LanguageServers,
    Notifications,
    Integrations,
    About,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 11] = [
        SettingsPage::General,
        SettingsPage::Agents,
        SettingsPage::Worktrees,
        SettingsPage::Appearance,
        SettingsPage::Theme,
        SettingsPage::Keymap,
        SettingsPage::Editor,
        SettingsPage::LanguageServers,
        SettingsPage::Notifications,
        SettingsPage::Integrations,
        SettingsPage::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsPage::General => "General",
            SettingsPage::Agents => "Agents",
            SettingsPage::Worktrees => "Worktrees",
            SettingsPage::Appearance => "Appearance & scaling",
            SettingsPage::Theme => "Themes",
            SettingsPage::Keymap => "Keybindings",
            SettingsPage::Editor => "Editor",
            SettingsPage::LanguageServers => "Language servers",
            SettingsPage::Notifications => "Notifications",
            SettingsPage::Integrations => "Integrations",
            SettingsPage::About => "About",
        }
    }

    /// A stable, unique string used as the GPUI element id suffix for this page's nav row and
    /// content column, independent of its human-readable [`Self::label`].
    pub fn id(self) -> &'static str {
        match self {
            SettingsPage::General => "general",
            SettingsPage::Agents => "agents",
            SettingsPage::Worktrees => "worktrees",
            SettingsPage::Appearance => "appearance",
            SettingsPage::Theme => "theme",
            SettingsPage::Keymap => "keymap",
            SettingsPage::Editor => "editor",
            SettingsPage::LanguageServers => "lsp",
            SettingsPage::Notifications => "notifications",
            SettingsPage::Integrations => "integrations",
            SettingsPage::About => "about",
        }
    }

    /// Whether this page has real, live-state-backed content - see the module docs' "Which
    /// pages are real" section. Every other page is honestly nav-only.
    pub fn is_implemented(self) -> bool {
        matches!(
            self,
            SettingsPage::General
                | SettingsPage::Agents
                | SettingsPage::Worktrees
                | SettingsPage::Appearance
                | SettingsPage::Theme
                | SettingsPage::Keymap
                | SettingsPage::Editor
                | SettingsPage::LanguageServers
                | SettingsPage::Notifications
        )
    }

    /// The content column's one-line rationale under the page title - app-authored text, the
    /// design carrying no per-page subtitle. Every nav-only page shares the same placeholder
    /// text; the placeholder page *body* is separate - see
    /// `crate::settings::render::render_settings_placeholder_page`.
    pub fn subtitle(self) -> &'static str {
        match self {
            SettingsPage::General => {
                "Window chrome. Restore-on-launch, a default environment and a discard confirmation aren't wired to anything real yet, so they're left off this page rather than shown inert."
            }
            SettingsPage::Agents => {
                "Which agent binaries Jerry can actually find on PATH right now - detected live, not configured."
            }
            SettingsPage::Worktrees => {
                "Every agent gets its own worktree. This is where they live, their real disk usage, and what's safe to prune."
            }
            SettingsPage::Appearance => {
                "These sizes and scale are saved for real, but nothing in the interface renders at them yet."
            }
            SettingsPage::Theme => {
                "Dark-first. Picking a theme really re-skins the app - saved for real and applied live, no restart needed."
            }
            SettingsPage::Keymap => {
                "Every real, globally-bound shortcut this build actually dispatches - click a keycap to record a new one. The same commands bind to Ctrl and Alt on Windows and Linux."
            }
            SettingsPage::LanguageServers => {
                "One row per language server this app knows how to spawn, detected live on PATH - not configured."
            }
            SettingsPage::Editor => {
                "The minimap (right of the code column) is real: syntax-colored overview, a draggable viewport slider, git-change ticks - saved for real and applied live. Search-match overlays, indentation, soft-wrap and whitespace display aren't built yet, so they're left off this page rather than shown inert."
            }
            SettingsPage::Notifications => {
                "Off by default. Sound effects for app start and agent status changes, picked from a real, importable sound library - saved for real and applied live. Desktop/OS notifications and a dock badge aren't built yet, so they're left off this page rather than shown inert."
            }
            _ => "Not designed yet - this page has no real content in this build.",
        }
    }
}

/// One of the Settings nav's four grouped sections (`CHANGELOG.md`'s change 3: "Nav regrouped:
/// Workspace ... Interface ... Editor ... Other").
pub struct NavGroup {
    pub label: &'static str,
    pub pages: Vec<SettingsPage>,
}

/// The fixed nav structure - the design's own grouping and order,
/// unchanged. Every page is clickable navigation even though not every page renders real
/// content past that point (see [`SettingsPage::is_implemented`]).
pub fn nav_groups() -> Vec<NavGroup> {
    vec![
        NavGroup {
            label: "Workspace",
            pages: vec![
                SettingsPage::General,
                SettingsPage::Agents,
                SettingsPage::Worktrees,
            ],
        },
        NavGroup {
            label: "Interface",
            pages: vec![
                SettingsPage::Appearance,
                SettingsPage::Theme,
                SettingsPage::Keymap,
            ],
        },
        NavGroup {
            label: "Editor",
            pages: vec![SettingsPage::Editor, SettingsPage::LanguageServers],
        },
        NavGroup {
            label: "Other",
            pages: vec![
                SettingsPage::Notifications,
                SettingsPage::Integrations,
                SettingsPage::About,
            ],
        },
    ]
}

/// Every agent CLI this app can spawn. A shell isn't spawnable *as an agent* here at all, and
/// that's now structural rather than a documented exclusion: [`AgentKind`] has no `Shell`
/// variant, so nothing can put one in this array (a shell is a
/// `crate::work_surface::agents::ProcessKind::Shell`, a different type).
pub const AGENT_KINDS: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

/// One row for the Agents page's Installed card - see [`detect_agent_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub kind: AgentKind,
    /// The exact command name `AgentKind::binary_name` hands to `TerminalSpec::command`
    /// at spawn time - the same name [`Self::resolved_path`] was searched for.
    pub binary_name: &'static str,
    /// `Some(path)` if a real `$PATH` search found the binary, `None` if it genuinely isn't
    /// installed - never a guess. The search (`pty_core::resolve_on_path`) checks file
    /// permission bits, not a real `access(2)` call, so it can't account for ACLs or the
    /// calling process's specific uid/gid.
    pub resolved_path: Option<PathBuf>,
}

impl AgentRow {
    pub fn is_ready(self: &AgentRow) -> bool {
        self.resolved_path.is_some()
    }

    /// The status label next to the row's status dot - a green dot and `ready`, or the honest
    /// opposite when not found.
    pub fn status_label(&self) -> &'static str {
        if self.is_ready() {
            "ready"
        } else {
            "not found"
        }
    }
}

/// Builds one [`AgentRow`] per [`AGENT_KINDS`] entry via `resolve` (in production,
/// `pty_core::resolve_on_path` - see [`AgentRow::resolved_path`] for the one disclosed gap in
/// that search, which this is not an absolute guarantee against). Takes `resolve` as a parameter
/// so this is unit-testable with a fake resolver, independent of which binaries happen to be
/// installed on the machine running the test suite.
pub fn detect_agent_rows(resolve: impl Fn(&str) -> Option<PathBuf>) -> Vec<AgentRow> {
    AGENT_KINDS
        .into_iter()
        .map(|kind| {
            let binary_name = kind.binary_name();
            AgentRow {
                kind,
                binary_name,
                resolved_path: resolve(binary_name),
            }
        })
        .collect()
}

/// A worktree row's status dot state on the Worktrees page - derived from the same
/// [`WorktreeNote`] the rail already computes (`crate::rail::state::compute_status_snapshot`), never a
/// second, independent notion of worktree health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeDotStatus {
    /// The main checkout - never prunable, never "dirty" in the sense that matters here.
    Main,
    /// Clean and not (yet) merged - nothing to do.
    Clean,
    /// Uncommitted changes - matches [`WorktreeNote::clean`] being `Some(false)`.
    Dirty,
    /// A prune candidate on its own merits (see [`WorktreeNote::is_prunable`]) - not the final
    /// "safe to remove right now" answer, since a live agent could still exclude it (see
    /// `crate::rail::state::prunable_worktree_paths`); just this row's own local state.
    Prunable,
    /// `wt_core::is_dirty` failed for this path - genuinely unknown, not a guess.
    Unknown,
}

pub fn worktree_dot_status(is_main: bool, note: &WorktreeNote) -> WorktreeDotStatus {
    if is_main {
        return WorktreeDotStatus::Main;
    }
    if note.is_prunable() {
        return WorktreeDotStatus::Prunable;
    }
    match note.clean {
        Some(true) => WorktreeDotStatus::Clean,
        Some(false) => WorktreeDotStatus::Dirty,
        None => WorktreeDotStatus::Unknown,
    }
}

/// A worktree row's right-aligned action - `Open` or `Prune`, never both. The main checkout's
/// row has no action at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeRowAction {
    /// The main checkout - `git worktree remove` refuses it outright, and there's nowhere else
    /// to "open" it.
    None,
    Open,
    Prune,
}

pub fn worktree_row_action(is_main: bool, note: &WorktreeNote) -> WorktreeRowAction {
    if is_main {
        return WorktreeRowAction::None;
    }
    if note.is_prunable() {
        return WorktreeRowAction::Prune;
    }
    WorktreeRowAction::Open
}

/// One built-in theme, as loaded from its real `assets/themes/*.toml` file. `name`/`subtitle` are
/// carried directly (leaked into `&'static str`s - see [`THEME_DEFS`]) so this stays a plain
/// `Copy` handle every call site can pass around, and `theme` is the real, fully-parsed
/// `crate::settings::custom_theme::CustomTheme` behind it: its own explicit token overrides, its
/// `base`, and its card preview swatches. There is deliberately no second, built-in-only shape -
/// a bundled theme is exactly the same kind of value a user's own theme file produces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeDef {
    pub name: &'static str,
    pub subtitle: &'static str,
    pub theme: &'static crate::settings::custom_theme::CustomTheme,
}

/// The Themes page's six cards - real `assets/themes/*.toml` files (repo root, alongside
/// `assets/fonts/` - see `crate::fonts`' own module docs for the established "embed a real,
/// checked-in asset at compile time via `include_str!`/`include_bytes!`" convention this
/// mirrors), embedded into the binary and parsed through the exact same
/// `CustomThemeFile`/`validate` deserialization and validation path GitHub issue #5's
/// user-authored custom themes already use (`crate::settings::custom_theme::
/// parse_builtin_theme_file_str`) - not a second, parallel parser. Follow-up to GitHub issue #5:
/// before this, these six were a hardcoded `const` array of Rust struct literals; the *values*
/// are unchanged (transcribed verbatim - see `custom_theme::tests::
/// parse_builtin_theme_file_str_parses_every_embedded_built_in_theme_file_into_the_exact_documented_swatches`
/// and this module's own `theme_defs_match_the_documented_exact_names_subtitles_and_hex_swatches`
/// for the regression pins), only *where they live* changed.
///
/// A `std::sync::LazyLock`, not a `const`, since parsing TOML is real runtime work - computed
/// exactly once, on first access, and cached for the rest of the process; every existing call
/// site across this crate (`crate::theme`, `crate::root`, `crate::settings::render`,
/// `crate::settings::store`) already only ever indexes (`THEME_DEFS[i]`) or iterates
/// (`THEME_DEFS.iter()`) this array, both of which `LazyLock`'s `Deref` impl serves exactly like
/// the old `const` did, so none of them needed to change for this. `name`/`subtitle` are each
/// leaked once (`Box::leak`) into real `&'static str`s, so [`ThemeDef`] keeps its existing
/// `Copy`-friendly shape rather than growing owned `String` fields just for six values that live
/// for the process's entire lifetime anyway.
///
/// `crate::settings::store::ThemeSettings::name` - not this fixture's own `on` field, which this
/// app never reads - is the persisted source of truth for which one is selected.
pub static THEME_DEFS: std::sync::LazyLock<[ThemeDef; 6]> = std::sync::LazyLock::new(|| {
    const FILES: [&str; 6] = [
        include_str!("../../../../assets/themes/jerry-dark.toml"),
        include_str!("../../../../assets/themes/jerry-dim.toml"),
        include_str!("../../../../assets/themes/slate.toml"),
        include_str!("../../../../assets/themes/ember.toml"),
        include_str!("../../../../assets/themes/moss.toml"),
        include_str!("../../../../assets/themes/paper.toml"),
    ];
    FILES.map(|contents| {
        let theme = crate::settings::custom_theme::parse_builtin_theme_file_str(contents);
        ThemeDef {
            name: Box::leak(theme.name.clone().into_boxed_str()),
            subtitle: Box::leak(theme.subtitle.clone().into_boxed_str()),
            theme: Box::leak(Box::new(theme)),
        }
    })
});

/// One Language servers page row's static, per-language identity - the binary name
/// [`detect_lsp_rows`] searches `$PATH` for. Sourced from `crate::language`'s canonical registry
/// (Revision R8) rather than an independently-authored table - see
/// [`crate::language::ExtensionEntry::settings_row`]'s own docs for which entries produce a row
/// here and why. Binary names verified for real, not guessed: `typescript-language-server` (the
/// npm package's own binary name), `vue-language-server` (the modern Volar-based
/// `@vue/language-server` package - not the deprecated `vls`), `pyright-langserver` (`pyright`'s
/// LSP-mode entry point, distinct from the plain `pyright` type-checker CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspLanguage {
    pub language: &'static str,
    /// The file extension chip label - fed to `crate::sidebar::file_tree::lang_chip_for_name` (via a
    /// synthetic `"x.<ext>"` name) at the render call site, so this row's chip matches what a
    /// file-tree row for that language would show.
    pub ext: &'static str,
    pub binary: &'static str,
    /// Generic descriptive copy, not a live count. The design mixes this slot with live figures
    /// ("1,284 crates indexed") this app has no per-language agent summary to back
    /// (`crate::lsp::client`'s server clients are keyed by worktree, not surfaced here), so every
    /// note here is deliberately the descriptive kind only.
    pub note: &'static str,
    /// This server's real official install/docs page - see
    /// `crate::language::SettingsLspRow::install_url`'s own docs for how each was verified.
    pub install_url: &'static str,
}

/// The real, generic shape [`lsp_languages`] applies to whatever `entries` iterator it's given -
/// pulled out on its own (rather than inlined into [`lsp_languages`]) so a test can exercise it
/// directly against a synthetic entry list of any real length/shape, independent of
/// [`crate::language::EXTENSIONS`]'s own current, real size. `filter_map` (not a fixed-size
/// `std::array::from_fn`) means this genuinely cannot panic no matter how many entries `entries`
/// yields, or how many of them carry `settings_row: None` - the old bug class (a hardcoded
/// `LSP_LANGUAGES_COUNT` silently drifting from the real registry's actual size and panicking a
/// render path) has no equivalent assumption left to drift.
fn build_lsp_languages<'a>(
    entries: impl Iterator<Item = &'a crate::language::ExtensionEntry>,
) -> Vec<LspLanguage> {
    entries
        .filter_map(|entry| {
            let row = entry.settings_row?;
            Some(LspLanguage {
                language: entry.display_name,
                ext: entry.extension,
                binary: row.binary,
                note: row.note,
                install_url: row.install_url,
            })
        })
        .collect()
}

/// Builds one [`LspLanguage`] per real [`crate::language::settings_lsp_entries`] entry - a plain
/// `Vec`, not a fixed-size array, so a real language gaining/losing a `settings_row` (growing or
/// shrinking the real underlying registry) can never desync from a hardcoded count and panic in
/// a render path (`crate::settings::state::detect_lsp_rows`, reached on every real render of the Settings ->
/// Language Servers page). See [`build_lsp_languages`] for the actual, directly-testable shape.
pub fn lsp_languages() -> Vec<LspLanguage> {
    build_lsp_languages(crate::language::settings_lsp_entries())
}

/// One Language servers page row - see [`detect_lsp_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspRow {
    pub language: &'static str,
    pub ext: &'static str,
    pub binary: &'static str,
    pub note: &'static str,
    /// This server's real official install/docs page - see
    /// [`LspLanguage::install_url`]'s own docs. Carried straight through from [`lsp_languages`],
    /// not recomputed, so there is exactly one real source for it.
    pub install_url: &'static str,
    /// Same search contract as [`AgentRow::resolved_path`] - see that field's docs for the one
    /// disclosed gap.
    pub resolved_path: Option<PathBuf>,
}

impl LspRow {
    pub fn is_ready(&self) -> bool {
        self.resolved_path.is_some()
    }

    /// This page's own word for the not-found state - `"not installed"`, distinct from the
    /// Agents page's `"not found"`; each page keeps its own designed wording.
    pub fn status_label(&self) -> &'static str {
        if self.is_ready() {
            "ready"
        } else {
            "not installed"
        }
    }
}

/// Builds one [`LspRow`] per [`lsp_languages`] entry via `resolve`, mirroring
/// [`detect_agent_rows`] exactly (same `$PATH` search, same reason for taking `resolve` as a
/// parameter).
pub fn detect_lsp_rows(resolve: impl Fn(&str) -> Option<PathBuf>) -> Vec<LspRow> {
    lsp_languages()
        .into_iter()
        .map(|def| LspRow {
            language: def.language,
            ext: def.ext,
            binary: def.binary,
            note: def.note,
            install_url: def.install_url,
            resolved_path: resolve(def.binary),
        })
        .collect()
}

/// One row of the Keybindings settings page, built by [`keybinding_rows`] straight from
/// `crate::default_key_bindings`'s live-registered `gpui::KeyBinding`s - not a hand-maintained
/// parallel list, which had already drifted once (a hand-copied row claimed context `"editor"`
/// for a binding registered as global). `context`/`keystrokes` are always read straight off the
/// real `KeyBinding`, so a row can't describe a binding differently than what's actually
/// registered.
#[derive(Debug, Clone, PartialEq)]
pub struct KeybindingRow {
    pub command: &'static str,
    /// The binding's real, registered context predicate, via `gpui::KeyBinding::predicate`'s own
    /// `Display` impl (`vendor/zed/crates/gpui/src/keymap/context.rs`) - `"global"` only when
    /// there is genuinely no predicate at all. A real, live-reproduced bug an audit caught fixed
    /// this from an earlier version that collapsed every non-global predicate down to the single
    /// literal `"scoped"`: once more than one real scoped context existed (`"diff"`,
    /// `"file-editor"`, `"file-editor && completions"`, `"merge-editor"`, ...), two bindings that
    /// share a command label and keystroke but are scoped to *different*, mutually-exclusive
    /// contexts (e.g. `EditorCopy` bound once under `"file-editor"` and once under
    /// `"merge-editor"`) rendered as literally indistinguishable rows on the real Keybindings
    /// page, and the filter field couldn't tell them apart either (both matched the same
    /// `"scoped"` substring). Showing the real predicate string fixes both.
    pub context: String,
    /// The *effective* keystroke(s) for this row (the real override's, if one applies - else the
    /// real default's) - resolved to per-platform keycaps via `crate::keymap::resolve_keystroke`
    /// at the render call site. Always what's actually registered right now, matching this
    /// struct's own "never describe a binding differently than what's actually registered" rule.
    pub keystrokes: Vec<gpui::Keystroke>,
    /// This row's real, stable identity - see `crate::keymap_overrides::BindingIdentity`'s own
    /// docs. The render layer uses this (not `command`/`context` alone, which aren't always
    /// unique - see this struct's own docs) to know which override a "record new shortcut" or
    /// "reset" click on this row should read/write.
    pub identity: crate::keymap_overrides::BindingIdentity,
    /// Whether a real, persisted override currently applies to this row (i.e. [`Self::keystrokes`]
    /// differs from the compiled-in default) - drives the Keybindings page's "reset" affordance.
    pub is_overridden: bool,
}

/// Maps this app's globally-bound `gpui::Action` types to the Keybindings page's human command
/// label, keyed by each action's compiler-generated [`gpui::Action::name`]. Exists because no
/// existing label source covers all four: the palette's `PaletteCommand::label` covers three,
/// but `TogglePalette`/`GotoDefinition` have no `PaletteCommand` counterpart (the palette can't
/// open itself, and go-to-definition isn't a palette command).
///
/// The test `every_registered_global_keybinding_has_a_real_keybindings_page_label` (below) is
/// the drift guard: it asserts every binding in `crate::default_key_bindings()` resolves to
/// `Some` here, so a new global binding without a matching label fails a test rather than
/// silently rendering blank on the Keybindings page.
///
/// `pub(crate)`, not private - `crate::settings::render`'s real collision-message renderer
/// (`keymap_overrides::find_colliding_binding` returns a raw `gpui::KeyBinding`, not a
/// `KeybindingRow`) needs the same label this module's own rows already use, rather than showing
/// a raw `gpui::Action::name()` compiler identifier in a user-facing error.
pub(crate) fn action_label(action: &dyn gpui::Action) -> Option<&'static str> {
    match action.name() {
        "app::NewAgent" => Some("New agent"),
        "app::TogglePalette" => Some("Command palette"),
        "app::ToggleSettings" => Some("Open settings"),
        "app::GotoDefinition" => Some("Go to definition"),
        "app::NewTerminal" => Some("New terminal"),
        "app::NewAgentPane" => Some("New agent pane"),
        "app::NewGitGraph" => Some("Open git graph"),
        "app::SearchInWorktree" => Some("Search in this worktree"),
        "app::FindInFile" => Some("Find in this file"),
        "app::NextChangedFile" => Some("Next changed file"),
        "app::ToggleChangeSeen" => Some("Mark file seen / unseen"),
        "app::ToggleChangeStaged" => Some("Stage / unstage file"),
        "app::JumpToAgent1" => Some("Jump to agent 1"),
        "app::JumpToAgent2" => Some("Jump to agent 2"),
        "app::JumpToAgent3" => Some("Jump to agent 3"),
        "app::JumpToAgent4" => Some("Jump to agent 4"),
        "app::JumpToAgent5" => Some("Jump to agent 5"),
        "app::JumpToAgent6" => Some("Jump to agent 6"),
        "app::JumpToAgent7" => Some("Jump to agent 7"),
        "app::JumpToAgent8" => Some("Jump to agent 8"),
        "app::EditorBackspace" => Some("Editor: delete backward"),
        "app::EditorDelete" => Some("Editor: delete forward"),
        "app::EditorEnter" => Some("Editor: insert newline"),
        "app::EditorLeft" => Some("Editor: move left"),
        "app::EditorRight" => Some("Editor: move right"),
        "app::EditorUp" => Some("Editor: move up"),
        "app::EditorDown" => Some("Editor: move down"),
        "app::EditorSelectLeft" => Some("Editor: extend selection left"),
        "app::EditorSelectRight" => Some("Editor: extend selection right"),
        "app::EditorSelectUp" => Some("Editor: extend selection up"),
        "app::EditorSelectDown" => Some("Editor: extend selection down"),
        // GitHub issue #27's "Ctrl+Shift+arrows (word-wise)".
        "app::EditorWordLeft" => Some("Editor: move left one word"),
        "app::EditorWordRight" => Some("Editor: move right one word"),
        "app::EditorSelectWordLeft" => Some("Editor: extend selection left one word"),
        "app::EditorSelectWordRight" => Some("Editor: extend selection right one word"),
        "app::EditorHome" => Some("Editor: go to line start"),
        "app::EditorEnd" => Some("Editor: go to line end"),
        "app::EditorSelectAll" => Some("Editor: select all"),
        "app::EditorCopy" => Some("Editor: copy"),
        "app::EditorCut" => Some("Editor: cut"),
        "app::EditorPaste" => Some("Editor: paste"),
        "app::EditorSave" => Some("Editor: save file"),
        "app::EditorSaveAnyway" => Some("Editor: save file (overwrite external change)"),
        // Multi-cursor (Revision R13, issue #28).
        "app::EditorSelectNextOccurrence" => Some("Editor: select next occurrence"),
        "app::EditorSelectAllOccurrences" => Some("Editor: select all occurrences"),
        "app::EditorSkipOccurrence" => Some("Editor: skip occurrence"),
        "app::EditorCollapseCursors" => Some("Editor: collapse cursors"),
        "app::CompletionsUp" => Some("Completions: select previous"),
        "app::CompletionsDown" => Some("Completions: select next"),
        "app::CompletionsAccept" => Some("Completions: accept selected"),
        "app::CompletionsDismiss" => Some("Completions: dismiss"),
        "app::CompletionsInvoke" => Some("Completions: trigger"),
        "app::EditorIndent" => Some("Editor: indent"),
        "app::EditorDedent" => Some("Editor: dedent"),
        "app::EditorEscape" => Some("Editor: move focus out"),
        "app::TextUndo" => Some("Text: undo"),
        "app::TextRedo" => Some("Text: redo"),
        // GitHub issue #336's real clipboard/select-all for every single-line text input.
        "app::TextCopy" => Some("Text: copy selection"),
        "app::TextCut" => Some("Text: cut selection"),
        "app::TextPaste" => Some("Text: paste"),
        "app::TextSelectAll" => Some("Text: select all"),
        "app::CloseFocusedTab" => Some("Close focused tab"),
        "app::FileTreeRename" => Some("Files tree: rename"),
        "app::FileTreeCopy" => Some("Files tree: copy"),
        "app::FileTreeCut" => Some("Files tree: cut"),
        "app::FileTreePaste" => Some("Files tree: paste"),
        "app::FileTreeDelete" => Some("Files tree: delete"),
        "app::FileTreeUndo" => Some("Files tree: undo"),
        "app::FileTreeRedo" => Some("Files tree: redo"),
        "app::TerminalClear" => Some("Terminal: clear"),
        // GitHub issue #158.
        "app::TerminalCopy" => Some("Terminal: copy selection"),
        "app::TerminalPaste" => Some("Terminal: paste"),
        // GitHub issue #304 - the interactive-rebase plan's own keyboard verbs, which design
        // spec §1.4's footer band advertises as keycap hints. Rebindable like every other row
        // here; the labels name the plan surface so they group together in the page's filter.
        "app::RebaseReorderUp" => Some("Rebase plan: move row up"),
        "app::RebaseReorderDown" => Some("Rebase plan: move row down"),
        "app::RebasePickRow" => Some("Rebase plan: pick"),
        "app::RebaseSquashRow" => Some("Rebase plan: squash"),
        "app::RebaseDropRow" => Some("Rebase plan: drop"),
        "app::RebaseStart" => Some("Rebase plan: start rebase"),
        // GitHub issue #288 - the diff's own review-note verbs. `mod+enter` is drawn as real
        // keycaps on the notes bar, so it is rebindable here like everything else that is.
        "app::SendReviewNotes" => Some("Diff: send review notes to the agent"),
        "app::ToggleLineNote" => Some("Diff: note on this line"),
        _ => None,
    }
}

/// Builds the Keybindings page's rows straight from `default_bindings` (in production,
/// `crate::default_key_bindings()`) plus `overrides` (in production,
/// `Settings.keymap.overrides`) - row order is default registration order, so there is no
/// separate order to drift, and identity/context always come from the real *default* binding
/// (stable even while a row is overridden). `context` is `"global"` when the `KeyBinding` has no
/// context predicate, else the real predicate's own `Display` string (see
/// [`KeybindingRow::context`]'s own docs for why this - not a constant `"scoped"` - is
/// load-bearing). A binding whose action has no [`action_label`] entry is skipped rather than
/// shown with a blank label - see that function's docs for the test guarding against that.
pub fn keybinding_rows(
    default_bindings: &[gpui::KeyBinding],
    overrides: &[crate::settings::store::KeybindingOverride],
) -> Vec<KeybindingRow> {
    default_bindings
        .iter()
        .filter_map(|binding| {
            let command = action_label(binding.action())?;
            let identity = crate::keymap_overrides::BindingIdentity::of(binding);
            let context = identity.context.clone();
            let default_keystrokes: Vec<gpui::Keystroke> = binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().clone())
                .collect();
            let override_entry = overrides
                .iter()
                .find(|entry| identity.matches_override(entry));
            let (keystrokes, is_overridden) = match override_entry {
                // A malformed persisted override (only reachable via a hand-edited
                // `settings.toml`) falls back to the real default here too, matching
                // `keymap_overrides::effective_key_bindings`'s own fallback - the Keybindings
                // page must never show a keystroke that isn't actually registered.
                Some(entry) => match gpui::Keystroke::parse(&entry.keystrokes) {
                    Ok(keystroke) => (vec![keystroke], true),
                    Err(_) => (default_keystrokes, false),
                },
                None => (default_keystrokes, false),
            };
            Some(KeybindingRow {
                command,
                context,
                keystrokes,
                identity,
                is_overridden,
            })
        })
        .collect()
}

/// The Keybindings page's filter row logic (`CHANGELOG.md`'s change 3: "filter row (`/ filter N
/// bindings`, right-aligned count)") - a case-insensitive substring match against a row's
/// command name or context, matching `crate::rail::state::filter_agents`'s shape. An empty (or
/// all-whitespace) query matches every row.
pub fn filter_keybinding_rows<'a>(
    rows: &'a [KeybindingRow],
    query: &str,
) -> Vec<&'a KeybindingRow> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return rows.iter().collect();
    }
    rows.iter()
        .filter(|row| {
            row.command.to_lowercase().contains(&query)
                || row.context.to_lowercase().contains(&query)
        })
        .collect()
}

/// What the General page's "Shell" row can honestly say about the configured shell program
/// (GitHub issue #213) - see [`detect_shell_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellStatus {
    /// Nothing configured: a shell tab launches the real OS default (`$SHELL`/`%COMSPEC%`).
    SystemDefault,
    /// A real program was found - the resolved absolute path, exactly as it would be run.
    Resolved(PathBuf),
    /// Nothing by that name exists. Advisory only: this never stops the app from trying to spawn
    /// it (see `crate::terminal::pane::configured_shell_program`'s docs for why the real spawn
    /// stays the authority), it just makes a typo visible before a tab fails.
    NotFound,
}

impl ShellStatus {
    /// The trailing hint text the row shows next to the field - a real resolved path, an honest
    /// "not found", or the name of what will actually run when nothing is configured.
    pub fn hint(&self) -> String {
        match self {
            ShellStatus::SystemDefault => "empty - using the system default".to_string(),
            ShellStatus::Resolved(path) => path.display().to_string(),
            ShellStatus::NotFound => "not found - this tab will fail to start".to_string(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, ShellStatus::NotFound)
    }
}

/// Resolves the configured shell the same two ways `pty_core::spawn` itself will (GitHub issue
/// #213): a name with no path separator is searched for on `PATH` via `resolve`, anything that
/// looks like a path is checked as a real file on disk. Whitespace-only (or absent) is
/// [`ShellStatus::SystemDefault`], matching
/// `crate::settings::store::TerminalSettings::shell_override`.
///
/// Takes `resolve` as a parameter - in production `pty_core::resolve_on_path` - for the same
/// reason [`detect_agent_rows`] does: so this is unit-testable independently of which shells
/// happen to be installed on the machine running the suite. The on-disk check for a path-shaped
/// value is a real `Path::is_file` call rather than a second injected closure; a test can point
/// it at a real temp file, which is a truer check than a fake.
///
/// This is deliberately *advisory*. It cannot be a guarantee - `resolve_on_path` is a second
/// implementation of `portable-pty`'s own search (see its docs for the disclosed divergences) -
/// so it is used to inform the settings row, never to gate the spawn.
pub fn detect_shell_status(
    configured: Option<&str>,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> ShellStatus {
    let Some(program) = configured
        .map(str::trim)
        .filter(|program| !program.is_empty())
    else {
        return ShellStatus::SystemDefault;
    };

    let path = std::path::Path::new(program);
    if path.components().count() > 1 {
        // A path, not a name to search for - `CommandBuilder` runs it as given, so the only
        // real question is whether that file exists.
        return match path.is_file() {
            true => ShellStatus::Resolved(path.to_path_buf()),
            false => ShellStatus::NotFound,
        };
    }

    match resolve(program) {
        Some(resolved) => ShellStatus::Resolved(resolved),
        None => ShellStatus::NotFound,
    }
}

/// One genuinely-present shell offered under the General page's free-text "Shell" field (GitHub
/// issue #213's follow-up: "would a select + auto-detect installed shells be better?" - a hybrid,
/// so the common case needs no typing while the field itself stays unrestricted).
///
/// Every one of these was found by real I/O - a line of `/etc/shells` whose file is on disk right
/// now, or a real `$PATH`/`%PATH%` hit - never a hardcoded "shells people usually have". A name
/// that isn't genuinely resolvable is never offered, because clicking it would configure a shell
/// that cannot spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSuggestion {
    /// The program's own file name (`bash`, `pwsh.exe`) - what a user recognizes at a glance.
    pub name: String,
    /// The real, absolute path it was found at, which is also the value clicking the row types
    /// into the field ([`Self::value`]). Absolute rather than the bare name so the suggestion
    /// means exactly one program: `pty_core::spawn` runs an absolute path verbatim, with no
    /// second `PATH` search that could resolve to a different `bash` than the one listed here.
    pub path: PathBuf,
}

impl ShellSuggestion {
    /// The text this suggestion puts in the field when clicked - identical to what the user could
    /// have typed by hand, so it flows through the exact same
    /// `crate::settings::store::TerminalSettings::shell` path with nothing special about it.
    pub fn value(&self) -> String {
        self.path.display().to_string()
    }
}

/// The real, standard file listing a Unix system's valid login shells - one absolute path per
/// line, `#` comments and blank lines allowed. Read (never written) by [`detect_installed_shells`].
#[cfg(unix)]
pub const ETC_SHELLS: &str = "/etc/shells";

/// Well-known shells that are genuinely, routinely *absent* from `/etc/shells` even when
/// installed, so [`unix_shell_suggestions`] probes `PATH` for them as a supplement - not as the
/// primary mechanism, and never as an answer of its own: a name here is only ever offered when a
/// real `PATH` search actually finds it.
///
/// The list is short and evidence-driven rather than a general "common shells" guess. Registering
/// a shell in `/etc/shells` is the *distribution packager's* job, and each of these is
/// predominantly installed by a route that has no packager: fish's and PowerShell's own install
/// instructions tell the user to append the path to `/etc/shells` by hand afterwards (which is
/// only necessary because the install genuinely doesn't), Nushell ships mainly through
/// `cargo install` and release tarballs, Xonsh through `pip`, and Elvish through `go install` and
/// tarballs. None of those routes touch `/etc/shells` at all.
#[cfg(unix)]
const UNIX_SUPPLEMENTARY_SHELLS: [&str; 5] = ["fish", "nu", "pwsh", "elvish", "xonsh"];

/// Shell programs a Windows install may genuinely have on `%PATH%`, probed one by one - see
/// [`windows_shell_suggestions`], which only offers the ones a real search actually finds.
///
/// There is no `/etc/shells` equivalent on Windows, so this is a probe list rather than a source
/// of truth, and it is deliberately confined to names that are real, well-known program names -
/// not a catalogue of everything that might be a shell. `bash.exe` is a real example of why the
/// *probe* matters more than the name: on a given machine it could be WSL's bash shim, Git Bash,
/// or Cygwin's, and this makes no claim about which - only that the file the search found really
/// exists and would really run.
const WINDOWS_PROBED_SHELLS: [&str; 3] = ["powershell.exe", "pwsh.exe", "bash.exe"];

/// Every shell this machine genuinely has, for the field's suggestion list - the production
/// entry point, wired to the real `/etc/shells`, the real `%COMSPEC%`, and the real
/// `pty_core::resolve_on_path`.
///
/// Does real filesystem and `PATH` I/O, so like [`detect_agent_rows`] and [`detect_shell_status`]
/// it is called when Settings opens (`AdeApp::refresh_shell_suggestions`) and the result is held
/// in state - never from `render`, which would put a directory walk on the frame path.
pub fn detect_installed_shells() -> Vec<ShellSuggestion> {
    #[cfg(unix)]
    {
        unix_shell_suggestions(std::path::Path::new(ETC_SHELLS), pty_core::resolve_on_path)
    }
    #[cfg(windows)]
    {
        windows_shell_suggestions(std::env::var_os("COMSPEC"), pty_core::resolve_on_path)
    }
}

/// The Unix half of [`detect_installed_shells`]: parse `shells_file` (the real `/etc/shells`
/// format - one absolute path per line, `#` comments and blank lines skipped) and keep the
/// entries that are *actually a file on disk right now*. A listed shell can have been
/// uninstalled without the line being removed, and offering a path that isn't there would be
/// offering a shell that cannot spawn.
///
/// Then, and only then, [`UNIX_SUPPLEMENTARY_SHELLS`] are looked for on `PATH` via `resolve` and
/// appended if genuinely found - see that constant's docs for why a supplement is warranted and
/// why it is not the primary source.
///
/// `shells_file`/`resolve` are parameters, not constants read inside, for the same reason
/// [`detect_agent_rows`] takes its resolver: a test can point this at a real, hand-written
/// `/etc/shells` in a real tempdir and get a deterministic answer, instead of asserting on
/// whatever the machine running the suite happens to have installed.
#[cfg(unix)]
pub fn unix_shell_suggestions(
    shells_file: &std::path::Path,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<ShellSuggestion> {
    let mut found = Vec::new();
    let mut seen = Vec::new();

    if let Ok(contents) = std::fs::read_to_string(shells_file) {
        for line in contents.lines() {
            let entry = line.trim();
            if entry.is_empty() || entry.starts_with('#') {
                continue;
            }
            let path = PathBuf::from(entry);
            if path.is_file() {
                push_unique_shell(&mut found, &mut seen, path);
            }
        }
    }

    for name in UNIX_SUPPLEMENTARY_SHELLS {
        if let Some(path) = resolve(name) {
            push_unique_shell(&mut found, &mut seen, path);
        }
    }

    found
}

/// The Windows half of [`detect_installed_shells`]. Windows has no `/etc/shells`, so there are
/// exactly two real sources and this uses both: `%COMSPEC%` - already this app's own default-shell
/// mechanism (`crate::terminal::pane::TerminalSpec::default_shell_program`), so the first
/// suggestion is literally the program an empty field already runs - and a real `PATH` search for
/// each of [`WINDOWS_PROBED_SHELLS`].
///
/// Both sources are verified before anything is offered: `%COMSPEC%` must name a file that
/// genuinely exists, and a probed name must be genuinely found by `resolve`. A name that is
/// merely plausible on Windows is never listed.
///
/// Takes `comspec`/`resolve` as parameters rather than reading the environment itself so this is
/// exercisable as a real unit test (with a real temp file standing in for a real `%COMSPEC%`
/// target) on any host, including the Linux machines this project's suite runs on - the
/// alternative being a Windows code path with no test coverage whatsoever. It is compiled on
/// every platform for exactly that reason (it is only *called* under `#[cfg(windows)]`), so the
/// Linux suite really runs it rather than only type-checking it.
pub fn windows_shell_suggestions(
    comspec: Option<std::ffi::OsString>,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<ShellSuggestion> {
    let mut found = Vec::new();
    let mut seen = Vec::new();

    if let Some(comspec) = comspec {
        let path = PathBuf::from(comspec);
        if path.is_file() {
            push_unique_shell(&mut found, &mut seen, path);
        }
    }

    for name in WINDOWS_PROBED_SHELLS {
        if let Some(path) = resolve(name) {
            push_unique_shell(&mut found, &mut seen, path);
        }
    }

    found
}

/// Appends `path` as a suggestion unless an equivalent one is already listed, keyed on
/// `(file name, real canonicalized target)`.
///
/// Both halves of that key are load-bearing against real, live `/etc/shells` content. On any
/// usr-merged distribution the file lists `/bin/bash` *and* `/usr/bin/bash`, which are the same
/// binary reached through a symlinked directory - two rows that would offer a user the same
/// choice twice, so the canonical target dedupes them. But `/bin/sh` and `/bin/dash` also
/// canonicalize to the same target on Debian, and those are genuinely different choices a user
/// means differently (POSIX `sh` semantics vs. dash by name), so the file name keeps them apart.
///
/// Symlink resolution failure falls back to the path itself rather than dropping the entry: the
/// entry has already been proven to be a real file, and a failure to canonicalize is a reason to
/// keep it, not to hide it.
fn push_unique_shell(
    found: &mut Vec<ShellSuggestion>,
    seen: &mut Vec<(String, PathBuf)>,
    path: PathBuf,
) {
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return;
    };
    let target = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let key = (name.clone(), target);
    if seen.contains(&key) {
        return;
    }
    seen.push(key);
    found.push(ShellSuggestion { name, path });
}

/// The suggestions worth showing under a field currently holding `query` - a case-insensitive
/// substring match against both the shell's name and its full path, mirroring
/// [`filter_keybinding_rows`]'s own established "one lowercase substring test per searchable
/// field" shape rather than inventing a second matching rule.
///
/// An empty (or whitespace-only) field matches everything: with nothing typed yet, every real
/// shell on the machine is a legitimate suggestion. A query nothing matches yields nothing, which
/// is exactly what should happen when a user is typing a custom path the machine has never heard
/// of - the field stays entirely usable, the dropdown simply has nothing to add.
pub fn filter_shell_suggestions<'a>(
    suggestions: &'a [ShellSuggestion],
    query: &str,
) -> Vec<&'a ShellSuggestion> {
    let query = query.trim().to_lowercase();
    suggestions
        .iter()
        .filter(|suggestion| {
            query.is_empty()
                || suggestion.name.to_lowercase().contains(&query)
                || suggestion
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&query)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rail::state::WorktreeNote;
    use wt_core::diff::WorktreeMergeStatus;

    /// Writes a real, executable file and returns its path - the suggestion detector's entire
    /// contract is "only offer things that genuinely exist on disk", so its tests need real files,
    /// not paths that merely look plausible.
    #[cfg(unix)]
    fn write_real_program(dir: &std::path::Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// The heart of the Unix detection (GitHub issue #213's follow-up): a real `/etc/shells`-format
    /// file is parsed for real, and a line whose program is genuinely not on disk is never
    /// offered, because a listed shell can have been uninstalled and suggesting it would be
    /// suggesting a tab that cannot start.
    #[cfg(unix)]
    #[test]
    fn only_shells_that_really_exist_on_disk_are_suggested() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_bash = write_real_program(dir.path(), "bash");
        let real_zsh = write_real_program(dir.path(), "zsh");
        let uninstalled = dir.path().join("ksh");

        let shells_file = dir.path().join("shells");
        std::fs::write(
            &shells_file,
            format!(
                "# /etc/shells: valid login shells\n\
                 \n\
                 {}\n\
                 {}\n\
                    {}   \n\
                 #{}\n",
                real_bash.display(),
                uninstalled.display(),
                real_zsh.display(),
                real_bash.display(),
            ),
        )
        .expect("write");

        let found = unix_shell_suggestions(&shells_file, |_| None);
        let paths: Vec<PathBuf> = found.iter().map(|s| s.path.clone()).collect();

        assert_eq!(
            paths,
            vec![real_bash.clone(), real_zsh.clone()],
            "only the two lines naming files that genuinely exist may be offered, in file order, \
             with the blank line, the comment header, and the commented-out duplicate all skipped"
        );
        assert!(
            !paths.contains(&uninstalled),
            "a shell listed in /etc/shells but not actually installed must never be offered"
        );
        assert_eq!(
            found[0].name, "bash",
            "the row's label is the program's own real file name"
        );
        assert_eq!(
            found[1].value(),
            real_zsh.display().to_string(),
            "clicking a row types the real absolute path it was found at, nothing invented"
        );
    }

    /// A missing `/etc/shells` (a system that genuinely has none) is not an error and not a
    /// fabricated fallback list - it just means the supplementary `PATH` probe is all there is.
    #[cfg(unix)]
    #[test]
    fn a_missing_shells_file_yields_only_what_path_really_has() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_fish = write_real_program(dir.path(), "fish");
        let resolve = |program: &str| (program == "fish").then(|| real_fish.clone());

        let found = unix_shell_suggestions(&dir.path().join("no-such-shells-file"), resolve);

        assert_eq!(
            found,
            vec![ShellSuggestion {
                name: "fish".to_string(),
                path: real_fish
            }],
            "with no /etc/shells at all, the only honest answer is what a real PATH search found"
        );
    }

    /// The supplementary probe's whole reason to exist: fish/nu/pwsh & co. are routinely installed
    /// by routes that never register them in `/etc/shells`, so they must still be offered - and a
    /// probed name that `PATH` genuinely doesn't have must not be.
    #[cfg(unix)]
    #[test]
    fn a_shell_missing_from_etc_shells_is_still_found_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_bash = write_real_program(dir.path(), "bash");
        let real_nu = write_real_program(dir.path(), "nu");
        let shells_file = dir.path().join("shells");
        std::fs::write(&shells_file, format!("{}\n", real_bash.display())).expect("write");

        let found = unix_shell_suggestions(&shells_file, |program| {
            (program == "nu").then(|| real_nu.clone())
        });
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["bash", "nu"],
            "the /etc/shells entries come first, then the genuinely-resolvable supplements"
        );
        assert!(
            !names.contains(&"xonsh"),
            "a probed name PATH does not have must never be offered - the probe list is a list of \
             things to *look for*, never a list of things to claim"
        );
    }

    /// Real usr-merge duplication (`/bin/bash` and `/usr/bin/bash` are one binary on every modern
    /// distribution, and this machine's own `/etc/shells` lists both) collapses to one row, while
    /// two genuinely different *names* for the same binary - `sh` and `dash` on Debian - stay two
    /// rows, because a user choosing "sh" means something different by it.
    #[cfg(unix)]
    #[test]
    fn the_same_binary_listed_twice_is_offered_once_but_two_names_are_not_merged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let usr_bin = dir.path().join("usr").join("bin");
        std::fs::create_dir_all(&usr_bin).expect("mkdir");
        let real_dash = write_real_program(&usr_bin, "dash");
        // The real usr-merge shape: /bin is a symlink to /usr/bin, so /bin/dash and /usr/bin/dash
        // are the same file reached two ways.
        std::os::unix::fs::symlink("usr/bin", dir.path().join("bin")).expect("symlink");
        // And the real Debian shape for `sh`: a differently-named symlink to that same binary.
        std::os::unix::fs::symlink("dash", usr_bin.join("sh")).expect("symlink");

        let shells_file = dir.path().join("shells");
        std::fs::write(
            &shells_file,
            format!(
                "{}\n{}\n{}\n",
                dir.path().join("bin").join("dash").display(),
                real_dash.display(),
                usr_bin.join("sh").display(),
            ),
        )
        .expect("write");

        let found = unix_shell_suggestions(&shells_file, |_| None);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["dash", "sh"],
            "one row for the one dash binary listed under two directories, and a separate row for \
             the sh name that resolves to it"
        );
        assert_eq!(
            found[0].path,
            dir.path().join("bin").join("dash"),
            "the first-listed spelling is the one kept, not the canonicalized target"
        );
    }

    /// Windows has no `/etc/shells`, so the two real sources are `%COMSPEC%` (this app's own
    /// existing default-shell mechanism) and a real `PATH` search - both verified before anything
    /// is offered. Runs on every platform, deliberately: the alternative is a Windows-only code
    /// path this project's Linux-only suite could never execute.
    #[test]
    fn windows_offers_comspec_and_real_path_hits_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_cmd = dir.path().join("cmd.exe");
        std::fs::write(&real_cmd, "").expect("write");
        let real_pwsh = dir.path().join("pwsh.exe");
        std::fs::write(&real_pwsh, "").expect("write");
        let resolve = |program: &str| (program == "pwsh.exe").then(|| real_pwsh.clone());

        let found = windows_shell_suggestions(Some(real_cmd.clone().into_os_string()), resolve);

        assert_eq!(
            found,
            vec![
                ShellSuggestion {
                    name: "cmd.exe".to_string(),
                    path: real_cmd
                },
                ShellSuggestion {
                    name: "pwsh.exe".to_string(),
                    path: real_pwsh
                },
            ],
            "%COMSPEC% first (it is what an empty field already runs), then only the probed names \
             a real PATH search actually found - never powershell.exe or bash.exe on faith"
        );

        assert!(
            windows_shell_suggestions(Some(dir.path().join("gone.exe").into_os_string()), |_| None)
                .is_empty(),
            "a %COMSPEC% pointing at a file that isn't there must produce no suggestion at all"
        );
        assert!(
            windows_shell_suggestions(None, |_| None).is_empty(),
            "no %COMSPEC% and nothing on PATH is an honest empty list, not a guessed one"
        );
    }

    /// The filter is a convenience over real data, never a gate: it narrows on both the name and
    /// the path, and a query matching nothing yields nothing rather than falling back to
    /// everything (which would offer suggestions for a custom path the machine has never heard of).
    #[test]
    fn the_suggestion_filter_matches_name_and_path_case_insensitively() {
        let suggestions = vec![
            ShellSuggestion {
                name: "bash".to_string(),
                path: PathBuf::from("/bin/bash"),
            },
            ShellSuggestion {
                name: "fish".to_string(),
                path: PathBuf::from("/usr/local/bin/fish"),
            },
        ];
        let names = |query: &str| -> Vec<String> {
            filter_shell_suggestions(&suggestions, query)
                .into_iter()
                .map(|s| s.name.clone())
                .collect()
        };

        assert_eq!(names(""), vec!["bash", "fish"], "an empty field offers all");
        assert_eq!(names("   "), vec!["bash", "fish"]);
        assert_eq!(names("FI"), vec!["fish"], "matching is case-insensitive");
        assert_eq!(
            names("/usr/local"),
            vec!["fish"],
            "a partial path must match too - a user typing an absolute path is exactly who the \
             suggestions can still help"
        );
        assert!(
            names("/opt/my-own-shell").is_empty(),
            "a custom path nothing matches must produce an empty list, never the whole list back"
        );
    }

    /// The real production detector on the real machine running this suite: whatever it returns,
    /// every single entry must be a file that genuinely exists right now. This is the honesty
    /// claim the whole feature rests on, checked against reality rather than a fixture.
    #[test]
    fn every_really_detected_shell_is_a_file_that_really_exists() {
        for suggestion in detect_installed_shells() {
            assert!(
                suggestion.path.is_file(),
                "detection offered {}, which is not a real file - a suggestion that cannot spawn \
                 must never be shown",
                suggestion.path.display()
            );
            assert!(
                suggestion.path.is_absolute(),
                "a suggestion's value is used verbatim as the program to spawn, so it must be an \
                 absolute path: {}",
                suggestion.path.display()
            );
            assert_eq!(
                suggestion.value(),
                suggestion.path.display().to_string(),
                "the value typed into the field is the real detected path, nothing else"
            );
        }
    }

    /// This project's own CI hosts really do have `/etc/shells` with real shells in it, so the
    /// production detector must genuinely find some - a detector that always returned an empty
    /// list would pass every test above without doing anything at all.
    #[cfg(unix)]
    #[test]
    fn real_detection_finds_at_least_one_real_shell_on_this_machine() {
        if !std::path::Path::new(ETC_SHELLS).exists() {
            // Honest skip rather than a false pass: a Unix host genuinely without /etc/shells has
            // nothing for this assertion to be about. The sibling test above still holds there.
            return;
        }
        let found = detect_installed_shells();
        assert!(
            !found.is_empty(),
            "a machine with a real /etc/shells must yield real suggestions"
        );
        assert!(
            found.iter().any(|s| s.name == "sh" || s.name == "bash"),
            "every Unix host running this suite has a real sh or bash; got {found:?}"
        );
    }

    /// GitHub issue #213's advisory found/not-found hint: both real forms a user can type, plus
    /// the "nothing configured" case that must never be reported as an error.
    #[test]
    fn shell_status_reports_each_real_configured_form_honestly() {
        let fake_path_search =
            |program: &str| (program == "fish").then(|| PathBuf::from("/usr/bin/fish"));

        assert_eq!(
            detect_shell_status(None, fake_path_search),
            ShellStatus::SystemDefault,
            "no override at all is the normal, healthy state - never a 'not found'"
        );
        assert_eq!(
            detect_shell_status(Some("   "), fake_path_search),
            ShellStatus::SystemDefault,
            "a blank field means the system default, exactly like an absent value"
        );
        assert_eq!(
            detect_shell_status(Some("fish"), fake_path_search),
            ShellStatus::Resolved(PathBuf::from("/usr/bin/fish")),
            "a bare name must be reported as the real path PATH resolution finds"
        );
        assert_eq!(
            detect_shell_status(Some("fsih"), fake_path_search),
            ShellStatus::NotFound,
            "a typo'd name must be called out, not silently accepted"
        );
    }

    /// A path-shaped value is checked against the real filesystem, not searched for on `PATH` -
    /// exercised against a genuinely existing temp file and a genuinely missing one.
    #[test]
    fn a_path_shaped_shell_is_checked_on_disk_not_on_path() {
        let never_resolves = |_: &str| -> Option<PathBuf> {
            panic!("a path-shaped value must never be searched for on PATH")
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let real_file = dir.path().join("my-shell");
        std::fs::write(&real_file, "#!/bin/sh\n").expect("write");

        assert_eq!(
            detect_shell_status(Some(real_file.to_str().expect("utf-8")), never_resolves),
            ShellStatus::Resolved(real_file.clone())
        );
        assert_eq!(
            detect_shell_status(
                Some(dir.path().join("no-such-shell").to_str().expect("utf-8")),
                never_resolves
            ),
            ShellStatus::NotFound
        );
    }

    /// The real production resolver, on a real binary this test environment genuinely has -
    /// proof the injected-resolver tests above aren't only true of the fake.
    #[cfg(unix)]
    #[test]
    fn the_real_path_resolver_finds_a_real_shell() {
        let status = detect_shell_status(Some("sh"), pty_core::resolve_on_path);
        match status {
            ShellStatus::Resolved(path) => assert!(
                path.is_file(),
                "the reported path must be a real file on this machine, got {}",
                path.display()
            ),
            other => panic!("expected a real resolved sh, got {other:?}"),
        }
    }

    #[test]
    fn all_eleven_pages_are_covered_by_the_four_nav_groups_exactly_once() {
        let groups = nav_groups();
        let mut seen: Vec<SettingsPage> = groups
            .iter()
            .flat_map(|group| group.pages.iter().copied())
            .collect();
        assert_eq!(seen.len(), SettingsPage::ALL.len());
        for page in SettingsPage::ALL {
            assert!(
                seen.contains(&page),
                "{page:?} (label {:?}) is missing from nav_groups",
                page.label()
            );
        }
        seen.sort_by_key(|page| SettingsPage::ALL.iter().position(|p| p == page));
        let mut dedup = seen.clone();
        dedup.dedup();
        assert_eq!(
            seen.len(),
            dedup.len(),
            "a page appeared in more than one group"
        );
    }

    #[test]
    fn nav_groups_match_the_documented_2026_07_29_regroup() {
        let groups = nav_groups();
        let labels: Vec<&str> = groups.iter().map(|g| g.label).collect();
        assert_eq!(labels, vec!["Workspace", "Interface", "Editor", "Other"]);
        assert_eq!(
            groups[0].pages,
            vec![
                SettingsPage::General,
                SettingsPage::Agents,
                SettingsPage::Worktrees
            ]
        );
        assert_eq!(
            groups[1].pages,
            vec![
                SettingsPage::Appearance,
                SettingsPage::Theme,
                SettingsPage::Keymap
            ]
        );
        assert_eq!(
            groups[2].pages,
            vec![SettingsPage::Editor, SettingsPage::LanguageServers]
        );
        assert_eq!(
            groups[3].pages,
            vec![
                SettingsPage::Notifications,
                SettingsPage::Integrations,
                SettingsPage::About
            ]
        );
    }

    #[test]
    fn exactly_the_nine_documented_pages_are_implemented() {
        for page in SettingsPage::ALL {
            let expected = matches!(
                page,
                SettingsPage::General
                    | SettingsPage::Agents
                    | SettingsPage::Worktrees
                    | SettingsPage::Appearance
                    | SettingsPage::Theme
                    | SettingsPage::Keymap
                    | SettingsPage::Editor
                    | SettingsPage::LanguageServers
                    | SettingsPage::Notifications
            );
            assert_eq!(
                page.is_implemented(),
                expected,
                "{:?} implemented-ness should match the design's documented scope",
                page.label()
            );
        }
    }

    #[test]
    fn nav_only_pages_share_the_same_honest_placeholder_subtitle() {
        // `About`, not `Notifications`: GitHub issue #226 made Notifications a real, implemented
        // page, so it no longer shows the shared nav-only placeholder text - `About` is still
        // honestly nav-only and works as the reference for every other placeholder page.
        let placeholder = SettingsPage::About.subtitle();
        for page in SettingsPage::ALL {
            if page.is_implemented() {
                assert_ne!(
                    page.subtitle(),
                    placeholder,
                    "{:?} is implemented and must have its own real subtitle",
                    page.label()
                );
            } else {
                assert_eq!(
                    page.subtitle(),
                    placeholder,
                    "{:?} is nav-only and must show the same honest placeholder text",
                    page.label()
                );
            }
        }
    }

    #[test]
    fn every_page_id_is_unique() {
        let mut ids: Vec<&str> = SettingsPage::ALL.iter().map(|p| p.id()).collect();
        let original_len = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "duplicate SettingsPage::id()");
    }

    #[test]
    fn detect_agent_rows_reports_ready_for_a_resolver_that_finds_the_binary() {
        let rows = detect_agent_rows(|name| Some(PathBuf::from(format!("/usr/bin/{name}"))));
        assert_eq!(rows.len(), 2, "one row per AGENT_KINDS entry");
        assert!(rows.iter().all(|row| row.is_ready()));
        assert!(rows.iter().all(|row| row.status_label() == "ready"));
        let claude = rows
            .iter()
            .find(|row| row.kind == AgentKind::Claude)
            .expect("a Claude row should exist");
        assert_eq!(claude.binary_name, "claude");
        assert_eq!(claude.resolved_path, Some(PathBuf::from("/usr/bin/claude")));
    }

    #[test]
    fn detect_agent_rows_reports_not_found_for_a_resolver_that_finds_nothing() {
        let rows = detect_agent_rows(|_name| None);
        assert!(rows.iter().all(|row| !row.is_ready()));
        assert!(rows.iter().all(|row| row.status_label() == "not found"));
    }

    #[test]
    fn detect_agent_rows_can_report_mixed_real_and_not_found_status() {
        // Proves the two rows' statuses are independent, not all-or-nothing.
        let rows = detect_agent_rows(|name| {
            if name == "claude" {
                Some(PathBuf::from("/usr/bin/claude"))
            } else {
                None
            }
        });
        let claude = rows
            .iter()
            .find(|row| row.kind == AgentKind::Claude)
            .expect("claude row");
        let codex = rows
            .iter()
            .find(|row| row.kind == AgentKind::Codex)
            .expect("codex row");
        assert!(claude.is_ready());
        assert!(!codex.is_ready());
    }

    fn note(clean: Option<bool>, merged: bool, is_locked: bool) -> WorktreeNote {
        WorktreeNote {
            is_main: false,
            clean,
            merge: Some(WorktreeMergeStatus {
                base_branch: "main".to_string(),
                merged,
                head_committer_unix_seconds: Some(0),
            }),
            is_locked,
        }
    }

    #[test]
    fn worktree_dot_status_main_checkout_is_always_main_regardless_of_note() {
        let dirty_main = note(Some(false), false, false);
        assert_eq!(
            worktree_dot_status(true, &dirty_main),
            WorktreeDotStatus::Main
        );
    }

    #[test]
    fn worktree_dot_status_prunable_takes_priority_over_plain_clean() {
        let merged_clean = note(Some(true), true, false);
        assert_eq!(
            worktree_dot_status(false, &merged_clean),
            WorktreeDotStatus::Prunable
        );
    }

    #[test]
    fn worktree_dot_status_dirty_and_clean_and_unknown() {
        assert_eq!(
            worktree_dot_status(false, &note(Some(false), false, false)),
            WorktreeDotStatus::Dirty
        );
        assert_eq!(
            worktree_dot_status(false, &note(Some(true), false, false)),
            WorktreeDotStatus::Clean
        );
        let unknown = WorktreeNote {
            is_main: false,
            clean: None,
            merge: None,
            is_locked: false,
        };
        assert_eq!(
            worktree_dot_status(false, &unknown),
            WorktreeDotStatus::Unknown
        );
    }

    #[test]
    fn worktree_dot_status_locked_merged_clean_is_not_prunable_matching_worktree_note() {
        // Mirrors `crate::rail::state`'s own locked-worktree test - this function is a thin reduction
        // of `WorktreeNote::is_prunable`, not a second implementation of the same rule.
        let locked = note(Some(true), true, true);
        assert_eq!(
            worktree_dot_status(false, &locked),
            WorktreeDotStatus::Clean
        );
    }

    #[test]
    fn worktree_row_action_main_checkout_has_no_action() {
        let main_note = WorktreeNote {
            is_main: true,
            clean: Some(true),
            merge: None,
            is_locked: false,
        };
        assert_eq!(
            worktree_row_action(true, &main_note),
            WorktreeRowAction::None
        );
    }

    #[test]
    fn worktree_row_action_prunable_gets_prune_others_get_open() {
        let prunable = note(Some(true), true, false);
        assert_eq!(
            worktree_row_action(false, &prunable),
            WorktreeRowAction::Prune
        );

        let unmerged = note(Some(true), false, false);
        assert_eq!(
            worktree_row_action(false, &unmerged),
            WorktreeRowAction::Open
        );

        let dirty = note(Some(false), false, false);
        assert_eq!(worktree_row_action(false, &dirty), WorktreeRowAction::Open);
    }

    #[test]
    fn every_theme_def_name_is_unique_and_jerry_dark_is_the_default_named_theme() {
        let mut names: Vec<&str> = THEME_DEFS.iter().map(|t| t.name).collect();
        let original_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate theme name");
        assert_eq!(THEME_DEFS[0].name, "Jerry Dark");
    }

    /// The real regression guard for the built-in themes' own identity: `THEME_DEFS` is built at
    /// runtime from `assets/themes/*.toml`, so this pins the exact names, subtitles and card
    /// preview swatches those files must produce. The swatch values are the same five each theme
    /// was originally defined by, before the theme system's rewrite turned them into full literal
    /// palettes - each generated file carries them forward as its explicit `preview`, so the
    /// Themes page's cards look exactly as they always have. A single-digit typo in one of those
    /// files would silently change the app's real appearance, and this is what would catch it.
    #[test]
    fn theme_defs_match_the_documented_exact_names_subtitles_and_preview_swatches() {
        let expected: [(&str, &str, [u32; 5]); 6] = [
            (
                "Jerry Dark",
                "default",
                [0x0e0f11, 0x1a1e21, 0x5cb87f, 0xe2a336, 0x74ade8],
            ),
            (
                "Jerry Dim",
                "lower contrast",
                [0x15181b, 0x20252a, 0x6ab97f, 0xd8a94a, 0x7f9ad4],
            ),
            (
                "Slate",
                "cool greys",
                [0x0d1117, 0x161b22, 0x57a773, 0xc9a227, 0x6b9bd1],
            ),
            (
                "Ember",
                "warm",
                [0x12100e, 0x1e1a16, 0x8fae6b, 0xd98b3a, 0xc4713f],
            ),
            (
                "Moss",
                "green-tinted",
                [0x0f1310, 0x1a201b, 0x7fc79a, 0xc8b45a, 0x6f9bb5],
            ),
            (
                "Paper",
                "light \u{b7} beta",
                [0xf4f1ea, 0xe4e0d6, 0x3f7a52, 0xa8752a, 0x3d6c9c],
            ),
        ];
        assert_eq!(THEME_DEFS.len(), expected.len());
        for (def, (name, subtitle, swatches)) in THEME_DEFS.iter().zip(expected.iter()) {
            assert_eq!(def.name, *name, "name mismatch");
            assert_eq!(def.subtitle, *subtitle, "subtitle mismatch for {name}");
            assert_eq!(
                def.theme.preview_swatches(),
                *swatches,
                "preview swatch mismatch for {name}"
            );
        }
    }

    /// Jerry Dark is the real identity theme: its file names no colour overrides at all, because
    /// every `crate::theme::ColorToken`'s own compiled default *is* Jerry Dark. Every other
    /// built-in is a full, literal palette that inherits from it.
    #[test]
    fn jerry_dark_overrides_nothing_and_every_other_builtin_is_a_full_palette_based_on_it() {
        let jerry_dark = THEME_DEFS[0].theme;
        assert!(
            jerry_dark.overrides.is_empty(),
            "Jerry Dark must not override any token - it is the compiled default palette itself"
        );
        assert_eq!(jerry_dark.base, None);

        let token_count = crate::theme::all_tokens().count();
        for def in THEME_DEFS.iter().skip(1) {
            assert_eq!(
                def.theme.base.as_deref(),
                Some("Jerry Dark"),
                "{} should name Jerry Dark as its base",
                def.name
            );
            assert_eq!(
                def.theme.overrides.len(),
                token_count,
                "{} should be a complete generated palette naming every real token",
                def.name
            );
        }
    }

    /// A hand-edited `settings.toml` from before this refactor persists a built-in theme
    /// selection purely by its `name` (`crate::settings::store::ThemeSettings::name`'s own docs) -
    /// this proves that lookup still resolves for every one of the six real names after built-ins
    /// moved from a `const` array to `assets/themes/*.toml` files, so an existing user's settings
    /// file keeps loading and resolving exactly as it did before.
    #[test]
    fn every_documented_built_in_theme_name_still_resolves_by_name_lookup() {
        for name in ["Jerry Dark", "Jerry Dim", "Slate", "Ember", "Moss", "Paper"] {
            assert!(
                THEME_DEFS.iter().any(|def| def.name == name),
                "{name:?} must still resolve by name lookup"
            );
        }
    }

    /// [`lsp_languages`] now derives its length directly from
    /// `crate::language::settings_lsp_entries` (a real `Vec`, not a fixed-size array), so there is
    /// no separate count to drift - this just pins the real, current number of settings-row
    /// languages so a change to that registry is still visible in a test diff.
    #[test]
    fn settings_lsp_entries_count_matches_lsp_languages_len() {
        assert_eq!(
            crate::language::settings_lsp_entries().count(),
            lsp_languages().len()
        );
    }

    /// A real proof that [`build_lsp_languages`] (the shape [`lsp_languages`] itself is a thin
    /// wrapper over) cannot panic in a genuinely mismatched-count scenario - the exact bug class
    /// the old `std::array::from_fn` + a hardcoded `LSP_LANGUAGES_COUNT` `.expect()` was
    /// vulnerable to (a fixed-size array assuming an iterator yields exactly N items). This feeds
    /// a synthetic entry list whose length and `settings_row`-presence doesn't match
    /// [`crate::language::EXTENSIONS`]'s own real, current size at all (some `Some`, some `None`,
    /// a different count entirely) and confirms it just filters, never panics.
    #[test]
    fn lsp_languages_never_panics_on_a_mismatched_entry_count() {
        use crate::language::{ExtensionEntry, SettingsLspRow};

        fn entry(extension: &'static str, has_row: bool) -> ExtensionEntry {
            ExtensionEntry {
                extension,
                display_name: "Synthetic",
                lsp_language_id: "synthetic",
                chip_label: "sy",
                chip_colors: crate::theme::lang::UNKNOWN,
                lsp: None,
                settings_row: has_row.then_some(SettingsLspRow {
                    binary: "synthetic-binary",
                    note: "synthetic",
                    install_url: "https://example.invalid/synthetic",
                }),
                highlighter: None,
            }
        }

        // Deliberately a different real count (7) than `EXTENSIONS`' own real current size, and a
        // deliberate mix of `Some`/`None` settings_row - exactly the shape that would have
        // panicked the old `std::array::from_fn`-against-a-stale-constant version.
        let synthetic = [
            entry("a", true),
            entry("b", false),
            entry("c", true),
            entry("d", false),
            entry("e", false),
            entry("f", true),
            entry("g", true),
        ];

        let languages = build_lsp_languages(synthetic.iter());
        // No panic reaching this line is the real assertion; this also confirms only the real
        // `Some(settings_row)` entries (4 of the 7) survived the filter.
        assert_eq!(languages.len(), 4);
        assert!(languages
            .iter()
            .all(|language| language.binary == "synthetic-binary"));
    }

    #[test]
    fn detect_lsp_rows_reports_ready_for_a_resolver_that_finds_every_binary() {
        let rows = detect_lsp_rows(|name| Some(PathBuf::from(format!("/usr/bin/{name}"))));
        assert_eq!(rows.len(), lsp_languages().len());
        assert!(rows.iter().all(|row| row.is_ready()));
        assert!(rows.iter().all(|row| row.status_label() == "ready"));
    }

    #[test]
    fn detect_lsp_rows_reports_not_installed_for_a_resolver_that_finds_nothing() {
        let rows = detect_lsp_rows(|_| None);
        assert!(rows.iter().all(|row| !row.is_ready()));
        assert!(rows.iter().all(|row| row.status_label() == "not installed"));
    }

    /// Proves [`detect_lsp_rows`] carries the real install URL straight through from
    /// `crate::language`'s registry - not a second, independently-authored copy that could drift.
    #[test]
    fn detect_lsp_rows_carries_the_real_install_url_from_the_language_registry() {
        let rows = detect_lsp_rows(|_| None);
        for row in &rows {
            assert!(
                row.install_url.starts_with("https://"),
                "{}'s install_url should be a real https:// URL, got {:?}",
                row.binary,
                row.install_url
            );
        }
        let rust = rows
            .iter()
            .find(|row| row.language == "Rust")
            .expect("a Rust row should exist");
        assert_eq!(
            rust.install_url,
            "https://rust-analyzer.github.io/book/rust_analyzer_binary.html"
        );
    }

    #[test]
    fn detect_lsp_rows_can_report_mixed_real_and_not_installed_status() {
        let rows = detect_lsp_rows(|name| {
            if name == "rust-analyzer" {
                Some(PathBuf::from("/usr/bin/rust-analyzer"))
            } else {
                None
            }
        });
        let rust = rows
            .iter()
            .find(|row| row.language == "Rust")
            .expect("a Rust row should exist");
        let go = rows
            .iter()
            .find(|row| row.language == "Go")
            .expect("a Go row should exist");
        assert!(rust.is_ready());
        assert!(!go.is_ready());
    }

    #[test]
    fn every_registered_global_keybinding_has_a_real_keybindings_page_label() {
        // The drift guard `action_label`'s docs describe.
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings, &[]);
        assert_eq!(
            rows.len(),
            bindings.len(),
            "every real, globally-bound KeyBinding must resolve to a real Keybindings-page row \
             (missing: {:?})",
            bindings
                .iter()
                .filter(|binding| action_label(binding.action()).is_none())
                .map(|binding| binding.action().name())
                .collect::<Vec<_>>()
        );
    }

    /// The real drift guard for the exact bug an audit caught: `KeybindingRow::context` used to
    /// collapse every scoped predicate down to the single literal `"scoped"`, so two bindings
    /// that share a command label and keystroke but are scoped to different, mutually-exclusive
    /// contexts (real, live example: `EditorCopy` bound once under `"file-editor"` and once
    /// under `"merge-editor"`) rendered as literally indistinguishable rows. Asserts every real
    /// `(command, context, keystrokes)` triple is unique - this is the actual, real property the
    /// Keybindings page needs (never two rows a user genuinely cannot tell apart), not just a
    /// row *count*, which the collision this guards against would not have changed at all.
    #[test]
    fn every_keybinding_row_is_genuinely_distinguishable_from_every_other() {
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings, &[]);
        let mut seen: Vec<(&str, &str, Vec<gpui::Keystroke>)> = Vec::new();
        for row in &rows {
            let identity = (row.command, row.context.as_str(), row.keystrokes.clone());
            assert!(
                !seen.contains(&identity),
                "two Keybindings-page rows are genuinely indistinguishable - command {:?}, \
                 context {:?}, keystrokes {:?} - a real user has no way to tell them apart: \
                 {rows:#?}",
                identity.0,
                identity.1,
                identity.2
            );
            seen.push(identity);
        }
    }

    #[test]
    fn keybinding_rows_are_derived_in_real_registration_order() {
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings, &[]);
        let commands: Vec<&str> = rows.iter().map(|row| row.command).collect();
        assert_eq!(
            commands,
            vec![
                "New agent",
                "Command palette",
                "Open settings",
                "Text: undo",
                "Text: redo",
                "Text: redo",
                "Text: copy selection",
                "Text: cut selection",
                "Text: paste",
                "Text: select all",
                "Go to definition",
                "New terminal",
                "New agent pane",
                "Open git graph",
                "Search in this worktree",
                "Find in this file",
                "Next changed file",
                "Mark file seen / unseen",
                "Stage / unstage file",
                "Jump to agent 1",
                "Jump to agent 2",
                "Jump to agent 3",
                "Jump to agent 4",
                "Jump to agent 5",
                "Jump to agent 6",
                "Jump to agent 7",
                "Jump to agent 8",
                "Editor: delete backward",
                "Editor: delete forward",
                "Editor: insert newline",
                "Editor: move left",
                "Editor: move right",
                "Editor: move up",
                "Editor: move down",
                "Editor: extend selection left",
                "Editor: extend selection right",
                "Editor: extend selection up",
                "Editor: extend selection down",
                // GitHub issue #27's "Ctrl+Shift+arrows (word-wise)".
                "Editor: move left one word",
                "Editor: move right one word",
                "Editor: extend selection left one word",
                "Editor: extend selection right one word",
                "Editor: go to line start",
                "Editor: go to line end",
                "Editor: select all",
                "Editor: copy",
                "Editor: cut",
                "Editor: paste",
                "Editor: save file",
                "Editor: save file (overwrite external change)",
                // Multi-cursor (Revision R13, issue #28): Ctrl+D/Ctrl+Shift+L/Ctrl+K Ctrl+D.
                "Editor: select next occurrence",
                "Editor: select all occurrences",
                "Editor: skip occurrence",
                // GitHub issue #26's real Tab/Shift+Tab indentation, scoped
                // `"file-editor && !completions"` - see `crate::default_key_bindings`'s own docs
                // for why `Tab` no longer falls through to plain-text insertion here.
                "Editor: indent",
                "Editor: dedent",
                // `Escape` here is `EditorCollapseCursors`, not a separate `EditorEscape` - it
                // composes GitHub issue #28's own multi-cursor collapse with issue #26's
                // accessibility focus-out hatch, since only one binding can genuinely own the
                // File view's plain `Escape` at equal context depth (see `crate::code_surface::
                // editing::AdeApp::handle_editor_collapse_cursors_action`'s own docs).
                "Editor: collapse cursors",
                "Completions: select previous",
                "Completions: select next",
                "Completions: accept selected",
                "Completions: accept selected",
                "Completions: dismiss",
                // GitHub issue #26's manual completion trigger/refresh, scoped plain
                // `"file-editor"` (fires whether the popup is open or closed).
                "Completions: trigger",
                // Revision R8.5c's `"merge-editor"`-scoped bindings for Surface D's merge
                // hand-edit whole-file editor - the same real `Editor*` action *types*/labels as
                // the `"file-editor"` set above (reused, not duplicated - see
                // `crate::code_surface::editing::AdeApp::active_edit_target`'s own docs), minus
                // `EditorSaveAnyway` (deliberately never bound here - there is no
                // external-change-conflict concept for a merge hand-edit buffer) and minus every
                // `Completions*` binding (no completions popup is ever wired up for this
                // surface).
                "Editor: delete backward",
                "Editor: delete forward",
                "Editor: insert newline",
                "Editor: move left",
                "Editor: move right",
                "Editor: move up",
                "Editor: move down",
                "Editor: extend selection left",
                "Editor: extend selection right",
                "Editor: extend selection up",
                "Editor: extend selection down",
                "Editor: move left one word",
                "Editor: move right one word",
                "Editor: extend selection left one word",
                "Editor: extend selection right one word",
                "Editor: go to line start",
                "Editor: go to line end",
                "Editor: select all",
                "Editor: copy",
                "Editor: cut",
                "Editor: paste",
                "Editor: save file",
                // The same GitHub issue #26 indent/dedent/escape bindings, mirrored here for the
                // merge hand-edit target - scoped plain `"merge-editor"` (no completions popup
                // exists for this surface, so there's no `!completions` narrowing to mirror).
                "Editor: indent",
                "Editor: dedent",
                "Editor: move focus out",
                // GitHub issue #19's file-tree bindings, each scoped to
                // `"file-tree && !tree-editing"` - see `crate::default_key_bindings`' own docs
                // and `crate::sidebar::tree_ops`' module docs for why both halves of that
                // predicate are load-bearing.
                "Files tree: rename",
                "Files tree: copy",
                "Files tree: cut",
                "Files tree: paste",
                // GitHub issue #105's `Delete` (runs immediately, no confirmation) and its own
                // real undo/redo, `Ctrl+Z`/`Ctrl+Shift+Z` - distinct actions from `TextUndo`/
                // `TextRedo`, scoped the same `"file-tree && !tree-editing"` as every binding
                // above.
                "Files tree: delete",
                "Files tree: undo",
                "Files tree: redo",
                // GitHub issue #26's `Ctrl+W` - closes the focused tab, scoped `Some("!terminal")`.
                "Close focused tab",
                // GitHub issue #20's terminal footer `clear`, scoped `Some("terminal")`.
                "Terminal: clear",
                // GitHub issue #158's terminal copy/paste, both scoped `Some("terminal")`.
                "Terminal: copy selection",
                "Terminal: paste",
                // GitHub issue #304's interactive-rebase plan verbs, scoped
                // `Some("rebase-plan && !text-input")` (and plain `Some("rebase-plan")` for the
                // last) - the real bindings behind design spec §1.4's footer keycap hints.
                "Rebase plan: move row up",
                "Rebase plan: move row down",
                "Rebase plan: pick",
                "Rebase plan: squash",
                "Rebase plan: drop",
                "Rebase plan: start rebase",
                "Diff: send review notes to the agent",
                "Diff: note on this line",
            ]
        );
    }

    #[test]
    fn keybinding_rows_report_the_real_global_context_for_every_default_binding() {
        // Regression coverage for the bug this replaced: a hand-copied list once labeled
        // `Go to definition` `context: "editor"` even though it's actually registered global.
        //
        // Revision R8.5a added a second real scoped context (`"file-editor"`, the real File
        // view text-editing actions) alongside the pre-existing `"diff"` one (`]` ->
        // `NextChangedFile`) - both are deliberately non-global for the same real reason
        // (swallowing ordinary keystrokes in a focused terminal agent), so the scoped count
        // grew from 1 to 1 + 19 real `Editor*` bindings (a fix round added `EditorSaveAnyway`,
        // the real escape hatch for a permanently-stuck `AdeApp::file_external_conflict`). A
        // later fix round changed `]`'s own registered predicate from `Some("diff")` to
        // `Some("diff && !file-editor")` (a real, live-reproduced bug: since `"file-editor"` is
        // *added onto* the same node's context rather than replacing `"diff"`, the bare
        // `"diff"` predicate kept matching - and kept swallowing a literal `]` keystroke - even
        // while a file was actively being edited) - `KeybindingRow::context` only ever reports
        // the coarse `"global"`/`"scoped"` distinction (see its own docs), so that predicate
        // change doesn't move this test's own counts, but `]` is still exactly as "scoped" as
        // before. Revision R8.5b added 5 more real scoped bindings for the Completions popup
        // (`CompletionsUp`/`CompletionsDown`/`CompletionsDismiss`, plus `CompletionsAccept`
        // bound twice - `tab` and `enter`), each `Some("file-editor && completions")`.
        // Revision R8.5c added 18 more real scoped bindings for Surface D's merge hand-edit
        // whole-file editor, each `Some("merge-editor")` - the same 18 `Editor*` actions as the
        // `"file-editor"` set minus `EditorSaveAnyway` (see
        // `keybinding_rows_are_derived_in_real_registration_order`'s own updated expectations
        // for exactly which).
        // GitHub issue #17 added 3 more real scoped bindings, `TextUndo` (`secondary-z`) and
        // `TextRedo` (bound twice - `secondary-shift-z` and `ctrl-y`), each `Some("text-input")`:
        // `secondary-z` resolves to plain `Ctrl+Z` on Linux/Windows, which a focused terminal
        // needs unclaimed to send the real `SIGTSTP` suspend control byte
        // (`crate::terminal::pane::keystroke_to_bytes`) - see `crate::default_key_bindings`'s own
        // docs for the full reasoning.
        // GitHub issue #27 added 8 more real scoped bindings: `EditorWordLeft`/`EditorWordRight`/
        // `EditorSelectWordLeft`/`EditorSelectWordRight`, each bound once under `"file-editor"`
        // and once under `"merge-editor"` - the same word-wise-caret-navigation set both real
        // editors share, matching every other `Editor*` action's own dual registration.
        // Revision R13 (issue #28) added 4 more real scoped bindings, all File-view-only
        // (`crate::merge::editing`'s `"merge-editor"` context deliberately does not get these):
        // `EditorSelectNextOccurrence` (`Ctrl+D`), `EditorSelectAllOccurrences` (`Ctrl+Shift+L`),
        // `EditorSkipOccurrence` (`Ctrl+K Ctrl+D`, all three `Some("file-editor")`), and
        // `EditorCollapseCursors` (`Esc`, `Some("file-editor && !completions")` - narrowed rather
        // than the other three's bare `"file-editor"` because GitHub issue #26 also wants the
        // File view's plain `Escape` for its own accessibility focus-out hatch, and only one
        // binding can genuinely own a keystroke at equal context depth; see `crate::
        // code_surface::editing::AdeApp::handle_editor_collapse_cursors_action`'s own docs for how
        // it composes both real behaviors from this one binding rather than a separate
        // `EditorEscape` shadowing or being shadowed by it).
        // GitHub issue #26 added 7 more real scoped bindings on top of that (not counting
        // `EditorCollapseCursors` above, which is issue #28's own action): `EditorIndent`/
        // `EditorDedent` under `"file-editor && !completions"` (2), the same two again plus
        // `EditorEscape` under plain `"merge-editor"` (3, `EditorEscape` here since
        // `"merge-editor"` never gets multi-cursor actions and so never faces the same collision
        // `EditorCollapseCursors` resolves in the File view), `CompletionsInvoke` under plain
        // `"file-editor"` (1), and `CloseFocusedTab` under `Some("!terminal")` (1, the same real
        // terminal-control-byte conflict class `"ctrl-shift-t"` above already established).
        // GitHub issue #105 added 3 more real scoped bindings: `FileTreeDelete` (`Delete`, runs
        // immediately - no confirmation step, so no different scoping from Copy/Cut/Paste/Rename
        // above it) and its own `FileTreeUndo`/`FileTreeRedo` (`Ctrl+Z`/`Ctrl+Shift+Z`) - distinct
        // actions from `TextUndo`/`TextRedo`, all three `Some("file-tree && !tree-editing")`.
        // GitHub issue #20 added 1 more real scoped binding: `TerminalClear`
        // (`cmd-k`/`ctrl-shift-l`), `Some("terminal")`.
        // GitHub issue #155 removed 1: `FileTreeContextMenu` (`Shift+F10`) - right-click plus
        // each row's own already-bound shortcut covers the same ground, so a second
        // keyboard-only path to the menu had no real justification of its own.
        // GitHub issue #158 added 2 more real scoped bindings: `TerminalCopy`/`TerminalPaste`
        // (`cmd-c`/`cmd-v` on macOS, `ctrl-shift-c`/`ctrl-shift-v` elsewhere), both
        // `Some("terminal")` - see `crate::default_key_bindings`'s own entry for why the shifted
        // variants, and why leaving them unbound was the bug.
        // GitHub issue #286 added 1 more real scoped binding: `v` -> `ToggleChangeSeen`, under
        // the same `Some("diff && !file-editor")` predicate `]` carries and for the same reason -
        // a plain letter must never be claimed over a focused terminal, and the seen-state it
        // toggles belongs to the file whose diff is open.
        // GitHub issue #304 added 6 more real scoped bindings: the interactive-rebase plan's own
        // `RebaseReorderUp`/`RebaseReorderDown`/`RebasePickRow`/`RebaseSquashRow`/`RebaseDropRow`
        // under `Some("rebase-plan && !text-input")` (5 - see `crate::default_key_bindings` for
        // why the negated conjunct is load-bearing over a surface that contains a real text
        // field) and `RebaseStart` under plain `Some("rebase-plan")` (1).
        // GitHub issue #288 added 2 more real scoped bindings, on the diff's own review-notes
        // surface: `SendReviewNotes` (`mod+enter`) under plain `Some("diff-view")` - the notes
        // bar draws it as keycaps, and having just typed a note is the likeliest moment to send
        // the batch - and `ToggleLineNote` (`c`) under `Some("diff-view && !text-input")`, the
        // same negated conjunct and the same reason as the rebase plan's plain letters above:
        // the pinned note card is a real text field inside that very container.
        // GitHub issue #162 added 1 more scoped binding: `mod+F` -> `FindInFile` under
        // `Some("file-editor")`. Its sibling `mod+shift+F` -> `SearchInWorktree` is deliberately
        // *global* and so is not counted here - the right panel's Search tab has no editor to be
        // scoped to, and a real Cmd/Ctrl-modified keystroke never reaches a focused pty anyway.
        // The Changes-footer prose fix added 1 more: `space` -> `ToggleChangeStaged`, alongside
        // `v` and `]` - the Changes footer advertises `space stage` and nothing was bound to it.
        // All three of those now also carry `&& !text-input`, since
        // issue #288's pinned note card is a real text input *inside* the `"diff"` node that
        // `"file-editor"` does not cover.
        // GitHub issue #336 added 4 more scoped bindings: `TextCopy`/`TextCut`/`TextPaste`/
        // `TextSelectAll` (`mod+C`/`mod+X`/`mod+V`/`mod+A`) under
        // `Some("text-input && !file-editor && !merge-editor")` - the same `"text-input"` tag
        // `TextUndo`/`TextRedo` above carry, with the two editor surfaces excluded because their
        // own `Editor*` actions already own those exact keystrokes there at the same predicate
        // depth (see `crate::default_key_bindings`' own entry).
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings, &[]);
        assert!(!rows.is_empty());
        let scoped: Vec<&KeybindingRow> =
            rows.iter().filter(|row| row.context != "global").collect();
        assert_eq!(
            scoped.len(),
            90,
            "expected `] -> NextChangedFile` (1) plus every real Editor* binding (19) plus \
             every real Completions* binding (5) plus every real merge-editor binding (18) plus \
             TextUndo/TextRedo (3, GitHub issue #17) plus every real \
             file-tree binding (7, GitHub issues #19 and #105, less 1 for issue #155's removed \
             FileTreeContextMenu) plus every real word-wise Editor* \
             binding (8, GitHub issue #27) plus every real multi-cursor Editor* binding (4, \
             GitHub issue #28) plus every real GitHub issue #26 binding (7, not counting \
             EditorCollapseCursors above, which is issue #28's own action) plus TerminalClear \
             (1, GitHub issue #20) plus TerminalCopy/TerminalPaste (2, GitHub issue #158) plus \
             every real interactive-rebase plan binding (6, GitHub issue #304) plus every \
             real review-note binding (2, GitHub issue #288) plus `space -> \
             ToggleChangeStaged` (1, the Changes footer's own `space stage` hint) plus \
             `mod+F -> FindInFile` (1, GitHub issue #162's in-file find) plus \
             TextCopy/TextCut/TextPaste/TextSelectAll (4, GitHub issue #336) to be scoped, \
             not global"
        );
        assert!(
            scoped
                .iter()
                .filter(|row| row.command.starts_with("Files tree: "))
                .count()
                == 7,
            "every file-tree binding must be reported as scoped - a globally-bound Ctrl+C would \
             be exactly the keystroke-swallowing bug class this list's own docs catalogue"
        );
        assert!(
            scoped.iter().any(|row| row.command == "Next changed file"),
            "the diff-scoped (now diff && !file-editor) ] binding must still be reported as \
             scoped"
        );
        assert!(
            scoped.iter().any(|row| row.command == "Editor: save file"),
            "a real file-editor-scoped binding must be reported as scoped too"
        );
    }

    #[test]
    fn keybinding_rows_carry_the_real_registered_keystroke() {
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings, &[]);
        let go_to_definition = rows
            .iter()
            .find(|row| row.command == "Go to definition")
            .expect("a Go to definition row should exist");
        assert_eq!(go_to_definition.keystrokes.len(), 1);
        assert_eq!(go_to_definition.keystrokes[0].key, "f12");
    }

    #[test]
    fn keybinding_rows_with_no_overrides_report_every_row_as_not_overridden() {
        let rows = keymap_page_rows();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| !row.is_overridden));
    }

    #[test]
    fn a_real_override_replaces_the_row_keystroke_and_marks_it_overridden() {
        let bindings = crate::default_key_bindings();
        let go_to_definition = bindings
            .iter()
            .find(|b| b.action().name() == "app::GotoDefinition")
            .expect("GotoDefinition should be a real default binding");
        let identity = crate::keymap_overrides::BindingIdentity::of(go_to_definition);
        let override_entry = crate::settings::store::KeybindingOverride {
            action: identity.action,
            context: identity.context,
            default_keystrokes: identity.default_keystrokes,
            keystrokes: "ctrl-shift-g".to_string(),
        };

        let rows = keybinding_rows(&bindings, std::slice::from_ref(&override_entry));
        let row = rows
            .iter()
            .find(|row| row.command == "Go to definition")
            .expect("a Go to definition row should exist");

        assert!(row.is_overridden);
        assert_eq!(row.keystrokes.len(), 1);
        assert_eq!(
            row.keystrokes[0],
            gpui::Keystroke::parse("ctrl-shift-g").unwrap()
        );

        // Every other row must be completely untouched by an override that isn't theirs.
        for other in &rows {
            if other.command == "Go to definition" {
                continue;
            }
            assert!(!other.is_overridden);
        }
    }

    /// A malformed persisted override (only reachable via a hand-edited `settings.toml`) must
    /// fall back to the real default keystroke, mirroring
    /// `keymap_overrides::effective_key_bindings`'s own fallback - the page must never claim a
    /// keystroke is registered when it genuinely isn't.
    #[test]
    fn a_malformed_override_keystroke_falls_back_to_the_real_default_and_is_not_marked_overridden()
    {
        let bindings = crate::default_key_bindings();
        let go_to_definition = bindings
            .iter()
            .find(|b| b.action().name() == "app::GotoDefinition")
            .expect("GotoDefinition should be a real default binding");
        let identity = crate::keymap_overrides::BindingIdentity::of(go_to_definition);
        let override_entry = crate::settings::store::KeybindingOverride {
            action: identity.action,
            context: identity.context,
            default_keystrokes: identity.default_keystrokes,
            // Three plain, non-modifier dash-separated segments - `gpui::Keystroke::parse` only
            // ever accepts a *single* trailing key after its recognized modifier prefixes, so
            // this is a real, guaranteed `Err`, not just an unusual-looking string.
            keystrokes: "one-two-three".to_string(),
        };
        assert!(
            gpui::Keystroke::parse(&override_entry.keystrokes).is_err(),
            "sanity check: this test's whole premise depends on this string genuinely failing \
             to parse"
        );

        let rows = keybinding_rows(&bindings, std::slice::from_ref(&override_entry));
        let row = rows
            .iter()
            .find(|row| row.command == "Go to definition")
            .expect("a Go to definition row should exist");

        assert!(!row.is_overridden);
        assert_eq!(row.keystrokes[0].key, "f12");
    }

    fn keymap_page_rows() -> Vec<KeybindingRow> {
        keybinding_rows(&crate::default_key_bindings(), &[])
    }

    #[test]
    fn filter_keybinding_rows_empty_query_returns_every_row() {
        let rows = keymap_page_rows();
        let filtered = filter_keybinding_rows(&rows, "");
        assert_eq!(filtered.len(), rows.len());
        let filtered_whitespace = filter_keybinding_rows(&rows, "   ");
        assert_eq!(filtered_whitespace.len(), rows.len());
    }

    #[test]
    fn filter_keybinding_rows_matches_command_case_insensitively() {
        let rows = keymap_page_rows();
        let filtered = filter_keybinding_rows(&rows, "PALETTE");
        // Exactly one row matches - `secondary-p` is the only keystroke bound to TogglePalette.
        assert_eq!(filtered.len(), 1);
        assert!(filtered.iter().all(|row| row.command == "Command palette"));
    }

    #[test]
    fn filter_keybinding_rows_matches_context_too() {
        let rows = keymap_page_rows();
        let filtered = filter_keybinding_rows(&rows, "global");
        // All but the deliberately-scoped `"diff"`/`"file-editor"` rows (20 total - see
        // `keybinding_rows_report_the_real_global_context_for_every_default_binding`).
        let scoped_count = rows.iter().filter(|row| row.context != "global").count();
        assert_eq!(filtered.len(), rows.len() - scoped_count);
    }

    #[test]
    fn filter_keybinding_rows_no_match_returns_empty() {
        let rows = keymap_page_rows();
        let filtered = filter_keybinding_rows(&rows, "nonexistent");
        assert!(filtered.is_empty());
    }
}
