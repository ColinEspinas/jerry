//! The Settings surface's pure data model - `design_handoff_jerry_ade/revision/README.md`'s
//! "Settings" section: "a separate surface, not a modal: it replaces the three zones while the
//! title bar and status bar stay." Mirrors `crate::rail`/`crate::palette`/`crate::work_surface`'s
//! own split: only the mapping from already-real app state (which agent binaries are actually on
//! `$PATH`, the real worktree list Phase B already built, the real global keybindings
//! `crate::default_key_bindings` registers) to what a settings row should show lives here,
//! directly unit-testable without a live GPUI window; turning the result into actual `gpui::Div`
//! trees happens in `crate::root`, which owns the `Context<AdeApp>` real actions (opening a
//! worktree, pruning) need. Real, config-file-backed values (`crate::settings_store::Settings`)
//! live in that separate module, not here - this one stays about live app state, not disk state.
//!
//! ## Which pages are real (Revision R3)
//!
//! `design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29 entry, change 3 adds five real
//! pages to the two Phase F already built: **Appearance & scaling**, **Themes**, **Keybindings**
//! (`SettingsPage::Keymap`), and **Language servers** all now render real, live-derived content,
//! alongside the pre-existing **Agents**/**Worktrees**. **General** gains one real row (`Window
//! controls`) but is not otherwise fully "designed" the way those five are - see its own
//! `subtitle`/`crate::root::settings_render` docs for the two rows this phase deliberately
//! leaves out and why. **Editor** stays nav-only: zero real backing exists anywhere in this
//! codebase for indentation/soft-wrap/whitespace-display, and building a plausible-looking
//! control for any of them would be exactly the "component bound to nothing" this project's
//! constraints forbid - the same judgment call this module's own "Why the Agents/Worktrees
//! toggle sections are left out" section (below) already made once. Notifications/Integrations/
//! About remain honest, nav-only "not designed in this mockup" placeholders - `Jerry.dc.html`'s
//! own `setStub` state's exact real copy (line ~857: `not designed in this mockup`).
//!
//! ## Why the Agents/Worktrees `setRows` "Behaviour"/"Policy" toggle sections are left out
//!
//! `Jerry.dc.html`'s `settingsRows.agents`/`settingsRows.worktrees` fixtures (a "Plan before
//! editing" toggle, a "Max parallel sessions" stepper, a "Worktree root" path field, and so on)
//! are sample *settings values* with no real, per-agent/per-worktree persistence layer behind
//! them in this app (even R3's new `crate::settings_store::Settings` is a flat, global struct -
//! it has nowhere to hang a *per-agent* `plan_before_editing` bool). Rendering them anyway would
//! be exactly the kind of decorative, bound-to-nothing control this project's constraints
//! forbid. Only the two sections this phase can back with real, already-loaded application
//! state (the Installed agents card, the Disk worktrees card) are built; see `crate::root`'s
//! Settings render methods for where those real data sources are.

use std::path::PathBuf;

use crate::rail::WorktreeNote;
use crate::sessions::SessionKind;

