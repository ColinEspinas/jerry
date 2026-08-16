//! Pure, GPUI-free data model for the command palette (⌘P): scope/matching/ranking/grouping
//! over already-real app state (open agents, the loaded file tree, a fixed command list),
//! kept unit-testable without a live GPUI window. `crate::palette::render` turns the
//! result into `gpui::Div` trees and real click/key handlers, since it owns the `Context<AdeApp>`
//! those need. Every [`PaletteCommand`] variant maps one-to-one onto an existing `AdeApp` method
//! (see `crate::root::AdeApp::execute_palette_command`) - none is a stub.

use std::path::PathBuf;

use crate::rail::status::Status;
use crate::work_surface::agents::{AgentId, ProcessKind};

/// Cap on how many rows a single group ([`PaletteGroup`]) contributes, independent of how many
/// candidates matched - a palette meant to answer "which of these am I looking for" at a glance
/// stops being that once a group scrolls past a screenful.
const MAX_ENTRIES_PER_GROUP: usize = 8;

/// Which of the palette's three scopes is active, matching the input row's segmented
/// `All ⇥ / Commands › / Files @` control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteScope {
    #[default]
    All,
    Commands,
    Files,
}

impl PaletteScope {
    /// Advances to the next scope in the segmented control's left-to-right order - the
    /// footer's `⇥ next scope` action, distinct from the typed-prefix route in
    /// [`typed_scope_prefix`].
    pub fn cycle(self) -> Self {
        match self {
            PaletteScope::All => PaletteScope::Commands,
            PaletteScope::Commands => PaletteScope::Files,
            PaletteScope::Files => PaletteScope::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PaletteScope::All => "All",
            PaletteScope::Commands => "Commands",
            PaletteScope::Files => "Files",
        }
    }

    /// The segmented control's per-scope key hint.
    pub fn segment_key(self) -> &'static str {
        match self {
            PaletteScope::All => "\u{21e5}",
            PaletteScope::Commands => "\u{203A}",
            PaletteScope::Files => "@",
        }
    }

    /// The input row's prefix glyph. `All` and `Commands` deliberately share `'›'` - the
    /// design's own fixture gives them the same prefix; only `Files`' `'@'` differs.
    pub fn prefix_glyph(self) -> &'static str {
        match self {
            PaletteScope::All | PaletteScope::Commands => "\u{203A}",
            PaletteScope::Files => "@",
        }
    }
}

/// Detects the "type the prefix character to switch scope" gesture: `>` (what a US keyboard
/// types for `Shift`+`.`; the typographic `›` is also accepted in case it's pasted) switches to
/// [`PaletteScope::Commands`], `@` switches to [`PaletteScope::Files`]. Pure and stateless - the
/// caller (`crate::root::AdeApp::handle_palette_key_down`) is responsible for only consulting
/// this for the first character typed into an otherwise-empty query, and for not appending the
/// consumed character to the query itself.
pub fn typed_scope_prefix(ch: char) -> Option<PaletteScope> {
    match ch {
        '>' | '\u{203A}' => Some(PaletteScope::Commands),
        '@' => Some(PaletteScope::Files),
        _ => None,
    }
}

/// Which step of the palette is showing. The palette is normally a flat list ([`Self::Root`]);
/// a step is the one real drill-down shape it has, entered by running a command that needs an
/// argument rather than doing something immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteStep {
    #[default]
    Root,
    /// [`PaletteCommand::RestartLanguageServer`]'s "which server?" step - lists the real live
    /// clients `crate::lsp::client::AdeApp::restartable_language_servers` reports, and running a
    /// row restarts exactly that one.
    PickLanguageServer,
}

/// An already-open agent, reduced to what a palette row needs - built from the same live
/// `crate::work_surface::agents::Agents` list the rail (`crate::rail::state::AgentRow`) renders.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCandidate {
    pub id: AgentId,
    pub kind: ProcessKind,
    pub title: String,
    pub branch: Option<String>,
    pub status: Status,
}

