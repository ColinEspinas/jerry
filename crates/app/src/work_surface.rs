//! Pure logic for Zone 2 (work surface): tab strip, session context bar, and the
//! CLI/terminal pane header/footer.
//!
//! Deliberately GPUI-free, mirroring `crate::status`'s own split: this module only maps
//! already-known facts (a [`SessionKind`], a [`Status`], a `bool`) onto *which*
//! colours/labels/actions a Zone 2 element should show, so that mapping is directly
//! unit-testable without a live GPUI window. Turning these into actual `gpui::Div` trees (and
//! wiring click handlers) happens one layer up, in `crate::root`, which has the
//! `Context<AdeApp>` these decisions need to act on.

use gpui::Rgba;

use crate::file_tree::LangChip;
use crate::sessions::SessionKind;
use crate::status::Status;
use crate::theme;

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

/// The agent tint `(fg, bg)` for a session's badge/chip. [`SessionKind::Shell`] isn't an agent,
/// so it gets a neutral chip instead of an invented tint.
pub fn agent_tint(kind: SessionKind) -> (Rgba, Rgba) {
    match kind {
        SessionKind::Claude => theme::agent::SONNET,
        SessionKind::Codex => theme::agent::CODEX,
        SessionKind::Shell => (theme::text::DIM, theme::surface::CHIP_NEUTRAL),
    }
}

/// The agent badge's single-character initial.
pub fn agent_initial(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Claude => "C",
        SessionKind::Codex => "X",
        SessionKind::Shell => "$",
    }
}

/// Which of the tab strip's two chip shapes a session's tab draws: agent CLI gets a `❯` glyph,
/// terminal gets the pane glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabChipKind {
    Cli,
    Term,
}

pub fn tab_chip_kind(kind: SessionKind) -> TabChipKind {
    match kind {
        SessionKind::Claude | SessionKind::Codex => TabChipKind::Cli,
        SessionKind::Shell => TabChipKind::Term,
    }
}

/// A tab chip's `(bg, fg)`, active or dimmed. Dimmed reuses [`theme::border::ZONE`] (the same
/// token an inactive tab's own underline uses) for `bg`, and [`theme::text::FAINTER`] for `fg`.
#[derive(Debug, Clone, Copy)]
pub struct ChipColors {
    pub bg: Rgba,
    pub fg: Rgba,
}

pub fn tab_chip_colors(kind: SessionKind, active: bool) -> ChipColors {
    if active {
        let (fg, bg) = agent_tint(kind);
        ChipColors { bg, fg }
    } else {
        ChipColors {
            bg: theme::border::ZONE,
            fg: theme::text::FAINTER,
        }
    }
}

/// A file tab's chip colours - the file's language chip when active, dimmed to the exact same
/// `bg`/`fg` [`tab_chip_colors`] dims a session tab's chip to when inactive.
pub fn file_tab_chip_colors(lang: LangChip, active: bool) -> ChipColors {
    if active {
        ChipColors {
            bg: lang.bg,
            fg: lang.fg,
        }
    } else {
        ChipColors {
            bg: theme::border::ZONE,
            fg: theme::text::FAINTER,
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
            bg: theme::surface::CENTER,
            underline: theme::surface::CENTER,
            label: theme::text::PRIMARY,
        }
    } else {
        TabColors {
            bg: TRANSPARENT,
            underline: theme::border::ZONE,
            label: theme::text::DIMMER,
        }
    }
}

/// The CLI/terminal pane header's pty-state text (`attached · waiting on stdin` / `attached ·
/// streaming` / `exited N` / `not started`). This app has no detach/resume concept (a session is
/// exactly one live process for its whole lifetime - see `crate::sessions`), so a `detached ·
/// resumable` state is never produced. Reads the same `is_running`/exit-code facts
/// `crate::status::derive_status` consumes, rather than a second heuristic that could drift from
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
            bg: theme::button::GREEN_BG,
            border: theme::button::GREEN_BG,
            fg: theme::button::GREEN_FG,
            keycap_fg: theme::button::GREEN_KEYCAP_FG,
            keycap_border: theme::button::GREEN_KEYCAP,
        },
        ActionStyle::PrimaryBlue => ActionColors {
            bg: theme::button::BLUE_BG,
            border: theme::button::BLUE_BG,
            fg: theme::button::BLUE_FG,
            // Same blue (`#8fbde6`) as `term::PROMPT`, reused rather than duplicated.
            keycap_fg: theme::term::PROMPT,
            keycap_border: theme::button::BLUE_KEYCAP,
        },
        ActionStyle::Outline => ActionColors {
            bg: TRANSPARENT,
            border: theme::border::BUTTON,
            fg: theme::text::SECONDARY,
            keycap_fg: theme::text::DIMMER,
            keycap_border: theme::border::BUTTON,
        },
        ActionStyle::Ghost => ActionColors {
            bg: TRANSPARENT,
            border: TRANSPARENT,
            fg: theme::text::DIMMER,
            keycap_fg: theme::text::FAINT,
            keycap_border: theme::border::KEYCAP,
        },
    }
}

