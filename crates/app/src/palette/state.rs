//! Pure, GPUI-free data model for the command palette (⌘P): scope/matching/ranking/grouping
//! over already-real app state (open agents, the loaded file tree, a fixed command list),
//! kept unit-testable without a live GPUI window. `crate::palette::render` turns the
//! result into `gpui::Div` trees and real click/key handlers, since it owns the `Context<AdeApp>`
//! those need. Every [`PaletteCommand`] variant maps one-to-one onto an existing `AdeApp` method
//! (see `crate::root::AdeApp::execute_palette_command`) - none is a stub.
//!
//! Matching is a plain, deterministic, case-insensitive (ASCII-fold) leftmost substring search
//! ([`substring_match`]), not fuzzy/skip-char matching, so a match highlights one contiguous
//! span per row rather than scattered characters. Results rank by how early the match starts;
//! an entry that only matched via a secondary field (an agent's branch, a file's directory, a
//! command's keywords) still qualifies but ranks after every primary-label match - see
//! [`match_against`].
//!
//! ## The History group (Revision R10)
//!
//! An earlier revision investigated the changelog's originally-proposed `Undo — keep all
//! changes`/`Redo — discard worktree` palette entries and found this app had no real backing
//! for either at the time: the only real commit path
//! (`crate::root::AdeApp::complete_merge_flow`) only finalizes an already-running merge attempt,
//! and the only real worktree-removal path (`crate::root::AdeApp::execute_prune`) always removes
//! every prunable worktree at once and explicitly excludes any worktree with a live agent -
//! neither matches "act on this one arbitrary agent, cold, in one click". That revision left
//! the History group out entirely rather than half-build it against those mismatched
//! primitives, deferring it to this one.
//!
//! Revision R10 built the real primitives instead: `wt_core::undo::commit_all_changes` (a real,
//! undoable "keep all changes" that can commit an arbitrary dirty worktree cold) and
//! `wt_core::undo::discard_worktree` (a real, undoable worktree removal that snapshots
//! uncommitted/untracked content into a real git stash first, unlike a bare
//! `wt_core::remove_worktree(force: true)`). [`HistoryCandidate`]/[`HistoryDirection`] are this
//! group's real data model over `crate::worktree_history::undo::UndoStack`'s live undo/redo cursor - see
//! [`HistoryCandidate`]'s own docs for why it's at most two rows, never a full log.

use std::path::PathBuf;

use crate::rail::status::Status;
use crate::work_surface::agents::{AgentId, AgentKind};

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

/// An already-open agent, reduced to what a palette row needs - built from the same live
/// `crate::work_surface::agents::Agents` list the rail (`crate::rail::state::AgentRow`) renders.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCandidate {
    pub id: AgentId,
    pub kind: AgentKind,
    pub title: String,
    pub branch: Option<String>,
    pub status: Status,
}

/// A fixed command this app can perform right now - every variant maps one-to-one to an
/// existing `crate::root::AdeApp` method (see `crate::root::AdeApp::execute_palette_command`),
/// never a stub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    /// `crate::root::AdeApp::new_agent(AgentKind::Shell, ..)`, same as the rail's `+`/⌘N.
    NewShell,
    /// `crate::root::AdeApp::new_agent(AgentKind::Claude, ..)`.
    NewClaudeAgent,
    /// `crate::root::AdeApp::new_agent(AgentKind::Codex, ..)`.
    NewCodexAgent,
    /// `crate::root::AdeApp::set_right_sidebar_view`, same as the `Files | Changes` control.
    ToggleFilesChanges,
    /// `crate::root::AdeApp::request_prune` - goes through the same two-click confirmation gate
    /// as the rail footer's own `prune` button, never bypassing it.
    PruneWorktrees,
    /// `crate::root::AdeApp::open_settings`.
    OpenSettings,
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
}