/// Every real page `design_handoff_jerry_ade/revision/Jerry.dc.html`'s `settingsNavDefs`
/// fixture lists, in the exact left-to-right, top-to-bottom order the design's four nav groups
/// present them (see [`nav_groups`]).
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

    /// A stable, unique string for this page - used as the GPUI element id suffix for its nav
    /// row and content column (`crate::root`'s Settings render methods), so every page's row
    /// keeps an identity GPUI can track across renders independent of its (constant, but
    /// spelled-out-for-humans) [`Self::label`].
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
                | SettingsPage::LanguageServers
        )
    }

    /// The content column's one-line rationale under the page title
    /// (`design_handoff_jerry_ade/revision/README.md`'s "Content column" section). Real,
    /// specific text for every implemented page (rewritten from `Jerry.dc.html`'s own
    /// `settingsMeta` fixture where that fixture describes a section this app doesn't build -
    /// see the module docs). This subtitle itself is app-authored explanatory text, not copy
    /// from the mockup for the nav-only pages - `Jerry.dc.html` has no per-page subtitle
    /// fixture at all for nav-only pages, so every nav-only page shares the same honest,
    /// app-written "not designed in this mockup" explanation below it. (The placeholder page
    /// *body*, separately, in `crate::root::render_settings_placeholder_page`, is the mockup's
    /// actual verbatim `setStub` copy - see that function's docs; this subtitle is not that.)
    pub fn subtitle(self) -> &'static str {
        match self {
            SettingsPage::General => {
                "Window chrome. Restore-on-launch, a default environment and a discard confirmation aren't wired to anything real yet, so they're left off this page rather than shown inert."
            }
            SettingsPage::Agents => {
                "Which agent binaries Jerry can actually find on PATH right now - detected live, not configured."
            }
            SettingsPage::Worktrees => {
                "Every session gets its own worktree. This is where they live, their real disk usage, and what's safe to prune."
            }
            SettingsPage::Appearance => {
                "These sizes and scale are saved for real, but nothing in the interface renders at them yet."
            }
            SettingsPage::Theme => {
                "Dark-first. Picking a theme other than Jerry Dark is saved for real, but the app doesn't change its colors to match yet."
            }
            SettingsPage::Keymap => {
                "Every real, globally-bound shortcut this build actually dispatches. The same commands bind to Ctrl and Alt on Windows and Linux."
            }
            SettingsPage::LanguageServers => {
                "One row per language server this app knows how to spawn, detected live on PATH - not configured."
            }
            _ => "Not designed yet - this page has no real content in this build.",
        }
    }
}

/// One of the Settings nav's four grouped sections (`design_handoff_jerry_ade/revision/
/// CHANGELOG.md`'s change 3: "Nav regrouped: Workspace ... Interface ... Editor ... Other").
pub struct NavGroup {
    pub label: &'static str,
    pub pages: Vec<SettingsPage>,
}

/// The real, fixed nav structure - `Jerry.dc.html`'s own `settingsNavDefs` grouping, unchanged
/// (every page listed there is included here, in the same order), so every page the design
/// lists is real, clickable navigation even though not every page renders real content past
/// that point (see [`SettingsPage::is_implemented`]).
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

/// Every real agent kind this app knows how to spawn as an "agent" (as opposed to a plain
/// shell) - `crate::sessions::SessionKind::Shell` is deliberately excluded, mirroring
/// `crate::work_surface::agent_tint`'s own docs: the design's Settings › Agents card is a list
/// of *agent CLIs*, and a shell isn't one.
pub const AGENT_KINDS: [SessionKind; 2] = [SessionKind::Claude, SessionKind::Codex];

/// One real row for the Agents page's Installed card - see [`detect_agent_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub kind: SessionKind,
    /// The exact literal command name `crate::sessions::SessionKind::agent_binary_name` hands
    /// to `TerminalSpec::command` at real spawn time - the same name [`resolved_path`] was
    /// searched for.
    ///
    /// [`resolved_path`]: AgentRow::resolved_path
    pub binary_name: &'static str,
    /// `Some(path)` if a real `$PATH` search (`pty_core::resolve_on_path`, injected via
    /// [`detect_agent_rows`]'s `resolve` parameter so this stays testable without touching the
    /// real filesystem/environment) found the binary; `None` if it genuinely isn't installed on
    /// this machine - never a guess either way.
    pub resolved_path: Option<PathBuf>,
}

impl AgentRow {
    pub fn is_ready(self: &AgentRow) -> bool {
        self.resolved_path.is_some()
    }

    /// The real status label next to the row's status dot - `design_handoff_jerry_ade/
    /// README.md`'s "green dot + 'ready'", or the honest opposite when the binary wasn't found.
    pub fn status_label(&self) -> &'static str {
        if self.is_ready() {
            "ready"
        } else {
            "not found"
        }
    }
}

