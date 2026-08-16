//! The rail's menus, as pure data (GitHub issue #290): which rows a worktree row, an agent row
//! and the `⋯` overflow each offer, and what running one of those rows means.

use std::path::PathBuf;

use crate::icons::Icon;
use crate::menu::model::{MenuEntry, MenuRow};
use crate::root::plural;
use crate::work_surface::agents::AgentId;

/// What a rail menu is open *on*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailMenuTarget {
    /// A worktree row, by the path that identifies it everywhere else in the rail.
    Worktree(PathBuf),
    /// The `Open in…` second level for that same worktree.
    WorktreeOpenIn(PathBuf),
    /// A live agent row, by the id its own row and tab are keyed on.
    Agent(AgentId),
}

/// An open rail row menu: what it is on, and the already-clamped, window-space corner it paints
/// at (`crate::menu::model::clamp_menu_origin`, resolved once at open time off the real pointer).
#[derive(Debug, Clone, PartialEq)]
pub struct RailRowMenu {
    pub target: RailMenuTarget,
    pub origin_x: f32,
    pub origin_y: f32,
}

/// The open `⋯` overflow menu. No target - there is one `⋯`, and its rows name destinations
/// rather than acting on a row. Anchored off the button's own rect instead of the pointer
/// (§4w: "the overflow menu off the ⋯ button's own rect with right edges aligned"), which is why
/// this is a separate surface from [`RailRowMenu`] rather than a fourth [`RailMenuTarget`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RailOverflowMenu {
    pub origin_x: f32,
    pub origin_y: f32,
}

/// One rail menu row's real action. Every variant runs a real handler in
/// [`crate::rail::menu_render`]; there is no decorative entry here (§7 rule 1: "Ship the
/// affordance with the behaviour, or ship neither").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailMenuAction {
    /// Spawn another agent into this worktree.
    NewAgentHere,
    /// End every run in this worktree and leave them in History.
    ArchiveWorktreeAgents,
    CopyBranchName,
    CopyPath,
    /// Opens this menu's own second level - see [`RailMenuTarget::WorktreeOpenIn`].
    OpenIn,
    OpenInFileManager,
    OpenInTerminal,
    /// The app's one worktree-deletion entry point (the Changes panel deliberately has none) -
    /// two clicks, routed into the existing discard flow. See
    /// `crate::worktree_history::flow::AdeApp::request_discard_worktree_path`.
    RemoveWorktree,
    /// Switch to this agent's tab.
    OpenAgent,
    /// Stop a running agent where it is.
    PauseAgent,
    /// Start an idle agent's work again.
    ResumeAgent,
    /// End the run; it stays in History.
    ArchiveRun,
    /// The `⋯` overflow's two destinations.
    OpenHistory,
    OpenSettings,
}

/// `Remove worktree…`'s hint - what `wt_core::undo::discard_worktree` really does, in both of
/// that row's two states (see [`worktree_menu_groups`]), stated once so the armed and unarmed
/// labels cannot end up describing different operations.
pub const REMOVE_WORKTREE_HINT: &str = "Removes the checkout. Uncommitted and untracked work is \
                                        stashed first; gitignored files are not preserved.";

/// `Archive run`'s hint, quoted verbatim from `REVISION-2026-08-14.md` §6 - the single row that
/// replaced an `Archive run` + red `Delete run…` pair, because "if two menu items need their
/// hints read to be told apart, they are one item" (§7 rule 3).
pub const ARCHIVE_RUN_HINT: &str = "Ends the run. It stays in History with its transcript, \
                                    diffstat and notes; the files it wrote are untouched.";

/// The worktree row menu (§4's list, in §4's order).
pub fn worktree_menu_groups(
    branch: Option<&str>,
    agent_count: usize,
    remove_armed: bool,
) -> Vec<Vec<MenuEntry<RailMenuAction>>> {
    let archive = if agent_count == 0 {
        MenuEntry::new(
            RailMenuAction::ArchiveWorktreeAgents,
            "No agents to archive",
        )
        .gated(false, "no agent is open in this worktree")
    } else {
        MenuEntry::new(
            RailMenuAction::ArchiveWorktreeAgents,
            format!("Archive {}", plural::count(agent_count, "agent", None)),
        )
        .tooltip(
            "Ends every run here and files them in History; the files they wrote are untouched.",
        )
    };
    vec![
        vec![
            MenuEntry::new(RailMenuAction::NewAgentHere, "New agent here")
                .tooltip("Another agent in this same checkout"),
            archive,
        ],
        vec![
            MenuEntry::new(RailMenuAction::CopyBranchName, "Copy branch name")
                .gated(branch.is_some(), "this worktree is not on a branch"),
            MenuEntry::new(RailMenuAction::CopyPath, "Copy path"),
            MenuEntry::new(RailMenuAction::OpenIn, "Open in\u{2026}")
                .tooltip("Your file manager, or a terminal in this worktree"),
        ],
        vec![MenuEntry::new(
            RailMenuAction::RemoveWorktree,
            // The armed state re-labels *this* row rather than adding a second one - one
            // command, one row, whichever click it is on.
            if remove_armed {
                "Remove worktree \u{2014} click again"
            } else {
                "Remove worktree\u{2026}"
            },
        )
        .tooltip(REMOVE_WORKTREE_HINT)
        .destructive()],
    ]
}