/// A fixed command this app can perform right now - every variant maps one-to-one to an
/// existing `crate::root::AdeApp` method (see `crate::root::AdeApp::execute_palette_command`),
/// never a stub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    /// `crate::root::AdeApp::new_agent(ProcessKind::Shell, ..)`, same as the rail's `+`/⌘N.
    NewShell,
    /// `crate::root::AdeApp::new_agent(ProcessKind::claude(), ..)`.
    NewClaudeAgent,
    /// `crate::root::AdeApp::new_agent(ProcessKind::codex(), ..)`.
    NewCodexAgent,
    /// `crate::root::AdeApp::set_right_sidebar_view`, same as the `Files - Search - Changes`
    /// control.
    CycleRightPanel,
    /// `crate::root::AdeApp::request_prune` - goes through the same two-click confirmation gate
    /// as the rail footer's own `prune` button, never bypassing it.
    PruneWorktrees,
    /// `crate::root::AdeApp::open_settings`.
    OpenSettings,
    /// `crate::root::AdeApp::restart_lsp_clients` - the real, discoverable recovery for a
    /// language server that has died (see that method's own docs for why recovery is a user
    /// action here rather than an automatic respawn). Deliberately always listed, not hidden
    /// until something is broken: a user whose diagnostics have gone quiet is looking for this
    /// *before* they know which server died, and a command that appears only once the app has
    /// already diagnosed the problem is not a recovery path they can find.
    RestartLanguageServers,
    /// The single-server half of the same recovery: instead of throwing away *every* server for
    /// the worktree, this asks which one - `crate::root::AdeApp::begin_language_server_pick`
    /// moves the palette into [`PaletteStep::PickLanguageServer`], listing the real live clients,
    /// and picking a row runs `crate::lsp::client::AdeApp::restart_lsp_client` for exactly that
    /// one. With only one server actually running there is no choice to make, so it restarts that
    /// one immediately rather than showing a one-row menu; with none running it isn't listed at
    /// all (the same "never list a command that would silently do nothing" rule
    /// `crate::root::AdeApp::build_palette_groups` already applies to `OpenGitGraph`).
    RestartLanguageServer,
    /// Pins `crate::keymap::WindowControlsStyle::System`. These three variants and the
    /// Settings "General" page's `Window controls` row both call
    /// `crate::root::AdeApp::set_window_controls_style`, which mutates and persists the same
    /// setting - two entry points, one real write, never a second independent copy.
    WindowControlsSystem,
    /// Pins `crate::keymap::WindowControlsStyle::MacosStyle` - see [`Self::WindowControlsSystem`].
    WindowControlsMacos,
    /// Pins `crate::keymap::WindowControlsStyle::WindowsLinuxStyle` - see
    /// [`Self::WindowControlsSystem`].
    WindowControlsWindowsLinux,
    /// `crate::graph_view::render::AdeApp::open_git_graph` - the palette's own "Git" group
    /// (design spec §6). Rendered under a dedicated `"Git"` group label rather than the plain
    /// `"Commands"` one - see [`build_groups`]'s own `is_git_command` split.
    OpenGitGraph,
    /// `crate::updater::flow::AdeApp::check_for_update` (GitHub issue #87) - the real, manual
    /// trigger alongside the startup check and the periodic background loop
    /// (`crate::updater::flow::AdeApp::start_update_check_loop`). Deliberately always listed,
    /// not hidden until an update happens to be available, matching
    /// [`Self::RestartLanguageServers`]'s own "a discoverable recovery/utility action must be
    /// findable before the user already knows something's up" reasoning - a user who suspects
    /// they're on an old build can check right now rather than wait for the next periodic tick.
    CheckForUpdates,
}

impl PaletteCommand {
    pub const ALL: [PaletteCommand; 13] = [
        PaletteCommand::NewShell,
        PaletteCommand::NewClaudeAgent,
        PaletteCommand::NewCodexAgent,
        PaletteCommand::CycleRightPanel,
        PaletteCommand::PruneWorktrees,
        PaletteCommand::OpenSettings,
        PaletteCommand::RestartLanguageServers,
        PaletteCommand::RestartLanguageServer,
        PaletteCommand::WindowControlsSystem,
        PaletteCommand::WindowControlsMacos,
        PaletteCommand::WindowControlsWindowsLinux,
        PaletteCommand::OpenGitGraph,
        PaletteCommand::CheckForUpdates,
    ];

    /// Whether this command belongs in the palette's `"Git"` group rather than `"Commands"` -
    /// see [`build_groups`].
    pub fn is_git_command(self) -> bool {
        matches!(self, PaletteCommand::OpenGitGraph)
    }

    pub fn label(self) -> &'static str {
        match self {
            PaletteCommand::NewShell => "New Shell",
            PaletteCommand::NewClaudeAgent => "New Claude Agent",
            PaletteCommand::NewCodexAgent => "New Codex Agent",
            PaletteCommand::CycleRightPanel => "Cycle Right Panel",
            PaletteCommand::PruneWorktrees => "Prune Worktrees",
            PaletteCommand::OpenSettings => "Open Settings",
            PaletteCommand::RestartLanguageServers => "Restart Language Servers",
            PaletteCommand::RestartLanguageServer => "Restart Language Server\u{2026}",
            PaletteCommand::WindowControlsSystem => "Window Controls: System Default",
            PaletteCommand::WindowControlsMacos => "Window Controls: macOS Style",
            PaletteCommand::WindowControlsWindowsLinux => "Window Controls: Windows/Linux Style",
            PaletteCommand::OpenGitGraph => "Open Git Graph",
            PaletteCommand::CheckForUpdates => "Check for Updates",
        }
    }

    /// Extra search terms beyond [`Self::label`] - matched but never highlighted (see
    /// [`match_against`]), so e.g. typing "terminal" still finds "New Shell".
    fn keywords(self) -> &'static str {
        match self {
            PaletteCommand::NewShell => "shell terminal spawn agent",
            PaletteCommand::NewClaudeAgent => "claude agent spawn agent cli",
            PaletteCommand::NewCodexAgent => "codex agent spawn agent cli",
            PaletteCommand::CycleRightPanel => {
                "files search changes find panel sidebar switch toggle tab"
            }
            PaletteCommand::PruneWorktrees => "prune worktree remove delete cleanup merged",
            PaletteCommand::OpenSettings => "settings preferences agents worktrees config",
            PaletteCommand::RestartLanguageServers => {
                "lsp language server restart reconnect reload crashed died dead disconnected \
                 hung stuck diagnostics hover completions rust-analyzer typescript pyright vue"
            }
            PaletteCommand::RestartLanguageServer => {
                "lsp language server restart reconnect reload crashed died dead disconnected \
                 hung stuck one single pick choose which rust-analyzer typescript pyright vue"
            }
            PaletteCommand::WindowControlsSystem => {
                "window controls title bar caption buttons dots platform override reset"
            }
            PaletteCommand::WindowControlsMacos => {
                "window controls title bar dots traffic lights keycap platform override macos"
            }
            PaletteCommand::WindowControlsWindowsLinux => {
                "window controls title bar caption buttons menu keycap platform override windows linux"
            }
            PaletteCommand::OpenGitGraph => "git graph commit history branches log",
            PaletteCommand::CheckForUpdates => "update version release new github check",
        }
    }

    /// The bound keyboard shortcut for this command, if any - a `crate::keymap::resolve_combo`
    /// spec string, not an already-resolved glyph, so `render_palette_row` renders `⌘N`/`⌘,` on
    /// macOS and `Ctrl N`/`Ctrl ,` elsewhere rather than a hardcoded platform literal. Every
    /// other command has no dedicated shortcut, so this returns `None` for them rather than
    /// showing a keycap that would silently do nothing if pressed.
    pub fn shortcut(self) -> Option<&'static str> {
        match self {
            PaletteCommand::NewShell => Some("mod+N"),
            PaletteCommand::OpenSettings => Some("mod+,"),
            PaletteCommand::OpenGitGraph => Some("mod+shift+G"),
            _ => None,
        }
    }
}