/// Builds one real [`AgentRow`] per [`AGENT_KINDS`] entry, resolving each one's real binary name
/// via `resolve` - in `crate::root`, always `pty_core::resolve_on_path` (the same real `$PATH`
/// search `pty-core`'s own spawn path effectively performs - see that function's docs), so a
/// row's "ready"/"not found" status reflects a real `$PATH` + execute-bit search, not a fabricated
/// guess. This is *not* an absolute guarantee that spawning would actually succeed -
/// `pty_core::resolve_on_path`'s own docs disclose the same gap this carries forward: its
/// executable check is real file-permission-bit metadata, not a real `access(2)` call, so it
/// doesn't itself account for ACLs or a process's specific uid/gid (e.g. a file with only a
/// group-execute bit set can pass this check while the calling process still can't actually run
/// it). Takes `resolve` as a parameter (rather than calling `pty_core` directly) so this mapping
/// is unit-testable with a fake, deterministic resolver instead of depending on which binaries
/// happen to be installed on whatever machine runs the test suite.
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

/// A worktree row's real status dot state on the Worktrees page - derived from the exact same
/// [`WorktreeNote`] Phase B's rail already computes (`crate::rail::compute_status_snapshot`),
/// never a second, independent notion of worktree health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeDotStatus {
    /// The main checkout - never prunable, never "dirty" in the sense that matters here.
    Main,
    /// Clean and not (yet) merged - nothing to do.
    Clean,
    /// Real uncommitted changes - matches [`WorktreeNote::clean`] being `Some(false)`.
    Dirty,
    /// A real prune candidate on its own merits - see [`WorktreeNote::is_prunable`]. This is
    /// *not* the final "safe to remove right now" answer (a live session could still exclude
    /// it - see `crate::rail::prunable_worktree_paths`), just this row's own real, local state.
    Prunable,
    /// `wt_core::is_dirty` itself failed for this path - genuinely unknown, not a guess.
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

/// A worktree row's real right-aligned action - `design_handoff_jerry_ade/README.md`'s "a
/// right-aligned Open ... or Prune ... action", matching `Jerry.dc.html`'s own `wtDefs` fixture
/// shape exactly (the main checkout's row has no action at all; every other row is `Open` or
/// `Prune`, never both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeRowAction {
    /// The main checkout - `git worktree remove` refuses it outright and there is nowhere else
    /// to "open" it (it's the initial worktree browsing already defaults to).
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

/// One real Themes-page card - `design_handoff_jerry_ade/revision/Jerry.dc.html`'s own
/// `themeDefs` fixture, transcribed verbatim (name, subtitle, five real swatch hex colours).
/// `crate::root::settings_render` converts each `swatches` entry to a real `gpui::Rgba` via
/// `gpui::rgb` at the render call site (matching `crate::terminal_pane`'s own precedent for a
/// one-off literal colour that isn't a `crate::theme` token) - kept as plain `u32` here so this
/// module stays free of a `gpui` dependency, mirroring its own module docs' "pure data" split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeDef {
    pub name: &'static str,
    pub subtitle: &'static str,
    pub swatches: [u32; 5],
}

/// The Themes page's six real cards - transcribed verbatim from `Jerry.dc.html`'s `themeDefs`.
/// `crate::settings_store::ThemeSettings::name` (not this fixture's own `on` field, which this
/// app never reads) is the real, persisted source of truth for which one is selected - see
/// `crate::root::settings_render`'s Themes page docs.
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

/// One real Language servers page row's static, per-language identity - the real binary name
/// `crate::root::settings_render` searches `$PATH` for via [`detect_lsp_rows`]. Binary names
/// verified for real (not guessed): `rust-analyzer`, `typescript-language-server` (the
/// `typescript-language-server` npm package's own real binary name), `vue-language-server`
/// (the modern Volar-based `@vue/language-server` package's real binary - not the older,
/// deprecated `vls`), `pyright-langserver` (`pyright`'s own real LSP-mode entry point, distinct
/// from the plain `pyright` CLI type-checker binary), `gopls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspLanguage {
    pub language: &'static str,
    /// The file extension chip label - fed to `crate::file_tree::lang_chip_for_name` (via a
    /// synthetic `"x.<ext>"` name) at the real render call site, so this row's chip is the
    /// exact same real, extension-derived chip a file-tree row for that language would show -
    /// never a hand-assigned colour. Extensions this app's chip table doesn't yet recognise
    /// (`ts`/`vue`/`py`/`go` - only `rs`/`toml`/`md`/`sql` are wired, see that function's own
    /// docs) honestly fall back to its real neutral chip rather than a fabricated coloured one.
    pub ext: &'static str,
    pub binary: &'static str,
    /// Real, generic descriptive copy - not a live count. `Jerry.dc.html`'s own `lspDefs` notes
    /// mix genuine descriptive copy ("installs when the first .go file opens") with fabricated
    /// live session data ("1,284 crates indexed", "tsserver 5.6 · 2 tsconfig projects") this
    /// app cannot actually know per-Settings-page (there is no live, per-language LSP session
    /// summary to read here - `crate::root::lsp`'s real `rust-analyzer` client is keyed by
    /// worktree, not surfaced to this page). Every note here is deliberately the former kind
    /// only.
    pub note: &'static str,
}