impl PaletteCommand {
    pub const ALL: [PaletteCommand; 9] = [
        PaletteCommand::NewShell,
        PaletteCommand::NewClaudeAgent,
        PaletteCommand::NewCodexAgent,
        PaletteCommand::ToggleFilesChanges,
        PaletteCommand::PruneWorktrees,
        PaletteCommand::OpenSettings,
        PaletteCommand::WindowControlsSystem,
        PaletteCommand::WindowControlsMacos,
        PaletteCommand::WindowControlsWindowsLinux,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaletteCommand::NewShell => "New Shell",
            PaletteCommand::NewClaudeAgent => "New Claude Agent",
            PaletteCommand::NewCodexAgent => "New Codex Agent",
            PaletteCommand::ToggleFilesChanges => "Toggle Files / Changes",
            PaletteCommand::PruneWorktrees => "Prune Worktrees",
            PaletteCommand::OpenSettings => "Open Settings",
            PaletteCommand::WindowControlsSystem => "Window Controls: System Default",
            PaletteCommand::WindowControlsMacos => "Window Controls: macOS Style",
            PaletteCommand::WindowControlsWindowsLinux => "Window Controls: Windows/Linux Style",
        }
    }

    /// Extra search terms beyond [`Self::label`] - matched but never highlighted (see
    /// [`match_against`]), so e.g. typing "terminal" still finds "New Shell".
    fn keywords(self) -> &'static str {
        match self {
            PaletteCommand::NewShell => "shell terminal spawn agent",
            PaletteCommand::NewClaudeAgent => "claude agent spawn agent cli",
            PaletteCommand::NewCodexAgent => "codex agent spawn agent cli",
            PaletteCommand::ToggleFilesChanges => "files changes panel sidebar switch",
            PaletteCommand::PruneWorktrees => "prune worktree remove delete cleanup merged",
            PaletteCommand::OpenSettings => "settings preferences agents worktrees config",
            PaletteCommand::WindowControlsSystem => {
                "window controls title bar caption buttons dots platform override reset"
            }
            PaletteCommand::WindowControlsMacos => {
                "window controls title bar dots traffic lights keycap platform override macos"
            }
            PaletteCommand::WindowControlsWindowsLinux => {
                "window controls title bar caption buttons menu keycap platform override windows linux"
            }
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
    /// Runs `crate::root::AdeApp::perform_undo`/`perform_redo` (Revision R10) - see
    /// [`HistoryDirection`]'s own docs for why this is the only real, actionable pair the
    /// History group ever shows.
    History(HistoryDirection),
}

/// Which end of `crate::root::AdeApp`'s real `crate::worktree_history::undo::UndoStack` a History palette row
/// acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryDirection {
    Undo,
    Redo,
}

/// The one real undo action and/or the one real redo action currently available, if any - built
/// by `crate::root::AdeApp::build_palette_groups` from the live `crate::worktree_history::undo::UndoStack`'s own
/// [`crate::worktree_history::undo::UndoStack::peek_undo`]/[`crate::worktree_history::undo::UndoStack::peek_redo`]. Deliberately not
/// a full history *log*: a stack only ever has one real next-undo and one real next-redo action
/// at a time (undo/redo isn't random-access - acting on an older entry while a newer one still
/// stands is exactly what `wt_core::undo`'s own identity guards would refuse), so those are the
/// only two rows that could ever be genuinely clickable. Showing older entries as additional,
/// unclickable rows was considered and rejected: this app's own rule against a control that
/// looks actionable but silently does nothing extends to *rows* that look like every other real,
/// runnable palette entry but aren't.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryCandidate {
    pub direction: HistoryDirection,
    /// The action's own human-readable description (e.g. `"Kept all changes (my-branch)"`),
    /// straight off the live `crate::worktree_history::undo::UndoEntry` - never re-derived.
    pub description: String,
    /// The worktree this action affected (`crate::worktree_history::undo::UndoableAction::worktree_path`) - shown
    /// as this row's `secondary` line. Deliberately *not* `description` again: an audit found
    /// `secondary` set to a literal clone of `description`, so every History row visibly
    /// duplicated its own label text (e.g. `Undo — Kept all changes (feature-a)    Kept all
    /// changes (feature-a)`). The worktree path is real, distinct information `description`
    /// doesn't already carry (which only names the branch, already visible in the label).
    pub worktree_path: PathBuf,
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
    /// Only set for an [`EntryTarget::Agent`] row - the rail's status colour, reused verbatim
    /// so the palette inherits the rail's colour coding.
    pub status: Option<Status>,
    /// Only set for an [`EntryTarget::File`] row that is an add/delete in the loaded diff.
    pub file_change: Option<FileChangeKind>,
    /// Only set for an [`EntryTarget::Agent`] row - which agent badge/tint to draw.
    pub agent_kind: Option<AgentKind>,
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
                agent_kind: Some(candidate.kind),
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
                agent_kind: None,
                target: EntryTarget::Command(candidate.command),
            },
        ));
    }
    finish_group(scored)
}