/// A command candidate paired with a live secondary/description line - filled in by
/// `crate::root::AdeApp::build_palette_groups` from current app state (e.g.
/// [`PaletteCommand::PruneWorktrees`]'s prunable count), so this shows the same live numbers
/// the rail footer does.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandCandidate {
    pub command: PaletteCommand,
    pub secondary: String,
}

/// One real, live language server client under the active worktree root - a
/// [`PaletteStep::PickLanguageServer`] row, built by `crate::root::AdeApp::build_palette_groups`
/// from `crate::lsp::client::AdeApp::restartable_language_servers`.
#[derive(Debug, Clone, PartialEq)]
pub struct LanguageServerCandidate {
    /// The `crate::root::AdeApp::lsp_clients` key's own binary half - what
    /// `crate::lsp::client::AdeApp::restart_lsp_client` is called with.
    pub client_key: &'static str,
    /// Language plus real live state, e.g. `"Rust \u{b7} ready"` or the failure's own text.
    pub secondary: String,
    /// Extra search terms (the language's registry display name) - matched, never highlighted,
    /// exactly like [`PaletteCommand::keywords`].
    pub keywords: String,
    /// The rail's status colour for this server's real state, reused verbatim the same way an
    /// agent row reuses it: [`Status::Run`] for a ready client, [`Status::Fail`] for a failed one.
    pub status: Status,
}

/// Whether a file result was added or deleted in the currently loaded diff - the palette row's
/// optional status dot. `None` for a modified/renamed/unchanged file (no dot colour is defined
/// for those).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Deleted,
}

/// A file from the already-loaded file tree (`crate::sidebar::file_tree::build_file_tree`), reduced to
/// what a palette row needs, with `add`/`del`/[`FileChangeKind`] merged in from the currently
/// loaded diff where the file has one (`0`/`None` otherwise).
#[derive(Debug, Clone, PartialEq)]
pub struct FileCandidate {
    pub path: PathBuf,
    pub name: String,
    /// Repo-relative directory, `""` for a root-level file - mirrors
    /// `crate::sidebar::changes::split_dir_name`'s `dir` shape.
    pub dir: String,
    pub add: u32,
    pub del: u32,
    pub changed: Option<FileChangeKind>,
}

/// What running a selected [`PaletteEntry`] actually does.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryTarget {
    Command(PaletteCommand),
    Agent(AgentId),
    /// The real, absolute file-tree path (`crate::sidebar::file_tree::FileTreeEntry::path`) - not yet
    /// resolved to repo-relative; see `crate::root::AdeApp::open_palette_file_result`'s docs
    /// for how it decides between opening a real diff and revealing the file in the real tree.
    File(PathBuf),
    /// A [`PaletteStep::PickLanguageServer`] row: restart exactly this one live client
    /// (`crate::lsp::client::AdeApp::restart_lsp_client`), never the others running beside it.
    /// Carries only the client key - the root is always the active worktree's, since these rows
    /// are rebuilt from live state on every render and there is no way to select a row belonging
    /// to a root that has since stopped being active.
    LanguageServer(&'static str),
}

/// A label split around its matched substring, for `pre`/`mid`/`post` rendering (three adjacent
/// spans, the middle one tinted). `mid`/`post` are both empty when there was no match to
/// highlight (an empty query, or a match that only came through a secondary field - see
/// [`match_against`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedText {
    pub pre: String,
    pub mid: String,
    pub post: String,
}

impl MatchedText {
    fn plain(text: &str) -> Self {
        Self {
            pre: text.to_string(),
            mid: String::new(),
            post: String::new(),
        }
    }

    fn from_match(text: &str, span: Option<(usize, usize)>) -> Self {
        let Some((start, len)) = span else {
            return Self::plain(text);
        };
        let chars: Vec<char> = text.chars().collect();
        let start = start.min(chars.len());
        let end = (start + len).min(chars.len());
        Self {
            pre: chars[..start].iter().collect(),
            mid: chars[start..end].iter().collect(),
            post: chars[end..].iter().collect(),
        }
    }
}