pub const LSP_LANGUAGES: [LspLanguage; 5] = [
    LspLanguage {
        language: "Rust",
        ext: "rs",
        binary: "rust-analyzer",
        note: "starts when a .rs file opens",
    },
    LspLanguage {
        language: "TypeScript",
        ext: "ts",
        binary: "typescript-language-server",
        note: "starts when a .ts file opens",
    },
    LspLanguage {
        language: "Vue",
        ext: "vue",
        binary: "vue-language-server",
        note: "starts when a .vue file opens",
    },
    LspLanguage {
        language: "Python",
        ext: "py",
        binary: "pyright-langserver",
        note: "starts when a .py file opens",
    },
    LspLanguage {
        language: "Go",
        ext: "go",
        binary: "gopls",
        note: "installs when the first .go file opens",
    },
];

/// One real Language servers page row - see [`detect_lsp_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspRow {
    pub language: &'static str,
    pub ext: &'static str,
    pub binary: &'static str,
    pub note: &'static str,
    /// `Some(path)` if a real `$PATH` search found `binary`; `None` if it genuinely isn't
    /// installed - same real-search contract as `AgentRow::resolved_path`, see that field's
    /// own docs for the one real, disclosed gap (a file-permission-bit check, not `access(2)`).
    pub resolved_path: Option<PathBuf>,
}

impl LspRow {
    pub fn is_ready(&self) -> bool {
        self.resolved_path.is_some()
    }

    /// `Jerry.dc.html`'s own `lspDefs` fixture's real word for the not-found state
    /// (`"not installed"`, distinct from the Agents page's `"not found"` - each page keeps its
    /// own mockup's real wording rather than a homogenized one).
    pub fn status_label(&self) -> &'static str {
        if self.is_ready() {
            "ready"
        } else {
            "not installed"
        }
    }
}

/// Builds one real [`LspRow`] per [`LSP_LANGUAGES`] entry, resolving each one's real binary name
/// via `resolve` - in `crate::root`, always `pty_core::resolve_on_path`, the same real `$PATH`
/// search [`detect_agent_rows`] uses for the Agents page. Takes `resolve` as a parameter for the
/// same unit-testability reason [`detect_agent_rows`] does.
pub fn detect_lsp_rows(resolve: impl Fn(&str) -> Option<PathBuf>) -> Vec<LspRow> {
    LSP_LANGUAGES
        .into_iter()
        .map(|def| LspRow {
            language: def.language,
            ext: def.ext,
            binary: def.binary,
            note: def.note,
            resolved_path: resolve(def.binary),
        })
        .collect()
}

/// One row of the Keybindings settings page - real data, built by [`keybinding_rows`] from
/// `crate::default_key_bindings`'s actual, live-registered `gpui::KeyBinding`s, never a second,
/// hand-maintained parallel list. This replaced exactly that: a hand-transcribed `[KeybindingRow;
/// 4]` array with its own re-typed spec strings (`"mod+K"`, ...) and its own re-typed `context`
/// per row, positioned in a hand-chosen order - independent of `crate::default_key_bindings`'s
/// real registrations, so nothing caught it when the two quietly disagreed: the real `F12`
/// binding is registered with context `None` (global), but the old hand-copied row said
/// `"editor"`. `context`/`keystrokes` here are always read straight off the real `KeyBinding` -
/// there is no way for this row to describe a binding differently than what's really registered,
/// because it *is* that registration, reshaped for rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct KeybindingRow {
    pub command: &'static str,
    pub context: &'static str,
    /// The real, already-registered keystroke(s) for this binding (almost always exactly one -
    /// none of this app's real global bindings are multi-keystroke chords right now) - resolved
    /// to real per-platform keycaps via `crate::keymap::resolve_keystroke` at the render call
    /// site, never a literal glyph or a hand-authored spec string here.
    pub keystrokes: Vec<gpui::Keystroke>,
}

