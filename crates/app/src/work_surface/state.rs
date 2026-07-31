//! Pure logic for Zone 2 (work surface): tab strip, agent context bar, and the
//! CLI/terminal pane header/footer.
//!
//! Deliberately GPUI-free, mirroring `crate::rail::status`'s own split: this module only maps
//! already-known facts (a [`AgentKind`], a [`Status`], a `bool`) onto *which*
//! colours/labels/actions a Zone 2 element should show, so that mapping is directly
//! unit-testable without a live GPUI window. Turning these into actual `gpui::Div` trees (and
//! wiring click handlers) happens one layer up, in `crate::root`, which has the
//! `Context<AdeApp>` these decisions need to act on.

use std::path::PathBuf;

use gpui::Rgba;

use crate::rail::status::Status;
use crate::sidebar::file_tree::LangChip;
use crate::theme;
use crate::work_surface::agents::{AgentId, AgentKind};

/// One entry in a worktree's combined tab-strip order (GitHub issue #16) - either an agent tab
/// or a file tab. `Agents`/`crate::root::AdeApp::open_files` remain the real storage for
/// *existence* and all process/buffer behaviour (spawning, closing, PTY lifecycle, edit-buffer
/// state) - this only records where a tab sits *visually*, so the two groups can interleave in
/// one strip instead of always rendering as two rigid blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabRef {
    Agent(AgentId),
    File(PathBuf),
}

/// Reconciles a worktree's stored tab order (`crate::root::AdeApp::tab_order`) against what's
/// *actually* open right now: drops any entry that no longer exists (a closed agent, a closed
/// file tab), then appends anything open that isn't in `stored` yet (a freshly spawned agent,
/// a freshly opened file) in `agents_for_cwd`/`open_files`'s own order - the same "just append
/// at the end" position a brand new tab has always landed at.
///
/// Pure and idempotent: calling it again on its own output, with the same
/// `agents_for_cwd`/`open_files`, changes nothing. That's what lets
/// `crate::root::AdeApp::combined_tab_order` call this fresh on every render instead of caching
/// a mutated copy - only a real drag-drop (`crate::root::AdeApp::reorder_tab`) needs to persist a
/// changed `Vec<TabRef>` back into `tab_order`.
pub fn reconcile_tab_order(
    stored: &[TabRef],
    agents_for_cwd: &[AgentId],
    open_files: &[PathBuf],
) -> Vec<TabRef> {
    let mut order: Vec<TabRef> = stored
        .iter()
        .filter(|tab_ref| match tab_ref {
            TabRef::Agent(id) => agents_for_cwd.contains(id),
            TabRef::File(path) => open_files.contains(path),
        })
        .cloned()
        .collect();
    for id in agents_for_cwd {
        if !order
            .iter()
            .any(|tab_ref| matches!(tab_ref, TabRef::Agent(existing) if existing == id))
        {
            order.push(TabRef::Agent(*id));
        }
    }
    for path in open_files {
        if !order
            .iter()
            .any(|tab_ref| matches!(tab_ref, TabRef::File(existing) if existing == path))
        {
            order.push(TabRef::File(path.clone()));
        }
    }
    order
}

/// Moves `dragged` to sit immediately before `target` (or immediately after it, if
/// `insert_after`) in `order` - the real backing for the unified tab strip's drag-to-reorder
/// gesture (`crate::root::AdeApp::reorder_tab`), used regardless of whether `dragged`/`target`
/// are the same kind (`TabRef::Agent`/`TabRef::File`) or not, which is GitHub issue #16's real
/// "any tab can cross into either group" ask - there is no separate per-kind reorder function to
/// keep in sync. A no-op if either entry is missing from `order`, or if they're the same entry.
pub fn move_tab_order(
    order: &mut Vec<TabRef>,
    dragged: &TabRef,
    target: &TabRef,
    insert_after: bool,
) {
    if dragged == target {
        return;
    }
    let Some(from) = order.iter().position(|tab_ref| tab_ref == dragged) else {
        return;
    };
    if !order.iter().any(|tab_ref| tab_ref == target) {
        return;
    }
    let item = order.remove(from);
    let mut to = order
        .iter()
        .position(|tab_ref| tab_ref == target)
        .unwrap_or(order.len());
    if insert_after {
        to += 1;
    }
    order.insert(to, item);
}