/// One ready-to-run palette result row.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteEntry {
    pub label: MatchedText,
    pub secondary: String,
    pub shortcut: Option<&'static str>,
    /// The rail's status colour, reused verbatim so the palette inherits the rail's colour
    /// coding - set for an [`EntryTarget::Agent`] row (that agent's real status) and for an
    /// [`EntryTarget::LanguageServer`] row (see [`LanguageServerCandidate::status`]), `None` for
    /// everything else.
    pub status: Option<Status>,
    /// Only set for an [`EntryTarget::File`] row that is an add/delete in the loaded diff.
    pub file_change: Option<FileChangeKind>,
    /// Only set for an [`EntryTarget::Agent`] row - which agent badge/tint to draw.
    pub process_kind: Option<ProcessKind>,
    pub target: EntryTarget,
}

/// One result group - a section header plus its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteGroup {
    pub label: &'static str,
    pub entries: Vec<PaletteEntry>,
}

/// Flattens every group's entries into one ordered list, matching the palette's visual (and
/// keyboard-selectable) row order top to bottom - the single source of truth for what index
/// `palette_selected` refers to, so the rendered highlight and the `⏎`-run target can never
/// disagree about which row is "row N".
pub fn flatten(groups: &[PaletteGroup]) -> Vec<&PaletteEntry> {
    groups
        .iter()
        .flat_map(|group| group.entries.iter())
        .collect()
}

/// A position-preserving ASCII-fold of `text` (only ASCII letters are case-folded; every other
/// `char` passes through unchanged), so a matched span's `(char_index, char_len)` always indexes
/// back correctly into the *original* string. A full Unicode fold (`str::to_lowercase`) can
/// change a string's char count (e.g. `'İ'` folds to two chars), which would misalign that
/// mapping - not worth guarding against for this app's content, so the simpler, alignment-safe
/// ASCII fold is used instead.
fn ascii_fold(text: &str) -> Vec<char> {
    text.chars().map(|c| c.to_ascii_lowercase()).collect()
}

/// Leftmost case-insensitive substring search (see the module docs for why this is plain
/// substring matching, not fuzzy/skip-char). Returns `(start_char_index, len_in_chars)`, or
/// `None` if `needle` is empty or doesn't occur in `haystack`.
pub fn substring_match(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let haystack_lower = ascii_fold(haystack);
    let needle_lower = ascii_fold(needle);
    if needle_lower.len() > haystack_lower.len() {
        return None;
    }
    let last_start = haystack_lower.len() - needle_lower.len();
    for start in 0..=last_start {
        if haystack_lower[start..start + needle_lower.len()] == needle_lower[..] {
            return Some((start, needle_lower.len()));
        }
    }
    None
}

/// Whether `primary` or any of `aux` qualifies a candidate for `query`, and its sort rank -
/// `Some((highlight_span, rank))` if it matches at all. An empty `query` matches everything with
/// rank `0` and no highlight (the "browse everything" state). A match in `primary` is
/// highlighted and ranked by how early it starts (`0` is best). A match found only in `aux` (a
/// agent's branch, a file's directory, a command's keywords) still qualifies but has nothing
/// for the label to highlight, so it ranks last, at `usize::MAX`.
fn match_against(
    primary: &str,
    aux: &[&str],
    query: &str,
) -> Option<(Option<(usize, usize)>, usize)> {
    if query.is_empty() {
        return Some((None, 0));
    }
    if let Some(span) = substring_match(primary, query) {
        return Some((Some(span), span.0));
    }
    if aux
        .iter()
        .any(|text| substring_match(text, query).is_some())
    {
        return Some((None, usize::MAX));
    }
    None
}

fn finish_group(mut scored: Vec<(usize, PaletteEntry)>) -> Vec<PaletteEntry> {
    scored.sort_by_key(|(rank, _)| *rank);
    scored.truncate(MAX_ENTRIES_PER_GROUP);
    scored.into_iter().map(|(_, entry)| entry).collect()
}

fn filter_agents(agents: &[AgentCandidate], query: &str) -> Vec<PaletteEntry> {
    let mut scored = Vec::new();
    for candidate in agents {
        let branch = candidate.branch.as_deref().unwrap_or("");
        let aux = [branch, candidate.kind.label()];
        let Some((span, rank)) = match_against(&candidate.title, &aux, query) else {
            continue;
        };
        scored.push((
            rank,
            PaletteEntry {
                label: MatchedText::from_match(&candidate.title, span),
                secondary: candidate
                    .branch
                    .clone()
                    .unwrap_or_else(|| "(detached)".to_string()),
                shortcut: None,
                status: Some(candidate.status),
                file_change: None,
                process_kind: Some(candidate.kind),
                target: EntryTarget::Agent(candidate.id),
            },
        ));
    }
    finish_group(scored)
}

fn filter_commands(commands: &[CommandCandidate], query: &str) -> Vec<PaletteEntry> {
    let mut scored = Vec::new();
    for candidate in commands {
        let label = candidate.command.label();
        let aux = [candidate.command.keywords(), candidate.secondary.as_str()];
        let Some((span, rank)) = match_against(label, &aux, query) else {
            continue;
        };
        scored.push((
            rank,
            PaletteEntry {
                label: MatchedText::from_match(label, span),
                secondary: candidate.secondary.clone(),
                shortcut: candidate.command.shortcut(),
                status: None,
                file_change: None,
                process_kind: None,
                target: EntryTarget::Command(candidate.command),
            },
        ));
    }
    finish_group(scored)
}