/// The `Open in…` second level - both destinations are real (`crate::settings::widgets`'s OS
/// open handler, and this app's own new-terminal spawn), which is what lets the parent row keep
/// its ellipsis.
pub fn open_in_menu_groups() -> Vec<Vec<MenuEntry<RailMenuAction>>> {
    vec![vec![
        MenuEntry::new(RailMenuAction::OpenInFileManager, "File manager")
            .tooltip("Opens this worktree with whatever your OS opens a folder with"),
        MenuEntry::new(RailMenuAction::OpenInTerminal, "Terminal")
            .keys("ctrl+shift+T")
            .tooltip("A shell in this worktree"),
    ]]
}

/// The agent row menu (§4 and §6).
pub fn agent_menu_groups(running: bool) -> Vec<Vec<MenuEntry<RailMenuAction>>> {
    let pause_or_resume = if running {
        MenuEntry::new(RailMenuAction::PauseAgent, "Pause")
            .tooltip("Stops the agent where it is; its tab and transcript stay open")
    } else {
        MenuEntry::new(RailMenuAction::ResumeAgent, "Resume").tooltip(
            "Closes this run's tab and starts a fresh agent of the same kind in this \
                 worktree - there is no saved transcript to continue from",
        )
    };
    vec![
        vec![
            MenuEntry::new(RailMenuAction::OpenAgent, "Open")
                .tooltip("Shows this run in the centre pane"),
            pause_or_resume,
        ],
        // One `Archive run`, and no red `Delete run…` beside it (§6, §7 rule 3). Removing a
        // History entry outright belongs in History, where you can see what you are removing.
        vec![MenuEntry::new(RailMenuAction::ArchiveRun, "Archive run").tooltip(ARCHIVE_RUN_HINT)],
    ]
}

/// Whether this build really has a History *view* for the `⋯` overflow to switch to.
pub const HISTORY_VIEW_AVAILABLE: bool = true;

/// The sidebar strip's `⋯` overflow (§4u): "History and Settings only, with the glyphs they had
/// in the strip (clock, sliders) so the move out of the strip does not cost their
/// recognisability. Command palette, Keyboard shortcuts and About were filler - the palette has
/// `⌘K` and its own surface."
pub fn overflow_menu_groups(history_available: bool) -> Vec<Vec<MenuEntry<RailMenuAction>>> {
    vec![vec![
        MenuEntry::new(RailMenuAction::OpenHistory, "History")
            .glyph(Icon::ClockCounterClockwise)
            .tooltip("Earlier runs, by repo and worktree")
            .gated(
                history_available,
                "there is no History surface in this build (issue #227)",
            ),
        MenuEntry::new(RailMenuAction::OpenSettings, "Settings")
            .glyph(Icon::SlidersHorizontal)
            .keys("mod+,")
            .tooltip("Agents, worktrees, appearance"),
    ]]
}