/// Fully transparent - used for the "outline"/"ghost" button variants and an inactive tab's
/// background, so every button/tab can always call `.bg()`/`.border_color()` uniformly rather
/// than conditionally skipping the call (which would also shift the box model by the border's
/// width).
pub const TRANSPARENT: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// The agent tint `(fg, bg)` for an agent's badge/chip. [`AgentKind::Shell`] isn't an agent,
/// so it gets a neutral chip instead of an invented tint.
pub fn agent_tint(kind: AgentKind) -> (Rgba, Rgba) {
    match kind {
        AgentKind::Claude => (theme::agent::SONNET.0.into(), theme::agent::SONNET.1.into()),
        AgentKind::Codex => (theme::agent::CODEX.0.into(), theme::agent::CODEX.1.into()),
        AgentKind::Shell => (theme::text::DIM.into(), theme::surface::CHIP_NEUTRAL.into()),
    }
}

/// The agent badge's single-character initial.
pub fn agent_initial(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "C",
        AgentKind::Codex => "X",
        AgentKind::Shell => "$",
    }
}

/// Which of the tab strip's two chip shapes an agent's tab draws: agent CLI gets a `❯` glyph,
/// terminal gets the pane glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabChipKind {
    Cli,
    Term,
}

pub fn tab_chip_kind(kind: AgentKind) -> TabChipKind {
    match kind {
        AgentKind::Claude | AgentKind::Codex => TabChipKind::Cli,
        AgentKind::Shell => TabChipKind::Term,
    }
}

/// A tab chip's `(bg, fg)`, active or dimmed. Dimmed reuses [`theme::border::ZONE`] (the same
/// token an inactive tab's own underline uses) for `bg`, and [`theme::text::FAINTER`] for `fg`.
#[derive(Debug, Clone, Copy)]
pub struct ChipColors {
    pub bg: Rgba,
    pub fg: Rgba,
}

pub fn tab_chip_colors(kind: AgentKind, active: bool) -> ChipColors {
    if active {
        let (fg, bg) = agent_tint(kind);
        ChipColors { bg, fg }
    } else {
        ChipColors {
            bg: theme::border::ZONE.into(),
            fg: theme::text::FAINTER.into(),
        }
    }
}

/// A file tab's chip colours - the file's language chip when active, dimmed to the exact same
/// `bg`/`fg` [`tab_chip_colors`] dims an agent tab's chip to when inactive.
pub fn file_tab_chip_colors(lang: LangChip, active: bool) -> ChipColors {
    if active {
        ChipColors {
            bg: lang.bg,
            fg: lang.fg,
        }
    } else {
        ChipColors {
            bg: theme::border::ZONE.into(),
            fg: theme::text::FAINTER.into(),
        }
    }
}

/// A tab's own background/underline/label colour, active or inactive. The design's inactive
/// label colour (`#767d84`) has no exact token in `theme.rs`; [`theme::text::DIMMER`]
/// (`#7d848b`) is the closest ported token, used here rather than adding a new one-off constant.
#[derive(Debug, Clone, Copy)]
pub struct TabColors {
    pub bg: Rgba,
    pub underline: Rgba,
    pub label: Rgba,
}

pub fn tab_colors(active: bool) -> TabColors {
    if active {
        TabColors {
            bg: theme::surface::CENTER.into(),
            underline: theme::surface::CENTER.into(),
            label: theme::text::PRIMARY.into(),
        }
    } else {
        TabColors {
            bg: TRANSPARENT,
            underline: theme::border::ZONE.into(),
            label: theme::text::DIMMER.into(),
        }
    }
}

