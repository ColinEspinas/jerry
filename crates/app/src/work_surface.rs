//! Pure logic for Zone 2's restyle (`design_handoff_jerry_ade/README.md`'s "Zone 2 — work
//! surface": tab strip, session context bar, and the CLI/terminal pane header/footer).
//!
//! Deliberately GPUI-free, mirroring `crate::status`'s own split: this module only maps
//! already-known facts (a [`SessionKind`], a [`Status`], a `bool`) onto *which* colours/
//! labels/actions a Zone 2 element should show, so that mapping is directly unit-testable
//! without a live GPUI window. Turning these into actual `gpui::Div` trees (and wiring real
//! click handlers - spawning/closing/interrupting a session) happens one layer up, in
//! `crate::root`, which has the `Context<AdeApp>` these decisions need to act on.

use gpui::Rgba;

use crate::file_tree::LangChip;
use crate::sessions::SessionKind;
use crate::status::Status;
use crate::theme;

/// Fully transparent - used where the design's own inline styles say `background:transparent`
/// / no border (the "outline"/"ghost" button variants, an inactive tab's background), so every
/// button/tab can always call `.bg()`/`.border_color()` uniformly rather than conditionally
/// skipping the call (which would also shift the box model by the border's width - see
/// `crate::root::render_session_tab`'s docs).
pub const TRANSPARENT: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// The real agent tint `(fg, bg)` for a session's badge/chip - `design_handoff_jerry_ade/
/// README.md`'s "Agent badge — 16×16, radius 3, agent tint background" and the tab strip's
/// "chip tinted with **that agent's** colour" both read this. [`SessionKind::Shell`] isn't an
/// "agent" in the design's sense (no agent tint is specified for a plain shell), so it gets a
/// neutral chip instead, matching the tab strip's own "terminal" chip colours rather than
/// inventing a tint the design never specified.
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

/// Which of the tab strip's two chip shapes a session's tab draws
/// (`design_handoff_jerry_ade/README.md`'s tab-strip spec: agent CLI gets a `❯` glyph,
/// terminal gets the pane glyph - 14×4 bar + prompt mark). The design's third tab kind
/// ("code", a file's language chip) has no equivalent in this app's session model - here
/// every session *is* its own tab, not a per-session Cli/Terminal/Code view switcher - so it's
/// never constructed by this app.
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

/// A tab chip's real `(bg, fg)`, active or dimmed - `Jerry.dc.html`'s own tab computation:
/// `chipBg: on ? chip.bg : '#1e2225', chipFg: on ? chip.fg : '#5e646a'` (`#1e2225` ==
/// `theme::border::ZONE`, `#5e646a` == `theme::text::FAINTER` - both already-ported tokens,
/// just reused for a different element here).
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

/// A file tab's real chip colours (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 4:
/// "the file's language chip, same table as the file tree" for the active state, dimmed to the
/// exact same `bg`/`fg` [`tab_chip_colors`] already dims a session tab's chip to when inactive -
/// `Jerry.dc.html`'s own tab computation applies the identical `chipBg: on ? chip.bg : '#1e2225',
/// chipFg: on ? chip.fg : '#5e646a'` rule regardless of whether the tab is a session or a file).
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

/// A tab's own label/background/underline/foreground - `Jerry.dc.html`'s tab computation:
/// `bg: on ? '#131518' : 'transparent', underline: on ? '#131518' : '#1e2225', fg: on ?
/// '#d3d8dd' : '#767d84'`. `#131518` and `#d3d8dd` are exact `theme::surface::CENTER`/
/// `theme::text::PRIMARY` matches; the inactive label colour `#767d84` has no exact token in
/// `design_handoff_jerry_ade/tokens.rs` (the design-approved transcription this app's
/// `theme.rs` mirrors field-for-field) - `theme::text::DIMMER` (`#7d848b`) is the closest
/// ported token (differs by 7/7/7 per channel vs. `FAINT`'s 11/12/12), so that's what's used
/// here rather than inventing a new, untracked colour constant for one pixel-level shade.
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