fn file_secondary(candidate: &FileCandidate) -> String {
    let has_stat = candidate.changed.is_some() || candidate.add > 0 || candidate.del > 0;
    if has_stat {
        if candidate.dir.is_empty() {
            format!("+{} \u{2212}{}", candidate.add, candidate.del)
        } else {
            format!(
                "{}/ \u{b7} +{} \u{2212}{}",
                candidate.dir, candidate.add, candidate.del
            )
        }
    } else if candidate.dir.is_empty() {
        "(root)".to_string()
    } else {
        format!("{}/", candidate.dir)
    }
}

fn filter_files(files: &[FileCandidate], query: &str) -> Vec<PaletteEntry> {
    let mut scored = Vec::new();
    for candidate in files {
        let aux = [candidate.dir.as_str()];
        let Some((span, rank)) = match_against(&candidate.name, &aux, query) else {
            continue;
        };
        scored.push((
            rank,
            PaletteEntry {
                label: MatchedText::from_match(&candidate.name, span),
                secondary: file_secondary(candidate),
                shortcut: None,
                status: None,
                file_change: candidate.changed,
                process_kind: None,
                target: EntryTarget::File(candidate.path.clone()),
            },
        ));
    }
    finish_group(scored)
}

/// Builds the [`PaletteStep::PickLanguageServer`] step's single group - the same
/// filter/rank/highlight/cap pipeline every other group goes through ([`match_against`],
/// [`finish_group`]), so typing filters this list, `↑`/`↓` walk it and `⏎` runs it exactly like
/// the command list a keystroke ago.
pub fn build_language_server_groups(
    query: &str,
    servers: &[LanguageServerCandidate],
) -> Vec<PaletteGroup> {
    let mut scored = Vec::new();
    for candidate in servers {
        let aux = [candidate.keywords.as_str(), candidate.secondary.as_str()];
        let Some((span, rank)) = match_against(candidate.client_key, &aux, query) else {
            continue;
        };
        scored.push((
            rank,
            PaletteEntry {
                label: MatchedText::from_match(candidate.client_key, span),
                secondary: candidate.secondary.clone(),
                shortcut: None,
                status: Some(candidate.status),
                file_change: None,
                process_kind: None,
                target: EntryTarget::LanguageServer(candidate.client_key),
            },
        ));
    }
    let entries = finish_group(scored);
    if entries.is_empty() {
        return Vec::new();
    }
    vec![PaletteGroup {
        label: "Language Servers",
        entries,
    }]
}