/// A small, real mapping from this app's globally-bound `gpui::Action` types to the Keybindings
/// page's human command label, keyed by each action's own real, compiler-generated
/// [`gpui::Action::name`] (e.g. `"app::NewSession"` - stable per action type, not guessed). This
/// is the fallback [`keybinding_rows`]'s own docs describe: there is no existing, reusable
/// action-to-label mapping this app can pull from for these four (the command palette's own
/// `crate::palette::PaletteCommand::label` is close for three of them, but
/// `TogglePalette`/`GotoDefinition` have no `PaletteCommand` counterpart at all - the palette
/// can't open itself, and go-to-definition isn't a palette command), so this table exists
/// specifically for this page.
///
/// The test `every_registered_global_keybinding_has_a_real_keybindings_page_label` (below) is the
/// real drift guard this table needs in place of the position/order guarantees a hand-authored
/// row list used to (accidentally) provide: it iterates the real `crate::default_key_bindings()`
/// and asserts every one of them resolves to `Some` here, so adding a new global binding without
/// adding its label here fails a test - not silently renders blank or missing on the Keybindings
/// page.
fn action_label(action: &dyn gpui::Action) -> Option<&'static str> {
    match action.name() {
        "app::NewSession" => Some("New session"),
        "app::TogglePalette" => Some("Command palette"),
        "app::ToggleSettings" => Some("Open settings"),
        "app::GotoDefinition" => Some("Go to definition"),
        _ => None,
    }
}

/// Builds the Keybindings page's real rows straight from `bindings` (in production, always
/// `crate::default_key_bindings()`) - see [`KeybindingRow`]'s own docs for why this replaced a
/// second, hand-maintained parallel list. Row order is always real registration order (there is
/// no separate order to drift). `context` is `"global"` when the real `gpui::KeyBinding` has no
/// context predicate (every one of this app's four real global bindings today - see
/// `crate::default_key_bindings`'s own `KeyBinding::new(.., None)` calls) and `"scoped"`
/// otherwise - a real reduction of the real predicate, not a guess. A binding whose action has no
/// [`action_label`] entry is skipped rather than shown with a blank/fabricated label - see that
/// function's own docs for the test that keeps this from happening silently.
pub fn keybinding_rows(bindings: &[gpui::KeyBinding]) -> Vec<KeybindingRow> {
    bindings
        .iter()
        .filter_map(|binding| {
            let command = action_label(binding.action())?;
            let context = if binding.predicate().is_none() {
                "global"
            } else {
                "scoped"
            };
            let keystrokes = binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().clone())
                .collect();
            Some(KeybindingRow {
                command,
                context,
                keystrokes,
            })
        })
        .collect()
}

