//! Pure data model for the Settings surface (`design_handoff_jerry_ade/revision/README.md`'s
//! "Settings" section). Maps already-real app state (agent binaries on `$PATH`, the worktree
//! list, the registered global keybindings) to what a settings row should show, with no `gpui`
//! dependency so it's directly unit-testable; `crate::root` turns the result into `gpui::Div`
//! trees. Config-file-backed values live in `crate::settings::store` instead - this module is
//! about live app state, not disk state.
//!
//! ## Which pages are real
//!
//! General, Agents, Worktrees, Appearance, Themes, Keybindings, Editor, and Language servers
//! render real, live-derived content (see [`SettingsPage::is_implemented`]). Notifications,
//! Integrations, and About are honest nav-only placeholders - `Jerry.dc.html`'s own `setStub`
//! copy, "not designed in this mockup". Editor is a partial exception, not a full one: its one
//! real row is the minimap (`crate::code_surface::minimap`, GitHub issue #30's
//! `editor.minimap.enabled`) - indentation/soft-wrap/whitespace-display still have no real
//! backing anywhere in this codebase, so those stay left off the page entirely rather than
//! growing controls bound to nothing, the same "only what's real" discipline every other page
//! here already follows.
//!
//! ## Why the Agents/Worktrees "Behaviour"/"Policy" toggle sections are left out
//!
//! `Jerry.dc.html`'s `settingsRows.agents`/`settingsRows.worktrees` fixtures show toggles like
//! "Plan before editing" or a "Worktree root" path field, but nothing in this app persists a
//! value per agent or per worktree (even `crate::settings::store::Settings` is a flat, global
//! struct with nowhere to hang a per-agent bool). Rendering them anyway would be a control bound
//! to nothing, so only the two sections backed by real, already-loaded state - the Installed
//! agents card and the Disk worktrees card - are built.

use std::path::PathBuf;

use crate::rail::state::WorktreeNote;
use crate::work_surface::agents::AgentKind;

/// Every page `Jerry.dc.html`'s `settingsNavDefs` fixture lists, in the order the design's four
/// nav groups present them (see [`nav_groups`]).
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
        )
    }

    /// The content column's one-line rationale under the page title
    /// (`design_handoff_jerry_ade/revision/README.md`'s "Content column" section) - app-authored
    /// text, not copy from the mockup (`Jerry.dc.html` has no per-page subtitle fixture). Every
    /// nav-only page shares the same placeholder text; the placeholder page *body* is separately
    /// the mockup's verbatim `setStub` copy - see `crate::settings::render::render_settings_placeholder_page`.
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

/// The fixed nav structure - `Jerry.dc.html`'s own `settingsNavDefs` grouping and order,
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

/// Agent kinds this app can spawn as an "agent" - `AgentKind::Shell` is deliberately excluded,
/// since the Settings › Agents card lists agent CLIs, not shells.
pub const AGENT_KINDS: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