/// Builds the palette's result groups for the current `scope`/`query`. Group order is always
/// Agents, Terminals, Commands, Files; a group with zero matches is omitted entirely rather than
/// shown as an empty header.
pub fn build_groups(
    scope: PaletteScope,
    query: &str,
    agents: &[AgentCandidate],
    commands: &[CommandCandidate],
    files: &[FileCandidate],
) -> Vec<PaletteGroup> {
    let mut groups = Vec::new();

    if scope == PaletteScope::All {
        let (sessions, shells): (Vec<_>, Vec<_>) = agents
            .iter()
            .cloned()
            .partition(|candidate| candidate.kind.is_agent_session());

        let entries = filter_agents(&sessions, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "Agents",
                entries,
            });
        }

        let entries = filter_agents(&shells, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "Terminals",
                entries,
            });
        }
    }

    if matches!(scope, PaletteScope::All | PaletteScope::Commands) {
        let (git_commands, plain_commands): (Vec<_>, Vec<_>) = commands
            .iter()
            .cloned()
            .partition(|candidate| candidate.command.is_git_command());

        let entries = filter_commands(&plain_commands, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "Commands",
                entries,
            });
        }

        // Design spec §6: a dedicated "Git" group, separate from the plain "Commands" one.
        let entries = filter_commands(&git_commands, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "Git",
                entries,
            });
        }
    }

    if matches!(scope, PaletteScope::All | PaletteScope::Files) {
        let (label, entries) = if query.is_empty() {
            let recent: Vec<FileCandidate> = files
                .iter()
                .filter(|file| file.changed.is_some())
                .cloned()
                .collect();
            ("Recent Files", filter_files(&recent, query))
        } else {
            ("Files", filter_files(files, query))
        };
        if !entries.is_empty() {
            groups.push(PaletteGroup { label, entries });
        }
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_cycles_all_commands_files_all() {
        assert_eq!(PaletteScope::default(), PaletteScope::All);
        assert_eq!(PaletteScope::All.cycle(), PaletteScope::Commands);
        assert_eq!(PaletteScope::Commands.cycle(), PaletteScope::Files);
        assert_eq!(PaletteScope::Files.cycle(), PaletteScope::All);
    }

    #[test]
    fn typed_scope_prefix_recognizes_gt_and_at_only() {
        assert_eq!(typed_scope_prefix('>'), Some(PaletteScope::Commands));
        assert_eq!(typed_scope_prefix('\u{203A}'), Some(PaletteScope::Commands));
        assert_eq!(typed_scope_prefix('@'), Some(PaletteScope::Files));
        assert_eq!(typed_scope_prefix('a'), None);
        assert_eq!(typed_scope_prefix(' '), None);
    }

    #[test]
    fn substring_match_finds_leftmost_case_insensitive_span() {
        assert_eq!(substring_match("query_builder.rs", "quer"), Some((0, 4)));
        assert_eq!(substring_match("query_builder.rs", "QUER"), Some((0, 4)));
        assert_eq!(
            substring_match("legacy_query.rs", "quer"),
            Some((7, 4)),
            "matches mid-string, not just a prefix"
        );
        assert_eq!(substring_match("Cargo.toml", "arg"), Some((1, 3)));
    }

    #[test]
    fn substring_match_is_none_for_empty_needle_or_no_match() {
        assert_eq!(substring_match("anything", ""), None);
        assert_eq!(substring_match("short", "much too long"), None);
        assert_eq!(substring_match("query.rs", "zzz"), None);
    }

    #[test]
    fn matched_text_splits_around_a_real_span() {
        let matched = MatchedText::from_match("query_builder.rs", Some((0, 4)));
        assert_eq!(matched.pre, "");
        assert_eq!(matched.mid, "quer");
        assert_eq!(matched.post, "y_builder.rs");
    }

    #[test]
    fn matched_text_is_plain_when_there_is_no_span() {
        let matched = MatchedText::from_match("New Shell", None);
        assert_eq!(matched.pre, "New Shell");
        assert_eq!(matched.mid, "");
        assert_eq!(matched.post, "");
    }

    #[test]
    fn match_against_empty_query_matches_everything_unranked_and_unhighlighted() {
        let (span, rank) = match_against("New Shell", &["shell terminal"], "").expect("matches");
        assert_eq!(span, None);
        assert_eq!(rank, 0);
    }

    #[test]
    fn match_against_primary_match_is_ranked_by_start_position_and_highlighted() {
        let (span, rank) =
            match_against("Prune Worktrees", &["prune worktree"], "wor").expect("matches");
        assert_eq!(span, Some((6, 3)));
        assert_eq!(rank, 6);
    }

    #[test]
    fn match_against_aux_only_match_still_qualifies_but_is_unranked_and_unhighlighted() {
        let (span, rank) =
            match_against("New Shell", &["shell terminal spawn"], "terminal").expect("matches");
        assert_eq!(
            span, None,
            "the primary label itself has no 'terminal' span"
        );
        assert_eq!(rank, usize::MAX, "ranked after any primary-label match");
    }

    #[test]
    fn match_against_no_match_anywhere_is_none() {
        assert_eq!(match_against("New Shell", &["shell terminal"], "zzz"), None);
    }

    #[test]
    fn every_documented_command_is_present_exactly_once() {
        let labels: Vec<&str> = PaletteCommand::ALL.iter().map(|c| c.label()).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "no duplicate commands");
        assert!(labels.contains(&"New Shell"));
        assert!(labels.contains(&"New Claude Agent"));
        assert!(labels.contains(&"New Codex Agent"));
        assert!(labels.contains(&"Prune Worktrees"));
        assert!(labels.contains(&"Open Settings"));
    }

    #[test]
    fn only_commands_with_a_real_global_keybinding_carry_a_shortcut() {
        for command in PaletteCommand::ALL {
            match command {
                PaletteCommand::NewShell => {
                    assert_eq!(command.shortcut(), Some("mod+N"))
                }
                PaletteCommand::OpenSettings => {
                    assert_eq!(command.shortcut(), Some("mod+,"))
                }
                PaletteCommand::OpenGitGraph => {
                    assert_eq!(command.shortcut(), Some("mod+shift+G"))
                }
                _ => assert_eq!(
                    command.shortcut(),
                    None,
                    "{command:?} has no real global keybinding, so it must not show one"
                ),
            }
        }
    }

    fn agent(id: AgentId, title: &str, branch: Option<&str>, status: Status) -> AgentCandidate {
        AgentCandidate {
            id,
            kind: ProcessKind::claude(),
            title: title.to_string(),
            branch: branch.map(str::to_string),
            status,
        }
    }

    fn command_candidate(command: PaletteCommand) -> CommandCandidate {
        CommandCandidate {
            command,
            secondary: "secondary".to_string(),
        }
    }

    fn file(path: &str, changed: Option<FileChangeKind>) -> FileCandidate {
        let path_buf = PathBuf::from(path);
        let name = path_buf
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = path_buf
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        FileCandidate {
            path: path_buf,
            name,
            dir,
            add: if changed.is_some() { 5 } else { 0 },
            del: 0,
            changed,
        }
    }

    #[test]
    fn empty_query_all_scope_shows_agents_commands_and_recent_files_together() {
        let agents = vec![agent(1, "Fix rate limiter", Some("fix/rl"), Status::Run)];
        let commands: Vec<CommandCandidate> = PaletteCommand::ALL
            .into_iter()
            .map(command_candidate)
            .collect();
        let files = vec![
            file("src/db/query_builder.rs", Some(FileChangeKind::Added)),
            file("src/unchanged.rs", None),
        ];

        let groups = build_groups(PaletteScope::All, "", &agents, &commands, &files);

        let labels: Vec<&str> = groups.iter().map(|g| g.label).collect();
        assert_eq!(
            labels,
            vec!["Agents", "Commands", "Git", "Recent Files"],
            "real order: Agents, Commands, Git, Files - Git is `OpenGitGraph`'s own group \
             (design spec §6), split out of plain Commands"
        );
        let files_group = groups.iter().find(|g| g.label == "Recent Files").unwrap();
        assert_eq!(
            files_group.entries.len(),
            1,
            "only the real changed file counts as 'recent', not the unchanged one"
        );
    }

    #[test]
    fn commands_scope_shows_only_matching_commands_and_highlights_the_label() {
        let commands: Vec<CommandCandidate> = PaletteCommand::ALL
            .into_iter()
            .map(command_candidate)
            .collect();

        let groups = build_groups(PaletteScope::Commands, "wor", &[], &commands, &[]);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "Commands");
        let prune = groups[0]
            .entries
            .iter()
            .find(|e| e.target == EntryTarget::Command(PaletteCommand::PruneWorktrees))
            .expect("Prune Worktrees matches 'wor'");
        assert_eq!(prune.label.mid, "Wor");
    }

    #[test]
    fn files_scope_with_a_query_searches_every_file_not_just_changed_ones() {
        let files = vec![
            file("src/db/query_builder.rs", Some(FileChangeKind::Added)),
            file("src/db/query_cache.rs", None),
        ];

        let groups = build_groups(PaletteScope::Files, "quer", &[], &[], &files);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].label, "Files",
            "not the empty-query 'Recent Files' label"
        );
        assert_eq!(
            groups[0].entries.len(),
            2,
            "an unchanged file must still be a real, searchable result under an explicit query"
        );
    }

    #[test]
    fn agents_never_appear_outside_all_scope() {
        let agents = vec![agent(1, "Fix rate limiter", None, Status::Run)];
        let groups = build_groups(PaletteScope::Commands, "", &agents, &[], &[]);
        assert!(groups.iter().all(|g| g.label != "Agents"));
        let groups = build_groups(PaletteScope::Files, "", &agents, &[], &[]);
        assert!(groups.iter().all(|g| g.label != "Agents"));
    }

    #[test]
    fn a_query_matching_nothing_yields_no_groups_at_all() {
        let agents = vec![agent(1, "Fix rate limiter", None, Status::Run)];
        let commands: Vec<CommandCandidate> = PaletteCommand::ALL
            .into_iter()
            .map(command_candidate)
            .collect();
        let files = vec![file("src/main.rs", None)];

        let groups = build_groups(
            PaletteScope::All,
            "zzz_no_such_thing",
            &agents,
            &commands,
            &files,
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn group_entries_are_capped_at_max_per_group() {
        let files: Vec<FileCandidate> = (0..20)
            .map(|i| file(&format!("src/mod{i}/target.rs", i = i), None))
            .collect();
        let groups = build_groups(PaletteScope::Files, "target", &[], &[], &files);
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0].entries.len() <= MAX_ENTRIES_PER_GROUP,
            "defensively capped even when far more real files matched"
        );
    }

    #[test]
    fn group_entries_in_the_same_group_are_ranked_by_earliest_match_offset() {
        // "logger.rs" matches "log" at offset 0; "my_logger.rs" matches it at offset 3. Passed
        // in reverse-of-expected order so a pass-through (no real sort) would fail this.
        let files = vec![file("src/my_logger.rs", None), file("src/logger.rs", None)];

        let groups = build_groups(PaletteScope::Files, "log", &[], &[], &files);

        assert_eq!(groups.len(), 1);
        let names: Vec<String> = groups[0]
            .entries
            .iter()
            .map(|entry| format!("{}{}{}", entry.label.pre, entry.label.mid, entry.label.post))
            .collect();
        assert_eq!(
            names,
            vec!["logger.rs", "my_logger.rs"],
            "an earlier substring match (offset 0) should rank ahead of a later one (offset 3)"
        );
    }

    fn server(
        client_key: &'static str,
        secondary: &str,
        keywords: &str,
        status: Status,
    ) -> LanguageServerCandidate {
        LanguageServerCandidate {
            client_key,
            secondary: secondary.to_string(),
            keywords: keywords.to_string(),
            status,
        }
    }

    #[test]
    fn the_language_server_step_lists_every_running_server_as_a_real_restart_target() {
        let servers = vec![
            server("rust-analyzer", "Rust \u{b7} ready", "Rust", Status::Run),
            server(
                "typescript-language-server",
                "TypeScript \u{b7} connection lost",
                "TypeScript",
                Status::Fail,
            ),
        ];

        let groups = build_language_server_groups("", &servers);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "Language Servers");
        let targets: Vec<&EntryTarget> = groups[0].entries.iter().map(|e| &e.target).collect();
        assert_eq!(
            targets,
            vec![
                &EntryTarget::LanguageServer("rust-analyzer"),
                &EntryTarget::LanguageServer("typescript-language-server"),
            ],
            "each row must carry the real client key its restart is keyed by"
        );
        assert_eq!(
            groups[0].entries[1].secondary, "TypeScript \u{b7} connection lost",
            "the row shows the server's own real state, not a generic label"
        );
        assert_eq!(
            groups[0].entries[1].status,
            Some(Status::Fail),
            "a failed server carries the rail's failure colour, so the broken one is the one \
             that stands out in a list a user is picking from"
        );
    }

    #[test]
    fn typing_filters_the_language_server_step_and_highlights_the_match() {
        let servers = vec![
            server("rust-analyzer", "Rust \u{b7} ready", "Rust", Status::Run),
            server(
                "typescript-language-server",
                "TypeScript \u{b7} ready",
                "TypeScript",
                Status::Run,
            ),
        ];

        let groups = build_language_server_groups("analyzer", &servers);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].entries.len(), 1);
        assert_eq!(groups[0].entries[0].label.mid, "analyzer");
        assert_eq!(
            groups[0].entries[0].target,
            EntryTarget::LanguageServer("rust-analyzer")
        );

        assert!(
            build_language_server_groups("zzz", &servers).is_empty(),
            "a query matching no server yields no group at all, so the palette shows its own \
             'no results' state rather than an empty header"
        );
    }

    #[test]
    fn a_language_name_matches_through_keywords_without_collapsing_two_real_servers() {
        let servers = vec![
            server(
                "vue-language-server",
                "Vue \u{b7} ready",
                "Vue",
                Status::Run,
            ),
            server(
                "typescript-language-server (vue)",
                "Vue companion \u{b7} ready",
                "Vue companion",
                Status::Run,
            ),
        ];

        let groups = build_language_server_groups("vue", &servers);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].entries.len(),
            2,
            "both real processes stay pickable - restarting one must not be a choice the palette \
             quietly merges into the other"
        );
        let targets: Vec<&EntryTarget> = groups[0].entries.iter().map(|e| &e.target).collect();
        assert!(targets.contains(&&EntryTarget::LanguageServer("vue-language-server")));
        assert!(targets.contains(&&EntryTarget::LanguageServer(
            "typescript-language-server (vue)"
        )));
    }

    #[test]
    fn restart_one_and_restart_all_are_two_distinct_commands_with_their_own_labels() {
        assert_ne!(
            PaletteCommand::RestartLanguageServer,
            PaletteCommand::RestartLanguageServers,
            "the bulk recovery and the single-server one are genuinely different actions"
        );
        assert_ne!(
            PaletteCommand::RestartLanguageServer.label(),
            PaletteCommand::RestartLanguageServers.label(),
            "two different actions must not read as the same command in a search result"
        );
    }

    #[test]
    fn flatten_preserves_group_then_row_order() {
        let groups = vec![
            PaletteGroup {
                label: "Agents",
                entries: vec![PaletteEntry {
                    label: MatchedText::plain("s1"),
                    secondary: String::new(),
                    shortcut: None,
                    status: None,
                    file_change: None,
                    process_kind: None,
                    target: EntryTarget::Agent(1),
                }],
            },
            PaletteGroup {
                label: "Commands",
                entries: vec![PaletteEntry {
                    label: MatchedText::plain("c1"),
                    secondary: String::new(),
                    shortcut: None,
                    status: None,
                    file_change: None,
                    process_kind: None,
                    target: EntryTarget::Command(PaletteCommand::NewShell),
                }],
            },
        ];
        let flat = flatten(&groups);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].target, EntryTarget::Agent(1));
        assert_eq!(
            flat[1].target,
            EntryTarget::Command(PaletteCommand::NewShell)
        );
    }

    #[test]
    fn a_shell_is_listed_under_terminals_never_under_agents() {
        let candidates = vec![
            agent(1, "Fix rate limiter", Some("fix/rl"), Status::Run),
            AgentCandidate {
                id: 2,
                kind: ProcessKind::Shell,
                title: "scratch".to_string(),
                branch: Some("fix/rl".to_string()),
                status: Status::Idle,
            },
        ];

        let groups = build_groups(PaletteScope::All, "", &candidates, &[], &[]);

        let labels: Vec<&str> = groups.iter().map(|g| g.label).collect();
        assert_eq!(
            labels,
            vec!["Agents", "Terminals"],
            "two separate groups, agents first"
        );
        let agents_group = groups.iter().find(|g| g.label == "Agents").unwrap();
        assert_eq!(
            agents_group
                .entries
                .iter()
                .map(|entry| entry.target.clone())
                .collect::<Vec<_>>(),
            vec![EntryTarget::Agent(1)],
            "only the real agent session may be filed under `Agents`"
        );
        let terminals_group = groups.iter().find(|g| g.label == "Terminals").unwrap();
        assert_eq!(
            terminals_group
                .entries
                .iter()
                .map(|entry| entry.target.clone())
                .collect::<Vec<_>>(),
            vec![EntryTarget::Agent(2)],
            "the shell is still a real, selectable row - just not an agent"
        );
    }

    #[test]
    fn the_terminals_group_is_absent_when_no_shell_is_open() {
        let candidates = vec![agent(1, "Fix rate limiter", Some("fix/rl"), Status::Run)];
        let groups = build_groups(PaletteScope::All, "", &candidates, &[], &[]);
        assert_eq!(
            groups.iter().map(|g| g.label).collect::<Vec<_>>(),
            vec!["Agents"]
        );
    }

    #[test]
    fn agent_matches_via_branch_still_qualifies_without_highlighting_the_title() {
        let agents = vec![agent(
            1,
            "Fix rate limiter",
            Some("fix/auth-token-race"),
            Status::Run,
        )];
        let groups = build_groups(PaletteScope::All, "token", &agents, &[], &[]);
        let agents_group = groups.iter().find(|g| g.label == "Agents").unwrap();
        assert_eq!(agents_group.entries.len(), 1);
        assert_eq!(agents_group.entries[0].label.mid, "");
        assert_eq!(agents_group.entries[0].label.pre, "Fix rate limiter");
    }
}