/// The CLI/terminal pane header's real pty-state text
/// (`design_handoff_jerry_ade/README.md`'s Surface A spec: "attached · waiting on stdin" /
/// "attached · streaming" / "exited 0" / "exited 101" / "detached · resumable"). This app has
/// no detach/resume concept (`crate::sessions`: a session is exactly one live process for its
/// whole lifetime, from `Sessions::spawn` to `Sessions::close`), so `detached · resumable` is
/// never produced. Every other state is read from already-derived, real signals - the same
/// `is_running`/exit-code facts `crate::status::derive_status` itself consumes - rather than a
/// second, independent heuristic that could drift from the status pill shown right next to it.
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
        // `Status::Fail`/`Status::Review` only ever arise from `ProcessSignal::Exited` (see
        // `crate::status::derive_status`), which implies `is_running == false` - unreachable
        // in practice while `is_running` is `true`, but matched explicitly (rather than a
        // wildcard) so a future status added to the enum doesn't silently fall through here.
        Status::Run | Status::Fail | Status::Review => "attached \u{b7} streaming".to_string(),
    }
}

/// A footer action button's colour treatment - `Jerry.dc.html`'s own `AB` (action-button)
/// dictionary (`primaryG`/`primaryB`/`outline`/`ghost`), backed by `theme::button::*` (plus
/// [`theme::button::GREEN_KEYCAP_FG`] - see its own docs for why that one constant was added
/// directly from the HTML rather than already existing in `tokens.rs`).
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
            // `Jerry.dc.html`'s blue keycap glyph colour (`#8fbde6`) is the exact same value
            // already ported as `term::PROMPT` - see `theme::button::GREEN_KEYCAP_FG`'s docs.
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

/// Which real operation a footer action button performs, if any - `crate::root::AdeApp`'s
/// click handlers dispatch on this. Every variant either has real, minimal backing logic
/// wired up this phase, or is rendered honestly disabled (see [`FooterAction::implemented`]'s
/// docs) - never a button that looks clickable but silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Sends a real `Ctrl-C` to the session's pty (`TerminalPane::interrupt`).
    Interrupt,
    /// Finds-or-spawns a real `Shell` session in the same cwd and selects it.
    OpenTerminal,
    /// Closes this tab and spawns a fresh session of the same kind/cwd - a real, if
    /// approximate, stand-in for the design's `Retry`/`Resume` (this app has no saved-session
    /// resumability to actually resume *from* - see `pty_state_label`'s docs on the same
    /// gap).
    Respawn,
    /// Closes this tab (`Sessions::close`) - the same real action as the context bar's own
    /// `Archive` button.
    Archive,
    /// No real backing logic exists yet this phase (git-level review/merge/editor-surface
    /// workflows - out of scope here, see `crate::root`'s module docs) - always rendered
    /// disabled regardless of [`FooterAction::implemented`] (which is always `false` for
    /// these).
    Unimplemented,
}

#[derive(Debug, Clone, Copy)]
pub struct FooterAction {
    pub kind: ActionKind,
    pub label: &'static str,
    /// A real keybinding **spec string** (`"mod+enter"`, `"ctrl+C"`), not an already-resolved
    /// glyph - `crate::root::work_surface_render::render_footer_action_button` runs it through
    /// `crate::keymap::resolve_combo` at render time, so the same spec renders `⌘⏎`/`⌃C` on
    /// macOS and `Ctrl Enter`/`Ctrl C` on Windows/Linux, never a hardcoded platform-specific
    /// literal (`design_handoff_jerry_ade/CHANGELOG.md`'s 2026-07-29 entry, change 2).
    pub keycap: Option<&'static str>,
    pub style: ActionStyle,
    /// Whether this action kind has real backing logic wired up in this phase at all (a
    /// *static* fact about the action, independent of this particular session's current
    /// state - `crate::root::AdeApp`'s render call site layers any further, state-dependent
    /// enablement - e.g. `Resume` only while the process isn't already running - on top of
    /// this). `false` here always means "rendered dimmed, non-interactive, real disabled
    /// state" (matching the design's own "Accept file is always rendered, dimmed ... when
    /// there is nothing to accept" precedent), never a clickable-looking no-op.
    pub implemented: bool,
}

/// The footer action strip for one [`Status`] - `design_handoff_jerry_ade/README.md`'s
/// Surface A footer spec, one list per status (keybinding spec strings shown resolved to
/// their macOS glyphs here for readability - see [`FooterAction::keycap`]'s docs for why the
/// stored value is the unresolved spec, not this literal glyph):
/// review: `Keep all ⌘⏎` (green) · `Review diff` · `Open in editor` · `Discard worktree`;
/// ask: `Open terminal` · `Interrupt ⌃C`; fail: `Retry ⌘R` · `Open terminal` ·
/// `Discard worktree`; run: `Interrupt ⌃C` · `Open terminal`; idle: `Resume ⌘⏎` (blue) ·
/// `Archive`.
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