/// Builds the History group's rows - at most two ([`HistoryCandidate::direction`]'s `Undo` and
/// `Redo` entries, if present at all) - see [`HistoryCandidate`]'s own docs for why more than
/// that is never shown.
fn filter_history(history: &[HistoryCandidate], query: &str) -> Vec<PaletteEntry> {
    let mut scored = Vec::new();
    for candidate in history {
        let verb = match candidate.direction {
            HistoryDirection::Undo => "Undo",
            HistoryDirection::Redo => "Redo",
        };
        let label = format!("{verb} \u{2014} {}", candidate.description);
        let aux = [candidate.description.as_str()];
        let Some((span, rank)) = match_against(&label, &aux, query) else {
            continue;
        };
        scored.push((
            rank,
            PaletteEntry {
                label: MatchedText::from_match(&label, span),
                secondary: candidate.worktree_path.display().to_string(),
                shortcut: Some(match candidate.direction {
                    HistoryDirection::Undo => "mod+Z",
                    HistoryDirection::Redo => "mod+shift+Z",
                }),
                status: None,
                file_change: None,
                agent_kind: None,
                target: EntryTarget::History(candidate.direction),
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
                agent_kind: None,
                target: EntryTarget::File(candidate.path.clone()),
            },
        ));
    }
    finish_group(scored)
}