/// One row for the Agents page's Installed card - see [`detect_agent_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub kind: AgentKind,
    /// The exact command name `AgentKind::agent_binary_name` hands to `TerminalSpec::command`
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

    /// The status label next to the row's status dot ("green dot + 'ready'" per
    /// `design_handoff_jerry_ade/revision/README.md`), or the honest opposite when not found.
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
        .filter_map(|kind| {
            kind.agent_binary_name().map(|binary_name| AgentRow {
                kind,
                binary_name,
                resolved_path: resolve(binary_name),
            })
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

/// A worktree row's right-aligned action - `design_handoff_jerry_ade/README.md`'s "a
/// right-aligned Open ... or Prune ... action" (the main checkout's row has no action at all;
/// every other row is `Open` or `Prune`, never both).
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

/// One Themes-page card - `Jerry.dc.html`'s own `themeDefs` fixture, transcribed verbatim (name,
/// subtitle, five swatch hex colours). Kept as plain `u32` (converted to `gpui::Rgba` at the
/// render call site, like `crate::terminal::pane`'s own one-off literal colours) so this module
/// stays free of a `gpui` dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeDef {
    pub name: &'static str,
    pub subtitle: &'static str,
    pub swatches: [u32; 5],
}

/// The Themes page's six cards, transcribed verbatim from `Jerry.dc.html`'s `themeDefs`.
/// `crate::settings::store::ThemeSettings::name` - not this fixture's own `on` field, which this
/// app never reads - is the persisted source of truth for which one is selected.
pub const THEME_DEFS: [ThemeDef; 6] = [
    ThemeDef {
        name: "Jerry Dark",
        subtitle: "default",
        swatches: [0x0e0f11, 0x1a1e21, 0x5cb87f, 0xe2a336, 0x74ade8],
    },
    ThemeDef {
        name: "Jerry Dim",
        subtitle: "lower contrast",
        swatches: [0x15181b, 0x20252a, 0x6ab97f, 0xd8a94a, 0x7f9ad4],
    },
    ThemeDef {
        name: "Slate",
        subtitle: "cool greys",
        swatches: [0x0d1117, 0x161b22, 0x57a773, 0xc9a227, 0x6b9bd1],
    },
    ThemeDef {
        name: "Ember",
        subtitle: "warm",
        swatches: [0x12100e, 0x1e1a16, 0x8fae6b, 0xd98b3a, 0xc4713f],
    },
    ThemeDef {
        name: "Moss",
        subtitle: "green-tinted",
        swatches: [0x0f1310, 0x1a201b, 0x7fc79a, 0xc8b45a, 0x6f9bb5],
    },
    ThemeDef {
        name: "Paper",
        subtitle: "light \u{b7} beta",
        swatches: [0xf4f1ea, 0xe4e0d6, 0x3f7a52, 0xa8752a, 0x3d6c9c],
    },
];

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
    /// Generic descriptive copy, not a live count - `Jerry.dc.html`'s own `lspDefs` notes mix
    /// this with fabricated live data ("1,284 crates indexed") this app has no per-language
    /// agent summary to back (`crate::lsp::client`'s server clients are keyed by worktree, not
    /// surfaced here). Every note here is deliberately the descriptive kind only.
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

    /// `Jerry.dc.html`'s own word for the not-found state - `"not installed"`, distinct from the
    /// Agents page's `"not found"`; each page keeps its own mockup's wording.
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
        "app::NextChangedFile" => Some("Next changed file"),
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
        // Deliberately *not* bare "Undo"/"Redo": GitHub issue #17 adds a second, genuinely
        // distinct undo system on the same physical keys (text undo, below), and two rows both
        // labelled "Undo" on this page would be exactly the confusion this project's own
        // "distinguishable rows" rule exists to prevent. The context column already differs, but
        // a context predicate is not what a user reads first.
        "app::Undo" => Some("Worktree history: undo"),
        "app::Redo" => Some("Worktree history: redo"),
        "app::TextUndo" => Some("Text: undo"),
        "app::TextRedo" => Some("Text: redo"),
        "app::CloseFocusedTab" => Some("Close focused tab"),
        "app::FileTreeContextMenu" => Some("Files tree: context menu"),
        "app::FileTreeRename" => Some("Files tree: rename"),
        "app::FileTreeCopy" => Some("Files tree: copy"),
        "app::FileTreeCut" => Some("Files tree: cut"),
        "app::FileTreePaste" => Some("Files tree: paste"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rail::state::WorktreeNote;
    use wt_core::diff::WorktreeMergeStatus;

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
    fn exactly_the_eight_documented_pages_are_implemented() {
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
        let placeholder = SettingsPage::Notifications.subtitle();
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
                "Worktree history: undo",
                "Worktree history: redo",
                "Text: undo",
                "Text: redo",
                "Text: redo",
                "Go to definition",
                "New terminal",
                "New agent pane",
                "Next changed file",
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
                "Files tree: context menu",
                "Files tree: rename",
                "Files tree: copy",
                "Files tree: cut",
                "Files tree: paste",
                // GitHub issue #26's `Ctrl+W` - closes the focused tab, scoped `Some("!terminal")`.
                "Close focused tab",
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
        // Revision R10 added 2 more real scoped bindings, `Undo`/`Redo`, each `Some("!terminal")`
        // - a real, live-reproduced-conflict fix in the same class as `]`'s own
        // `"diff && !file-editor"` narrowing above: `secondary-z` resolves to plain `Ctrl+Z` on
        // Linux/Windows, which a focused terminal needs unclaimed to send the real `SIGTSTP`
        // suspend control byte (`crate::terminal::pane::keystroke_to_bytes`) - see
        // `crate::default_key_bindings`'s own docs for the full reasoning.
        // GitHub issue #17 added 3 more real scoped bindings, `TextUndo` (`secondary-z`) and
        // `TextRedo` (bound twice - `secondary-shift-z` and `ctrl-y`), each `Some("text-input")`,
        // and correspondingly narrowed `Undo`/`Redo` from `Some("!terminal")` to
        // `Some("!terminal && !text-input")` - still scoped either way, so only the count of
        // scoped rows moves. See `crate::default_key_bindings`'s own docs for why the two undo
        // systems are kept disjoint structurally rather than by dispatch order.
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
        // terminal-control-byte conflict class as `Undo`/`Redo` above).
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings, &[]);
        assert!(!rows.is_empty());
        let scoped: Vec<&KeybindingRow> =
            rows.iter().filter(|row| row.context != "global").collect();
        assert_eq!(
            scoped.len(),
            72,
            "expected `] -> NextChangedFile` (1) plus every real Editor* binding (19) plus \
             every real Completions* binding (5) plus every real merge-editor binding (18) plus \
             Undo/Redo (2) plus TextUndo/TextRedo (3, GitHub issue #17) plus every real \
             file-tree binding (5, GitHub issue #19) plus every real word-wise Editor* binding \
             (8, GitHub issue #27) plus every real multi-cursor Editor* binding (4, GitHub issue \
             #28) plus every real GitHub issue #26 binding (7, not counting \
             EditorCollapseCursors above, which is issue #28's own action) to be scoped, not \
             global"
        );
        assert!(
            scoped
                .iter()
                .filter(|row| row.command.starts_with("Files tree: "))
                .count()
                == 5,
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