/// Exactly the rows an open menu on `target` paints, dividers included.
pub fn menu_rows(
    target: &RailMenuTarget,
    branch: Option<&str>,
    agent_count: usize,
    remove_armed: bool,
    agent_running: bool,
) -> Vec<MenuRow<RailMenuAction>> {
    crate::menu::model::menu_rows(match target {
        RailMenuTarget::Worktree(_) => worktree_menu_groups(branch, agent_count, remove_armed),
        RailMenuTarget::WorktreeOpenIn(_) => open_in_menu_groups(),
        RailMenuTarget::Agent(_) => agent_menu_groups(agent_running),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(groups: &[Vec<MenuEntry<RailMenuAction>>]) -> Vec<Vec<&str>> {
        groups
            .iter()
            .map(|group| group.iter().map(|entry| entry.label.as_str()).collect())
            .collect()
    }

    #[test]
    fn the_worktree_menu_is_the_row_set_the_revision_lists() {
        assert_eq!(
            labels(&worktree_menu_groups(Some("fix/auth"), 2, false)),
            vec![
                vec!["New agent here", "Archive 2 agents"],
                vec!["Copy branch name", "Copy path", "Open in\u{2026}"],
                vec!["Remove worktree\u{2026}"],
            ]
        );
    }

    #[test]
    fn no_rail_menu_has_a_title_row() {
        for target in [
            RailMenuTarget::Worktree(PathBuf::from("/wt")),
            RailMenuTarget::WorktreeOpenIn(PathBuf::from("/wt")),
            RailMenuTarget::Agent(1),
        ] {
            let rows = menu_rows(&target, Some("main"), 1, false, true);
            assert!(
                !matches!(rows.first(), Some(MenuRow::Separator)),
                "a leading divider is what a stripped title row leaves behind: {target:?}"
            );
            assert!(rows.iter().any(|row| matches!(row, MenuRow::Item(_))));
        }
    }

    #[test]
    fn the_archive_row_counts_the_worktrees_real_agents_and_disables_itself_at_zero() {
        let one = worktree_menu_groups(Some("main"), 1, false);
        assert_eq!(one[0][1].label, "Archive 1 agent");
        assert!(one[0][1].enabled);
        let none = worktree_menu_groups(Some("main"), 0, false);
        assert_eq!(none[0][1].label, "No agents to archive");
        assert!(!none[0][1].enabled);
        assert!(none[0][1].disabled_reason.is_some());
    }

    #[test]
    fn copy_branch_name_is_disabled_without_a_branch() {
        let detached = worktree_menu_groups(None, 1, false);
        let row = &detached[1][0];
        assert_eq!(row.action, RailMenuAction::CopyBranchName);
        assert!(!row.enabled);
        assert!(row.disabled_reason.is_some());
        assert!(worktree_menu_groups(Some("main"), 1, false)[1][0].enabled);
    }

    #[test]
    fn only_remove_worktree_is_destructive_and_arming_relabels_it() {
        let groups = worktree_menu_groups(Some("main"), 2, false);
        let destructive: Vec<&str> = groups
            .iter()
            .flatten()
            .filter(|entry| entry.destructive)
            .map(|entry| entry.label.as_str())
            .collect();
        assert_eq!(destructive, vec!["Remove worktree\u{2026}"]);

        let armed = worktree_menu_groups(Some("main"), 2, true);
        assert_eq!(armed.len(), groups.len());
        assert_eq!(armed[2].len(), 1, "arming must not add a row");
        assert_eq!(armed[2][0].label, "Remove worktree \u{2014} click again");
        assert!(armed[2][0].destructive);
    }

    #[test]
    fn the_agent_menu_is_open_pause_or_resume_and_one_archive_run() {
        assert_eq!(
            labels(&agent_menu_groups(true)),
            vec![vec!["Open", "Pause"], vec!["Archive run"]]
        );
        assert_eq!(
            labels(&agent_menu_groups(false)),
            vec![vec!["Open", "Resume"], vec!["Archive run"]]
        );
        for running in [true, false] {
            let groups = agent_menu_groups(running);
            let rows: Vec<&str> = groups
                .iter()
                .flatten()
                .map(|entry| entry.label.as_str())
                .collect();
            assert!(
                !rows.iter().any(|label| label.contains("Delete")),
                "there is no red Delete run - archive already names what survives"
            );
            assert!(
                groups.iter().flatten().all(|entry| !entry.destructive),
                "nothing on an agent row destroys anything: {rows:?}"
            );
            let archive = groups
                .iter()
                .flatten()
                .find(|entry| entry.action == RailMenuAction::ArchiveRun)
                .expect("every agent menu has Archive run");
            assert_eq!(archive.tooltip.as_deref(), Some(ARCHIVE_RUN_HINT));
        }
    }

    #[test]
    fn the_overflow_holds_history_and_settings_with_their_glyphs() {
        let groups = overflow_menu_groups(false);
        assert_eq!(labels(&groups), vec![vec!["History", "Settings"]]);
        assert_eq!(groups[0][0].glyph, Some(Icon::ClockCounterClockwise));
        assert_eq!(groups[0][1].glyph, Some(Icon::SlidersHorizontal));
        assert_eq!(
            groups[0][1].keystroke_spec,
            Some("mod+,"),
            "Settings has a real registered binding, so its row shows it"
        );
        assert!(groups[0][1].enabled);
        assert!(
            !groups[0][0].enabled && groups[0][0].disabled_reason.is_some(),
            "with no History surface to switch to, the row must say so rather than look live"
        );
        assert!(overflow_menu_groups(true)[0][0].enabled);
        // GitHub issue #227 built the History view, so the overflow's own row is now live -
        // \u{a7}7 rule 1: ship the affordance with the behaviour, or ship neither.
        const { assert!(HISTORY_VIEW_AVAILABLE) };
    }

    #[test]
    fn every_keycap_in_a_rail_menu_names_a_real_binding() {
        let bindings = crate::default_key_bindings();
        let mut all: Vec<MenuEntry<RailMenuAction>> = Vec::new();
        all.extend(
            worktree_menu_groups(Some("main"), 2, false)
                .into_iter()
                .flatten(),
        );
        all.extend(open_in_menu_groups().into_iter().flatten());
        all.extend(agent_menu_groups(true).into_iter().flatten());
        all.extend(agent_menu_groups(false).into_iter().flatten());
        all.extend(overflow_menu_groups(true).into_iter().flatten());
        for entry in all {
            let Some(spec) = entry.keystroke_spec else {
                continue;
            };
            let resolved = crate::keymap::resolve_combo(spec, false);
            assert!(
                bindings.iter().any(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|keystroke| crate::keymap::resolve_keystroke(keystroke.inner(), false))
                        .eq(std::iter::once(resolved.clone()))
                }),
                "{}'s keycap {spec} names no registered binding",
                entry.label
            );
        }
    }
}