/// Which operation a footer action button performs, if any - `crate::root::AdeApp`'s click
/// handlers dispatch on this. Every variant either has real backing logic wired up, or is
/// rendered honestly disabled (see [`FooterAction::implemented`]) - never a button that looks
/// clickable but silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Sends a `Ctrl-C` to the session's pty (`TerminalPane::interrupt`).
    Interrupt,
    /// Finds-or-spawns a `Shell` session in the same cwd and selects it.
    OpenTerminal,
    /// Closes this tab and spawns a fresh session of the same kind/cwd - an approximate
    /// stand-in for `Retry`/`Resume` (this app has no saved-session resumability to actually
    /// resume *from* - see [`pty_state_label`] on the same gap).
    Respawn,
    /// Closes this tab (`Sessions::close`) - the same action as the context bar's own `Archive`
    /// button.
    Archive,
    /// No backing logic exists yet (git-level review/merge/editor-surface workflows) - always
    /// rendered disabled regardless of [`FooterAction::implemented`] (always `false` for these).
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
    /// independent of this session's current state - the render call site layers further,
    /// state-dependent enablement on top of this). `false` always means rendered dimmed and
    /// non-interactive, never a clickable-looking no-op.
    pub implemented: bool,
}

/// The footer action strip for one [`Status`]: review gets `Keep all ⌘⏎` (green) · `Review
/// diff` · `Open in editor` · `Discard worktree`; ask gets `Open terminal` · `Interrupt ⌃C`;
/// fail gets `Retry ⌘R` · `Open terminal` · `Discard worktree`; run gets `Interrupt ⌃C` ·
/// `Open terminal`; idle gets `Resume ⌘⏎` (blue) · `Archive`.
pub fn footer_actions(status: Status) -> Vec<FooterAction> {
    match status {
        Status::Review => vec![
            FooterAction {
                kind: ActionKind::Unimplemented,
                label: "Keep all",
                keycap: Some("mod+enter"),
                style: ActionStyle::PrimaryGreen,
                implemented: false,
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
                kind: ActionKind::Unimplemented,
                label: "Discard worktree",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: false,
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
                kind: ActionKind::Unimplemented,
                label: "Discard worktree",
                keycap: None,
                style: ActionStyle::Ghost,
                implemented: false,
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
        assert_eq!(tab_chip_kind(SessionKind::Claude), TabChipKind::Cli);
        assert_eq!(tab_chip_kind(SessionKind::Codex), TabChipKind::Cli);
        assert_eq!(tab_chip_kind(SessionKind::Shell), TabChipKind::Term);
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    #[test]
    fn an_active_cli_chip_is_tinted_with_its_own_agent_colour_not_a_shared_default() {
        let claude = tab_chip_colors(SessionKind::Claude, true);
        let codex = tab_chip_colors(SessionKind::Codex, true);
        assert!(
            !same(claude.bg, codex.bg),
            "two different agents must not share a tab chip colour"
        );
        let (claude_fg, claude_bg) = theme::agent::SONNET;
        assert!(same(claude.fg, claude_fg));
        assert!(same(claude.bg, claude_bg));
    }

    #[test]
    fn an_active_file_tab_chip_is_tinted_with_its_own_language_colour() {
        let rs = LangChip {
            label: "rs",
            fg: theme::lang::RS.0,
            bg: theme::lang::RS.1,
        };
        let colors = file_tab_chip_colors(rs, true);
        assert!(same(colors.fg, theme::lang::RS.0));
        assert!(same(colors.bg, theme::lang::RS.1));
    }

    #[test]
    fn an_inactive_file_tab_chip_is_dimmed_to_the_same_neutral_a_session_tab_chip_uses() {
        let rs = LangChip {
            label: "rs",
            fg: theme::lang::RS.0,
            bg: theme::lang::RS.1,
        };
        let file_colors = file_tab_chip_colors(rs, false);
        let session_colors = tab_chip_colors(SessionKind::Shell, false);
        assert!(same(file_colors.bg, session_colors.bg));
        assert!(same(file_colors.fg, session_colors.fg));
    }

    #[test]
    fn an_inactive_chip_is_always_dimmed_to_the_same_neutral_regardless_of_kind() {
        let claude = tab_chip_colors(SessionKind::Claude, false);
        let shell = tab_chip_colors(SessionKind::Shell, false);
        assert!(same(claude.bg, shell.bg));
        assert!(same(claude.fg, shell.fg));
        assert!(same(claude.bg, theme::border::ZONE));
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
    fn a_running_and_recently_active_session_reports_streaming() {
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
    fn review_actions_are_all_disabled_since_no_review_workflow_exists_yet() {
        for action in footer_actions(Status::Review) {
            assert!(
                !action.implemented,
                "{} must be disabled - no real diff-review/merge backing exists this phase",
                action.label
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
    fn fail_actions_include_a_real_retry_and_a_disabled_discard() {
        let actions = footer_actions(Status::Fail);
        assert_eq!(actions[0].kind, ActionKind::Respawn);
        assert!(actions[0].implemented);
        assert!(!actions.last().unwrap().implemented);
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
}