/// Builds the palette's result groups for the current `scope`/`query`. Group order is always
/// Agents, Commands, History, Files; a group with zero matches is omitted entirely rather than
/// shown as an empty header.
///
/// Agents only appear in [`PaletteScope::All`] - there is no dedicated Agents segment in the
/// scope control. For an empty query in a scope that shows files, the file candidates are first
/// narrowed to changed files (`FileCandidate::changed.is_some()`) under a `"Recent Files"`
/// label: this app has no file-access/mtime history to rank true recency by, so "recent" is
/// defined as "currently has uncommitted changes" - the one recency-adjacent signal the data
/// model actually has. A non-empty query searches every file in the tree instead, under a plain
/// `"Files"` label.
///
/// `history` (Revision R10) shares [`PaletteScope::Commands`] with `commands` - see
/// [`HistoryCandidate`]'s own docs for why it's at most the two real Undo/Redo rows, never a
/// full log.
pub fn build_groups(
    scope: PaletteScope,
    query: &str,
    agents: &[AgentCandidate],
    commands: &[CommandCandidate],
    history: &[HistoryCandidate],
    files: &[FileCandidate],
) -> Vec<PaletteGroup> {
    let mut groups = Vec::new();

    if scope == PaletteScope::All {
        let entries = filter_agents(agents, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "Agents",
                entries,
            });
        }
    }

    if matches!(scope, PaletteScope::All | PaletteScope::Commands) {
        let entries = filter_commands(commands, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "Commands",
                entries,
            });
        }
    }

    if matches!(scope, PaletteScope::All | PaletteScope::Commands) {
        let entries = filter_history(history, query);
        if !entries.is_empty() {
            groups.push(PaletteGroup {
                label: "History",
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
    fn only_new_shell_and_open_settings_carry_a_real_bound_shortcut() {
        for command in PaletteCommand::ALL {
            match command {
                PaletteCommand::NewShell => {
                    assert_eq!(command.shortcut(), Some("mod+N"))
                }
                PaletteCommand::OpenSettings => {
                    assert_eq!(command.shortcut(), Some("mod+,"))
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
            kind: AgentKind::Claude,
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

        let groups = build_groups(PaletteScope::All, "", &agents, &commands, &[], &files);

        let labels: Vec<&str> = groups.iter().map(|g| g.label).collect();
        assert_eq!(
            labels,
            vec!["Agents", "Commands", "Recent Files"],
            "real order: Agents, Commands, Files - matching Jerry.dc.html's own fixture"
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

        let groups = build_groups(PaletteScope::Commands, "wor", &[], &commands, &[], &[]);

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

        let groups = build_groups(PaletteScope::Files, "quer", &[], &[], &[], &files);

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
        let groups = build_groups(PaletteScope::Commands, "", &agents, &[], &[], &[]);
        assert!(groups.iter().all(|g| g.label != "Agents"));
        let groups = build_groups(PaletteScope::Files, "", &agents, &[], &[], &[]);
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
            &[],
            &files,
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn group_entries_are_capped_at_max_per_group() {
        let files: Vec<FileCandidate> = (0..20)
            .map(|i| file(&format!("src/mod{i}/target.rs", i = i), None))
            .collect();
        let groups = build_groups(PaletteScope::Files, "target", &[], &[], &[], &files);
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0].entries.len() <= MAX_ENTRIES_PER_GROUP,
            "defensively capped even when far more real files matched"
        );
    }

    /// The cap test above can't catch a ranking bug: every one of its synthetic files matches
    /// `"target"` at the same offset. This uses two files matching at different offsets to
    /// assert `finish_group` actually sorts by rank rather than passthrough order.
    #[test]
    fn group_entries_in_the_same_group_are_ranked_by_earliest_match_offset() {
        // "logger.rs" matches "log" at offset 0; "my_logger.rs" matches it at offset 3. Passed
        // in reverse-of-expected order so a pass-through (no real sort) would fail this.
        let files = vec![file("src/my_logger.rs", None), file("src/logger.rs", None)];

        let groups = build_groups(PaletteScope::Files, "log", &[], &[], &[], &files);

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
                    agent_kind: None,
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
                    agent_kind: None,
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
    fn agent_matches_via_branch_still_qualifies_without_highlighting_the_title() {
        let agents = vec![agent(
            1,
            "Fix rate limiter",
            Some("fix/auth-token-race"),
            Status::Run,
        )];
        let groups = build_groups(PaletteScope::All, "token", &agents, &[], &[], &[]);
        let agents_group = groups.iter().find(|g| g.label == "Agents").unwrap();
        assert_eq!(agents_group.entries.len(), 1);
        assert_eq!(agents_group.entries[0].label.mid, "");
        assert_eq!(agents_group.entries[0].label.pre, "Fix rate limiter");
    }

    #[test]
    fn no_history_candidates_means_no_history_group_at_all() {
        let groups = build_groups(PaletteScope::All, "", &[], &[], &[], &[]);
        assert!(
            groups.iter().all(|g| g.label != "History"),
            "an empty undo stack must not render an empty History group header"
        );
    }

    #[test]
    fn history_group_shows_undo_and_redo_rows_with_the_real_direction_targets() {
        let history = vec![
            HistoryCandidate {
                direction: HistoryDirection::Undo,
                description: "Kept all changes (feature-a)".to_string(),
                worktree_path: PathBuf::from("/repo/feature-a"),
            },
            HistoryCandidate {
                direction: HistoryDirection::Redo,
                description: "Discarded worktree (feature-b)".to_string(),
                worktree_path: PathBuf::from("/repo/feature-b"),
            },
        ];
        let groups = build_groups(PaletteScope::All, "", &[], &[], &history, &[]);
        let group = groups
            .iter()
            .find(|g| g.label == "History")
            .expect("a History group should be present");
        assert_eq!(group.entries.len(), 2);

        assert_eq!(
            group.entries[0].target,
            EntryTarget::History(HistoryDirection::Undo)
        );
        assert_eq!(
            format!(
                "{}{}{}",
                group.entries[0].label.pre, group.entries[0].label.mid, group.entries[0].label.post
            ),
            "Undo \u{2014} Kept all changes (feature-a)"
        );
        // Regression coverage for a real render bug an audit caught: `secondary` used to be a
        // literal clone of the label's own description, so every row visibly duplicated its own
        // text. It must now carry real, distinct information (the affected worktree path).
        assert_eq!(group.entries[0].secondary, "/repo/feature-a");
        assert_ne!(
            group.entries[0].secondary, "Kept all changes (feature-a)",
            "secondary must not just duplicate the label's own description text"
        );

        assert_eq!(
            group.entries[1].target,
            EntryTarget::History(HistoryDirection::Redo)
        );
        assert_eq!(
            format!(
                "{}{}{}",
                group.entries[1].label.pre, group.entries[1].label.mid, group.entries[1].label.post
            ),
            "Redo \u{2014} Discarded worktree (feature-b)"
        );
        assert_eq!(group.entries[1].secondary, "/repo/feature-b");
    }

    #[test]
    fn history_group_only_appears_in_all_and_commands_scope_and_respects_the_query() {
        let history = vec![HistoryCandidate {
            direction: HistoryDirection::Undo,
            description: "Kept all changes (feature-a)".to_string(),
            worktree_path: PathBuf::from("/repo/feature-a"),
        }];

        let files_scope = build_groups(PaletteScope::Files, "", &[], &[], &history, &[]);
        assert!(
            files_scope.iter().all(|g| g.label != "History"),
            "Files scope must never show the History group, matching Commands' own scoping"
        );

        let matching = build_groups(PaletteScope::Commands, "feature-a", &[], &[], &history, &[]);
        assert!(matching.iter().any(|g| g.label == "History"));

        let non_matching = build_groups(
            PaletteScope::Commands,
            "zzz_no_such_thing",
            &[],
            &[],
            &history,
            &[],
        );
        assert!(non_matching.iter().all(|g| g.label != "History"));
    }
}