/// The CLI/terminal pane header's pty-state text (`attached · waiting on stdin` / `attached ·
/// streaming` / `exited N` / `not started`). This app has no detach/resume concept (an agent is
/// exactly one live process for its whole lifetime - see `crate::work_surface::agents`), so a `detached ·
/// resumable` state is never produced. Reads the same `is_running`/exit-code facts
/// `crate::rail::status::derive_status` consumes, rather than a second heuristic that could drift from
/// the status pill shown right next to it.
pub fn pty_state_label(is_running: bool, status: Status, exit_code: Option<u32>) -> String {
    if !is_running {
        return match exit_code {
            Some(code) => format!("exited {code}"),
            None => "not started".to_string(),
        };
    }
    match status {
        Status::Ask => "attached \u{b7} waiting on stdin".to_string(),
        Status::Idle => "attached \u{b7} idle".to_string(),
        // Fail/Review only arise from `ProcessSignal::Exited`, which implies `is_running ==
        // false` - unreachable here in practice, but matched explicitly so a future status
        // variant doesn't silently fall through a wildcard arm.
        Status::Run | Status::Fail | Status::Review => "attached \u{b7} streaming".to_string(),
    }
}

/// A bare worktree's shell tab label (Revision R12 §3): the real resolved shell binary name
/// (`program`, from `TerminalPane::program_label` - never a hardcoded `"zsh"`, even though
/// that's the design mockup's literal example) joined with the worktree's branch, e.g.
/// `zsh \u{b7} feature/x`. Falls back to the bare program name for a detached worktree with no
/// branch, rather than inventing one.
pub fn bare_worktree_shell_label(program: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("{program} \u{b7} {branch}"),
        None => program.to_string(),
    }
}

/// Appends a ` #N` ordinal (1-based, in order of appearance) to every label that repeats within
/// `labels`, so two agents on the same model in one worktree never render two identical tab
/// labels (`claude`, `claude` -> `claude #1`, `claude #2` - Revision R12 §3). A label that
/// appears only once is returned unchanged - a lone `claude` tab never grows a spurious `#1`.
pub fn disambiguate_tab_labels(labels: Vec<String>) -> Vec<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for label in &labels {
        *counts.entry(label.clone()).or_insert(0) += 1;
    }
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    labels
        .into_iter()
        .map(|label| {
            if counts.get(&label).copied().unwrap_or(0) > 1 {
                let ordinal = seen.entry(label.clone()).or_insert(0);
                *ordinal += 1;
                format!("{label} #{ordinal}")
            } else {
                label
            }
        })
        .collect()
}

/// A footer action button's colour treatment, backed by `theme::button::*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStyle {
    PrimaryGreen,
    PrimaryBlue,
    Outline,
    Ghost,
}

#[derive(Debug, Clone, Copy)]
pub struct ActionColors {
    pub bg: Rgba,
    pub border: Rgba,
    pub fg: Rgba,
    pub keycap_fg: Rgba,
    pub keycap_border: Rgba,
}

pub fn action_button_colors(style: ActionStyle) -> ActionColors {
    match style {
        ActionStyle::PrimaryGreen => ActionColors {
            bg: theme::button::GREEN_BG.into(),
            border: theme::button::GREEN_BG.into(),
            fg: theme::button::GREEN_FG.into(),
            keycap_fg: theme::button::GREEN_KEYCAP_FG.into(),
            keycap_border: theme::button::GREEN_KEYCAP.into(),
        },
        ActionStyle::PrimaryBlue => ActionColors {
            bg: theme::button::BLUE_BG.into(),
            border: theme::button::BLUE_BG.into(),
            fg: theme::button::BLUE_FG.into(),
            // Same blue (`#8fbde6`) as `term::PROMPT`, reused rather than duplicated.
            keycap_fg: theme::term::PROMPT.into(),
            keycap_border: theme::button::BLUE_KEYCAP.into(),
        },
        ActionStyle::Outline => ActionColors {
            bg: TRANSPARENT,
            border: theme::border::BUTTON.into(),
            fg: theme::text::SECONDARY.into(),
            keycap_fg: theme::text::DIMMER.into(),
            keycap_border: theme::border::BUTTON.into(),
        },
        ActionStyle::Ghost => ActionColors {
            bg: TRANSPARENT,
            border: TRANSPARENT,
            fg: theme::text::DIMMER.into(),
            keycap_fg: theme::text::FAINT.into(),
            keycap_border: theme::border::KEYCAP.into(),
        },
    }
}

