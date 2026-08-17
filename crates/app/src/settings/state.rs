//! Pure data model for the Settings surface (`design_handoff_jerry_ade/revision/README.md`'s
//! "Settings" section). Maps already-real app state (agent binaries on `$PATH`, the worktree
//! list, the registered global keybindings) to what a settings row should show, with no `gpui`
//! dependency so it's directly unit-testable; `crate::root` turns the result into `gpui::Div`
//! trees. Config-file-backed values live in `crate::settings::store` instead - this module is
//! about live app state, not disk state.

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
                | SettingsPage::Notifications
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
pub(crate) fn action_label(action: &dyn gpui::Action) -> Option<&'static str> {
    match action.name() {
        "app::NewAgent" => Some("New agent"),
        // macOS-only (`cmd-q`, registered in `crate::default_key_bindings`), and the same wording
        // the application menu's own row uses (`title_bar::menu_model::MenuCommand::Quit`).
        "app::Quit" => Some("Quit Jerry"),
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
#[cfg(unix)]
const UNIX_SUPPLEMENTARY_SHELLS: [&str; 5] = ["fish", "nu", "pwsh", "elvish", "xonsh"];

/// Shell programs a Windows install may genuinely have on `%PATH%`, probed one by one - see
/// [`windows_shell_suggestions`], which only offers the ones a real search actually finds.
const WINDOWS_PROBED_SHELLS: [&str; 3] = ["powershell.exe", "pwsh.exe", "bash.exe"];

/// Every shell this machine genuinely has, for the field's suggestion list - the production
/// entry point, wired to the real `/etc/shells`, the real `%COMSPEC%`, and the real
/// `pty_core::resolve_on_path`.
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

    /// One row per `AGENT_KINDS` entry, each carrying its own real resolved path and status -
    /// the mixed case is the one that matters, since an all-or-nothing bug passes both extremes.
    #[test]
    fn detect_agent_rows_reports_each_binarys_own_real_status() {
        let all_found = detect_agent_rows(|name| Some(PathBuf::from(format!("/usr/bin/{name}"))));
        assert_eq!(all_found.len(), 2, "one row per AGENT_KINDS entry");
        assert!(all_found.iter().all(|row| row.is_ready()));
        assert!(all_found.iter().all(|row| row.status_label() == "ready"));
        let claude = all_found
            .iter()
            .find(|row| row.kind == AgentKind::Claude)
            .expect("a Claude row should exist");
        assert_eq!(claude.binary_name, "claude");
        assert_eq!(claude.resolved_path, Some(PathBuf::from("/usr/bin/claude")));

        let none_found = detect_agent_rows(|_name| None);
        assert!(none_found.iter().all(|row| !row.is_ready()));
        assert!(none_found
            .iter()
            .all(|row| row.status_label() == "not found"));

        let mixed =
            detect_agent_rows(|name| (name == "claude").then(|| PathBuf::from("/usr/bin/claude")));
        let ready = |kind| {
            mixed
                .iter()
                .find(|row| row.kind == kind)
                .expect("a row per kind")
                .is_ready()
        };
        assert!(ready(AgentKind::Claude));
        assert!(!ready(AgentKind::Codex));
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

    /// The dot a worktree row paints, over every real note shape. `Prunable` outranks plain
    /// `Clean`, a main checkout is always `Main` however dirty it is, and a *locked* merged-clean
    /// worktree is deliberately not prunable - the same rule `WorktreeNote::is_prunable` states,
    /// which this is a thin reduction of rather than a second implementation.
    #[test]
    fn worktree_dot_status_reduces_every_real_note_to_its_own_dot() {
        let unknown = WorktreeNote {
            is_main: false,
            clean: None,
            merge: None,
            is_locked: false,
        };
        for (name, is_main, note, expected) in [
            (
                "a dirty main checkout",
                true,
                note(Some(false), false, false),
                WorktreeDotStatus::Main,
            ),
            (
                "merged and clean",
                false,
                note(Some(true), true, false),
                WorktreeDotStatus::Prunable,
            ),
            (
                "dirty",
                false,
                note(Some(false), false, false),
                WorktreeDotStatus::Dirty,
            ),
            (
                "clean but unmerged",
                false,
                note(Some(true), false, false),
                WorktreeDotStatus::Clean,
            ),
            ("no note at all", false, unknown, WorktreeDotStatus::Unknown),
            (
                "locked, merged and clean",
                false,
                note(Some(true), true, true),
                WorktreeDotStatus::Clean,
            ),
        ] {
            assert_eq!(worktree_dot_status(is_main, &note), expected, "{name}");
        }
    }

    #[test]
    fn worktree_row_action_offers_prune_only_where_pruning_is_real() {
        let main_note = WorktreeNote {
            is_main: true,
            clean: Some(true),
            merge: None,
            is_locked: false,
        };
        for (name, is_main, note, expected) in [
            ("a main checkout", true, main_note, WorktreeRowAction::None),
            (
                "merged and clean",
                false,
                note(Some(true), true, false),
                WorktreeRowAction::Prune,
            ),
            (
                "clean but unmerged",
                false,
                note(Some(true), false, false),
                WorktreeRowAction::Open,
            ),
            (
                "dirty",
                false,
                note(Some(false), false, false),
                WorktreeRowAction::Open,
            ),
        ] {
            assert_eq!(worktree_row_action(is_main, &note), expected, "{name}");
        }
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

    #[test]
    fn every_documented_built_in_theme_name_still_resolves_by_name_lookup() {
        for name in ["Jerry Dark", "Jerry Dim", "Slate", "Ember", "Moss", "Paper"] {
            assert!(
                THEME_DEFS.iter().any(|def| def.name == name),
                "{name:?} must still resolve by name lookup"
            );
        }
    }

    #[test]
    fn settings_lsp_entries_count_matches_lsp_languages_len() {
        assert_eq!(
            crate::language::settings_lsp_entries().count(),
            lsp_languages().len()
        );
    }

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

    /// One row per registered language, each carrying its own real status - the mixed case is
    /// the one that matters, since an all-or-nothing bug passes both extremes.
    #[test]
    fn detect_lsp_rows_reports_each_binarys_own_real_status() {
        let all_found = detect_lsp_rows(|name| Some(PathBuf::from(format!("/usr/bin/{name}"))));
        assert_eq!(all_found.len(), lsp_languages().len());
        assert!(all_found.iter().all(|row| row.is_ready()));
        assert!(all_found.iter().all(|row| row.status_label() == "ready"));

        let none_found = detect_lsp_rows(|_| None);
        assert!(none_found.iter().all(|row| !row.is_ready()));
        assert!(none_found
            .iter()
            .all(|row| row.status_label() == "not installed"));

        let mixed = detect_lsp_rows(|name| {
            (name == "rust-analyzer").then(|| PathBuf::from("/usr/bin/rust-analyzer"))
        });
        let ready = |language| {
            mixed
                .iter()
                .find(|row| row.language == language)
                .unwrap_or_else(|| panic!("a {language} row should exist"))
                .is_ready()
        };
        assert!(ready("Rust"));
        assert!(!ready("Go"));
    }

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
    fn every_registered_global_keybinding_has_a_real_keybindings_page_label() {
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

    /// The keystroke-swallowing bug class this page's rows exist to make visible: a binding that
    /// claims a plain letter or a bare `Ctrl+C` *globally* eats that keystroke out of a focused
    /// terminal or text field. Each command below is registered with a real context predicate for
    /// that reason, so each must report as scoped rather than global.
    #[test]
    fn every_binding_registered_behind_a_context_is_reported_as_scoped_not_global() {
        let rows = keymap_page_rows();
        let scoped: Vec<&str> = rows
            .iter()
            .filter(|row| row.context != "global")
            .map(|row| row.command)
            .collect();
        assert!(
            !scoped.is_empty(),
            "premise: some bindings really are scoped"
        );
        for command in [
            "Files tree: rename",
            "Files tree: copy",
            "Files tree: cut",
            "Files tree: paste",
            "Files tree: delete",
            "Next changed file",
            "Mark file seen / unseen",
            "Stage / unstage file",
            "Editor: save file",
            "Completions: accept selected",
            "Terminal: clear",
            "Terminal: copy selection",
            "Rebase plan: pick",
            "Diff: note on this line",
        ] {
            assert!(
                scoped.contains(&command),
                "{command:?} must be reported as scoped - a global binding for it would swallow \
                 that keystroke out of a focused terminal or text field"
            );
        }
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

        for other in &rows {
            if other.command == "Go to definition" {
                continue;
            }
            assert!(!other.is_overridden);
        }
    }

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