/// The Keybindings page's real filter row logic (`design_handoff_jerry_ade/revision/
/// CHANGELOG.md`'s change 3: "filter row (`/ filter N bindings`, right-aligned count)") - a
/// case-insensitive substring match against a row's command name or context, matching
/// `crate::rail::filter_sessions`'s own real filtering shape. An empty (or all-whitespace)
/// query matches every row, same as that function's own empty-query behaviour.
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
    use crate::rail::WorktreeNote;
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
        // No duplicates: every real page appears in exactly one group.
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
    fn exactly_the_seven_documented_pages_are_implemented() {
        for page in SettingsPage::ALL {
            let expected = matches!(
                page,
                SettingsPage::General
                    | SettingsPage::Agents
                    | SettingsPage::Worktrees
                    | SettingsPage::Appearance
                    | SettingsPage::Theme
                    | SettingsPage::Keymap
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
        let placeholder = SettingsPage::Editor.subtitle();
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
            .find(|row| row.kind == SessionKind::Claude)
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
        // Exactly this app's real, honest dev-machine state at the time this phase shipped:
        // `claude` present, `codex` absent (see `crate::sessions::SessionKind::Codex`'s own
        // module docs) - proof the two rows' statuses are independent, not all-or-nothing.
        let rows = detect_agent_rows(|name| {
            if name == "claude" {
                Some(PathBuf::from("/usr/bin/claude"))
            } else {
                None
            }
        });
        let claude = rows
            .iter()
            .find(|row| row.kind == SessionKind::Claude)
            .expect("claude row");
        let codex = rows
            .iter()
            .find(|row| row.kind == SessionKind::Codex)
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
        // Mirrors `crate::rail`'s own
        // `worktree_note_locked_merged_clean_worktree_is_never_prunable_but_label_says_locked`
        // test - a locked worktree must never show as `Prunable` here either, since this
        // function is a thin, real reduction of `WorktreeNote::is_prunable`, not a second
        // implementation of the same rule.
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

    #[test]
    fn detect_lsp_rows_reports_ready_for_a_resolver_that_finds_every_binary() {
        let rows = detect_lsp_rows(|name| Some(PathBuf::from(format!("/usr/bin/{name}"))));
        assert_eq!(rows.len(), LSP_LANGUAGES.len());
        assert!(rows.iter().all(|row| row.is_ready()));
        assert!(rows.iter().all(|row| row.status_label() == "ready"));
    }

    #[test]
    fn detect_lsp_rows_reports_not_installed_for_a_resolver_that_finds_nothing() {
        let rows = detect_lsp_rows(|_| None);
        assert!(rows.iter().all(|row| !row.is_ready()));
        assert!(rows.iter().all(|row| row.status_label() == "not installed"));
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
        // The real drift guard `action_label`'s own docs describe: if `crate::default_key_bindings`
        // ever grows a new global binding without a matching `action_label` entry, this fails -
        // silently rendering that binding blank/missing on the Keybindings page never ships
        // unnoticed the way a purely positional/length-only check could miss.
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings);
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
    fn keybinding_rows_are_derived_in_real_registration_order() {
        // There is no separate, hand-authored order to drift from real registration order
        // anymore - `keybinding_rows` just reads `crate::default_key_bindings()`'s own order.
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings);
        let commands: Vec<&str> = rows.iter().map(|row| row.command).collect();
        assert_eq!(
            commands,
            vec![
                "New session",
                "Command palette",
                "Open settings",
                "Go to definition",
            ]
        );
    }

    #[test]
    fn keybinding_rows_report_the_real_global_context_for_every_default_binding() {
        // The real bug this replaced: the old hand-copied list labeled `Go to definition`
        // `context: "editor"`, but `crate::default_key_bindings` actually registers it (like
        // every other entry) with `KeyBinding::new(.., None)` - a real, global context. Every
        // row here is derived from that same real `None`, so every row must say `"global"`.
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row.context == "global"));
    }

    #[test]
    fn keybinding_rows_carry_the_real_registered_keystroke() {
        let bindings = crate::default_key_bindings();
        let rows = keybinding_rows(&bindings);
        let go_to_definition = rows
            .iter()
            .find(|row| row.command == "Go to definition")
            .expect("a Go to definition row should exist");
        assert_eq!(go_to_definition.keystrokes.len(), 1);
        assert_eq!(go_to_definition.keystrokes[0].key, "f12");
    }

    fn keymap_page_rows() -> Vec<KeybindingRow> {
        keybinding_rows(&crate::default_key_bindings())
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
        assert_eq!(filtered[0].command, "Command palette");
    }

    #[test]
    fn filter_keybinding_rows_matches_context_too() {
        let rows = keymap_page_rows();
        let filtered = filter_keybinding_rows(&rows, "global");
        assert_eq!(filtered.len(), rows.len(), "every real row is global");
    }

    #[test]
    fn filter_keybinding_rows_no_match_returns_empty() {
        let rows = keymap_page_rows();
        let filtered = filter_keybinding_rows(&rows, "nonexistent");
        assert!(filtered.is_empty());
    }
}