/// Which operation a footer action button performs, if any - `crate::root::AdeApp`'s click
/// handlers dispatch on this. Every variant either has real backing logic wired up, or is
/// rendered honestly disabled (see [`FooterAction::implemented`]) - never a button that looks
/// clickable but silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Sends a `Ctrl-C` to the agent's pty (`TerminalPane::interrupt`).
    Interrupt,
    /// Finds-or-spawns a `Shell` agent in the same cwd and selects it.
    OpenTerminal,
    /// Closes this tab and spawns a fresh agent of the same kind/cwd - an approximate
    /// stand-in for `Retry`/`Resume` (this app has no saved-agent resumability to actually
    /// resume *from* - see [`pty_state_label`] on the same gap).
    Respawn,
    /// Closes this tab (`Agents::close`) - the same action as the context bar's own `Archive`
    /// button.
    Archive,
    /// `crate::worktree_history::flow::AdeApp::keep_all_changes` (Revision R10): a real,
    /// undoable `wt_core::undo::commit_all_changes` on this agent's worktree.
    KeepAllChanges,
    /// `crate::worktree_history::flow::AdeApp::request_discard_worktree` (Revision R10): a real,
    /// undoable `wt_core::undo::discard_worktree`, behind the same two-click confirmation as the
    /// rail footer's `prune` button (see that method's own docs for why - this is real,
    /// destructive-feeling, and force-removes a worktree, even though it's now recoverable via
    /// `Undo`).
    DiscardWorktree,
    /// No backing logic exists yet (git-level review/editor-surface workflows) - always rendered
    /// disabled regardless of [`FooterAction::implemented`] (always `false` for these).
    Unimplemented,
}

#[derive(Debug, Clone, Copy)]
pub struct FooterAction {
    pub kind: ActionKind,
    pub label: &'static str,
    /// A keybinding **spec string** (`"mod+enter"`, `"ctrl+C"`), not an already-resolved glyph -
    /// the render call site runs it through `crate::keymap::resolve_combo`, so the same spec
    /// renders `⌘⏎`/`⌃C` on macOS and `Ctrl Enter`/`Ctrl C` on Windows/Linux.
    pub keycap: Option<&'static str>,
    pub style: ActionStyle,
    /// Whether this action kind has real backing logic wired up at all (a *static* fact,
    /// independent of this agent's current state - the render call site layers further,
    /// state-dependent enablement on top of this). `false` always means rendered dimmed and
    /// non-interactive, never a clickable-looking no-op.
    pub implemented: bool,
}

/// The footer action strip for one [`Status`]: review gets `Keep all ⌘⏎` (green, real - Revision
/// R10) · `Review diff` (still unimplemented) · `Open in editor` (still unimplemented) ·
/// `Discard worktree` (real - Revision R10); ask gets `Open terminal` · `Interrupt ⌃C`; fail gets
/// `Retry ⌘R` · `Open terminal` · `Discard worktree` (real - Revision R10); run gets
/// `Interrupt ⌃C` · `Open terminal`; idle gets `Resume ⌘⏎` (blue) · `Archive`.
pub fn footer_actions(status: Status) -> Vec<FooterAction> {
    match status {
        Status::Review => vec![
            FooterAction {
                kind: ActionKind::KeepAllChanges,
                label: "Keep all",
                // No keycap: the original mockup shows `mod+enter`, but this app has no real
                // global keybinding for it - see `crate::default_key_bindings`'s own docs on
                // why a global `Ctrl+Enter`/`Cmd+Enter` isn't safe to add casually (the same
                // "app-level shortcut steals terminal input" risk class already documented
                // there for `Undo`/`Redo`, which scope themselves away from a focused terminal
                // rather than accept the collision; this app's whole domain is running agent
                // CLIs in terminals, and Ctrl+Enter/Cmd+Enter is a plausible binding one
                // of them could reasonably use for its own "submit" gesture). An audit caught
                // this row still advertising the keycap after being promoted from
                // `implemented: false` - a real keycap must never render for a keystroke that
                // does nothing, the same rule `Self::render_pty_header`'s own `clear` hint
                // already follows for the identical reason.
                keycap: None,
                style: ActionStyle::PrimaryGreen,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::Unimplemented,
                label: "Review diff",
                keycap: None,
                style: ActionStyle::Outline,
                implemented: false,
            },
            FooterAction {
                kind: ActionKind::Unimplemented,
                label: "Open in editor",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: false,
            },
            FooterAction {
                kind: ActionKind::DiscardWorktree,
                label: "Discard worktree",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: true,
            },
        ],
        Status::Ask => vec![
            FooterAction {
                kind: ActionKind::OpenTerminal,
                label: "Open terminal",
                keycap: None,
                style: ActionStyle::Outline,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::Interrupt,
                label: "Interrupt",
                keycap: Some("ctrl+C"),
                style: ActionStyle::Ghost,
                implemented: true,
            },
        ],
        Status::Fail => vec![
            FooterAction {
                kind: ActionKind::Respawn,
                label: "Retry",
                keycap: Some("mod+R"),
                style: ActionStyle::Outline,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::OpenTerminal,
                label: "Open terminal",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::DiscardWorktree,
                label: "Discard worktree",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: true,
            },
        ],
        Status::Run => vec![
            FooterAction {
                kind: ActionKind::Interrupt,
                label: "Interrupt",
                keycap: Some("ctrl+C"),
                style: ActionStyle::Outline,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::OpenTerminal,
                label: "Open terminal",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: true,
            },
        ],
        Status::Idle => vec![
            FooterAction {
                kind: ActionKind::Respawn,
                label: "Resume",
                keycap: Some("mod+enter"),
                style: ActionStyle::PrimaryBlue,
                implemented: true,
            },
            FooterAction {
                kind: ActionKind::Archive,
                label: "Archive",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: true,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kinds_get_the_cli_chip_and_shell_gets_the_terminal_chip() {
        assert_eq!(tab_chip_kind(AgentKind::Claude), TabChipKind::Cli);
        assert_eq!(tab_chip_kind(AgentKind::Codex), TabChipKind::Cli);
        assert_eq!(tab_chip_kind(AgentKind::Shell), TabChipKind::Term);
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    #[test]
    fn an_active_cli_chip_is_tinted_with_its_own_agent_colour_not_a_shared_default() {
        let claude = tab_chip_colors(AgentKind::Claude, true);
        let codex = tab_chip_colors(AgentKind::Codex, true);
        assert!(
            !same(claude.bg, codex.bg),
            "two different agents must not share a tab chip colour"
        );
        let (claude_fg, claude_bg) = theme::agent::SONNET;
        assert!(same(claude.fg, claude_fg.into()));
        assert!(same(claude.bg, claude_bg.into()));
    }

    #[test]
    fn an_active_file_tab_chip_is_tinted_with_its_own_language_colour() {
        let rs = LangChip {
            label: "rs",
            fg: theme::lang::RS.0.into(),
            bg: theme::lang::RS.1.into(),
        };
        let colors = file_tab_chip_colors(rs, true);
        assert!(same(colors.fg, theme::lang::RS.0.into()));
        assert!(same(colors.bg, theme::lang::RS.1.into()));
    }

    #[test]
    fn an_inactive_file_tab_chip_is_dimmed_to_the_same_neutral_a_agent_tab_chip_uses() {
        let rs = LangChip {
            label: "rs",
            fg: theme::lang::RS.0.into(),
            bg: theme::lang::RS.1.into(),
        };
        let file_colors = file_tab_chip_colors(rs, false);
        let agent_colors = tab_chip_colors(AgentKind::Shell, false);
        assert!(same(file_colors.bg, agent_colors.bg));
        assert!(same(file_colors.fg, agent_colors.fg));
    }

    #[test]
    fn an_inactive_chip_is_always_dimmed_to_the_same_neutral_regardless_of_kind() {
        let claude = tab_chip_colors(AgentKind::Claude, false);
        let shell = tab_chip_colors(AgentKind::Shell, false);
        assert!(same(claude.bg, shell.bg));
        assert!(same(claude.fg, shell.fg));
        assert!(same(claude.bg, theme::border::ZONE.into()));
    }

    #[test]
    fn active_tab_background_and_underline_are_the_same_colour_so_it_merges_into_the_surface() {
        let active = tab_colors(true);
        assert!(
            same(active.bg, active.underline),
            "an active tab's underline must match its own background - that's how it visually \
             merges into the surface below it, per design_handoff_jerry_ade/README.md"
        );
    }

    #[test]
    fn inactive_tab_background_is_transparent_not_the_active_colour() {
        let inactive = tab_colors(false);
        assert!(same(inactive.bg, TRANSPARENT));
        assert!(!same(inactive.underline, tab_colors(true).underline));
    }

    #[test]
    fn a_process_that_never_started_reports_not_started_not_a_false_exit() {
        assert_eq!(pty_state_label(false, Status::Idle, None), "not started");
    }

    #[test]
    fn an_exited_process_always_reports_its_real_exit_code() {
        assert_eq!(
            pty_state_label(false, Status::Fail, Some(101)),
            "exited 101"
        );
        assert_eq!(pty_state_label(false, Status::Review, Some(0)), "exited 0");
    }

    #[test]
    fn a_running_agent_past_the_ask_threshold_reports_waiting_on_stdin() {
        assert_eq!(
            pty_state_label(true, Status::Ask, None),
            "attached \u{b7} waiting on stdin"
        );
    }

    #[test]
    fn a_running_and_recently_active_agent_reports_streaming() {
        assert_eq!(
            pty_state_label(true, Status::Run, None),
            "attached \u{b7} streaming"
        );
    }

    #[test]
    fn a_running_idle_shell_reports_idle_not_streaming_or_exited() {
        assert_eq!(
            pty_state_label(true, Status::Idle, None),
            "attached \u{b7} idle"
        );
    }

    #[test]
    fn review_actions_keep_all_and_discard_are_real_review_diff_and_open_in_editor_are_not() {
        let actions = footer_actions(Status::Review);
        for action in &actions {
            let should_be_implemented = matches!(
                action.kind,
                ActionKind::KeepAllChanges | ActionKind::DiscardWorktree
            );
            assert_eq!(
                action.implemented, should_be_implemented,
                "{} implemented={} - Revision R10 gave Keep all/Discard worktree real backing, \
                 Review diff/Open in editor still have none",
                action.label, action.implemented
            );
        }
    }

    #[test]
    fn ask_actions_are_open_terminal_then_interrupt_both_real() {
        let actions = footer_actions(Status::Ask);
        let labels: Vec<&str> = actions.iter().map(|a| a.label).collect();
        assert_eq!(labels, vec!["Open terminal", "Interrupt"]);
        assert!(actions.iter().all(|a| a.implemented));
    }

    #[test]
    fn fail_actions_include_a_real_retry_and_a_real_discard() {
        let actions = footer_actions(Status::Fail);
        assert_eq!(actions[0].kind, ActionKind::Respawn);
        assert!(actions[0].implemented);
        assert_eq!(actions.last().unwrap().kind, ActionKind::DiscardWorktree);
        assert!(actions.last().unwrap().implemented);
    }

    #[test]
    fn idle_actions_are_resume_then_archive_both_real() {
        let actions = footer_actions(Status::Idle);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].kind, ActionKind::Respawn);
        assert_eq!(actions[1].kind, ActionKind::Archive);
        assert!(actions.iter().all(|a| a.implemented));
    }

    #[test]
    fn every_status_produces_a_non_empty_action_list() {
        for status in Status::ORDER {
            assert!(
                !footer_actions(status).is_empty(),
                "{status:?} produced no footer actions at all"
            );
        }
    }

    #[test]
    fn a_lone_tab_label_is_never_given_a_spurious_ordinal() {
        let labels = disambiguate_tab_labels(vec!["claude".to_string(), "codex".to_string()]);
        assert_eq!(labels, vec!["claude", "codex"]);
    }

    #[test]
    fn two_agents_on_the_same_model_get_ordinal_suffixes_never_an_identical_label() {
        let labels = disambiguate_tab_labels(vec![
            "sonnet-4.5".to_string(),
            "sonnet-4.5".to_string(),
            "codex".to_string(),
        ]);
        assert_eq!(labels, vec!["sonnet-4.5 #1", "sonnet-4.5 #2", "codex"]);
        assert_ne!(
            labels[0], labels[1],
            "two tabs must never share an identical label"
        );
    }

    #[test]
    fn three_agents_on_the_same_model_all_get_distinct_ordinals_in_appearance_order() {
        let labels = disambiguate_tab_labels(vec![
            "claude".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        ]);
        assert_eq!(labels, vec!["claude #1", "claude #2", "claude #3"]);
    }

    #[test]
    fn a_bare_worktrees_shell_label_joins_the_real_program_name_with_its_branch() {
        assert_eq!(
            bare_worktree_shell_label("zsh", Some("feature/x")),
            "zsh \u{b7} feature/x"
        );
    }

    #[test]
    fn a_bare_worktrees_shell_label_falls_back_to_the_program_name_when_detached() {
        assert_eq!(bare_worktree_shell_label("bash", None), "bash");
    }

    #[test]
    fn action_button_colours_are_distinct_per_style() {
        let styles = [
            ActionStyle::PrimaryGreen,
            ActionStyle::PrimaryBlue,
            ActionStyle::Outline,
            ActionStyle::Ghost,
        ];
        let colors: Vec<ActionColors> = styles.iter().map(|s| action_button_colors(*s)).collect();
        for (i, a) in colors.iter().enumerate() {
            for (j, b) in colors.iter().enumerate() {
                if i != j {
                    assert!(
                        !same(a.fg, b.fg) || !same(a.bg, b.bg),
                        "action styles {i} and {j} are visually indistinguishable"
                    );
                }
            }
        }
    }

    /// A fresh worktree (nothing dragged yet) must render its agents first, in creation order,
    /// then its file tabs, in `open_files`' own order - exactly the old two-block layout, so this
    /// revision's interleaving layer is a strict superset of the old behaviour, not a visible
    /// change until a real drag happens.
    #[test]
    fn reconcile_with_no_stored_order_appends_agents_then_files_in_their_own_order() {
        let order = reconcile_tab_order(
            &[],
            &[1, 2],
            &[PathBuf::from("a.rs"), PathBuf::from("b.rs")],
        );
        assert_eq!(
            order,
            vec![
                TabRef::Agent(1),
                TabRef::Agent(2),
                TabRef::File(PathBuf::from("a.rs")),
                TabRef::File(PathBuf::from("b.rs")),
            ]
        );
    }

    /// The real interleaving case (GitHub issue #16): a stored order with a file tab sitting
    /// between two agent tabs must survive reconciliation unchanged, as long as every entry in
    /// it still exists.
    #[test]
    fn reconcile_preserves_a_stored_interleaved_order() {
        let stored = vec![
            TabRef::Agent(1),
            TabRef::File(PathBuf::from("a.rs")),
            TabRef::Agent(2),
        ];
        let order = reconcile_tab_order(&stored, &[1, 2], &[PathBuf::from("a.rs")]);
        assert_eq!(order, stored);
    }

    /// A agent closed (or a file tab closed) since the order was last stored must be dropped,
    /// not left as a dangling reference to something `crate::root::AdeApp::render_tab_strip`
    /// would otherwise try to render.
    #[test]
    fn reconcile_drops_entries_that_no_longer_exist() {
        let stored = vec![
            TabRef::Agent(1),
            TabRef::File(PathBuf::from("a.rs")),
            TabRef::Agent(2),
        ];
        let order = reconcile_tab_order(&stored, &[2], &[]);
        assert_eq!(order, vec![TabRef::Agent(2)]);
    }

    /// A brand new agent/file not yet in the stored order must be appended at the end, never
    /// silently dropped or inserted somewhere the user never asked for.
    #[test]
    fn reconcile_appends_newly_opened_tabs_not_yet_in_the_stored_order() {
        let stored = vec![TabRef::Agent(1)];
        let order = reconcile_tab_order(&stored, &[1, 2], &[PathBuf::from("a.rs")]);
        assert_eq!(
            order,
            vec![
                TabRef::Agent(1),
                TabRef::Agent(2),
                TabRef::File(PathBuf::from("a.rs")),
            ]
        );
    }

    /// The real cross-kind drag this revision exists to unlock: dropping a file tab so it lands
    /// immediately before an agent tab must actually interleave them, not just reorder within
    /// each tab's own kind.
    #[test]
    fn move_tab_order_drops_a_file_tab_before_a_agent_tab() {
        let mut order = vec![
            TabRef::Agent(1),
            TabRef::Agent(2),
            TabRef::File(PathBuf::from("a.rs")),
        ];
        move_tab_order(
            &mut order,
            &TabRef::File(PathBuf::from("a.rs")),
            &TabRef::Agent(2),
            false,
        );
        assert_eq!(
            order,
            vec![
                TabRef::Agent(1),
                TabRef::File(PathBuf::from("a.rs")),
                TabRef::Agent(2),
            ]
        );
    }

    /// `insert_after` must land the dragged tab on the far side of the target from a plain
    /// "insert before" - the real distinction the insertion-caret precision (left half vs. right
    /// half of the hovered tab) is for.
    #[test]
    fn move_tab_order_respects_insert_after() {
        let mut order = vec![TabRef::Agent(1), TabRef::Agent(2), TabRef::Agent(3)];
        move_tab_order(&mut order, &TabRef::Agent(1), &TabRef::Agent(2), true);
        assert_eq!(
            order,
            vec![TabRef::Agent(2), TabRef::Agent(1), TabRef::Agent(3)]
        );
    }

    /// `move_tab_order` must never corrupt the order on a bad move - dropping a tab onto itself,
    /// an unknown dragged entry, or an unknown target must all be real no-ops.
    #[test]
    fn move_tab_order_is_a_no_op_for_an_unknown_or_identical_entry() {
        let original = vec![TabRef::Agent(1), TabRef::File(PathBuf::from("a.rs"))];
        let mut order = original.clone();
        move_tab_order(&mut order, &TabRef::Agent(1), &TabRef::Agent(1), false);
        move_tab_order(&mut order, &TabRef::Agent(99), &TabRef::Agent(1), false);
        move_tab_order(&mut order, &TabRef::Agent(1), &TabRef::Agent(99), false);
        assert_eq!(order, original);
    }
}
