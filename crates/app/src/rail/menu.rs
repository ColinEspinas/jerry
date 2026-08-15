//! The rail's menus, as pure data (GitHub issue #290): which rows a worktree row, an agent row
//! and the `⋯` overflow each offer, and what running one of those rows means.
//!
//! GPUI-free, like every other `state`-shaped module in this folder - the row sets are exactly
//! what `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §4 lists, so they are worth
//! asserting directly, without a window. [`crate::rail::menu_render`] is the `impl AdeApp` half:
//! opening these menus off a real right-click, and running the actions below.
//!
//! Every row is drawn by the app's one shared menu ([`crate::menu`]) - this module only decides
//! *which* rows, exactly as `crate::sidebar::context_menu` does for the file tree.
//!
//! ## Where the row sets come from
//!
//! §4 is the newest word and outranks `STAGE-A-CHANGELOG.md` §4t's longer earlier draft (which
//! had `Open in Finder`/`Open in terminal` as two rows, plus `Rename branch…`):
//!
//! > **Context menus** on worktree rows (`New agent here`, `Archive N agents`, `Copy branch
//! > name`, `Copy path`, `Open in…`, `Remove worktree…`) and agent rows (`Open`, `Pause`,
//! > `Archive run`).
//!
//! §4u then removed the title row from all three menus ("the menu was captioning its own target.
//! You right-clicked the row; you can see it") and cut the overflow down to "History and Settings
//! only, with the glyphs they had in the strip".

use std::path::PathBuf;

use crate::icons::Icon;
use crate::menu::model::{MenuEntry, MenuRow};
use crate::root::plural;
use crate::work_surface::agents::AgentId;

/// What a rail menu is open *on*.
///
/// `WorktreeOpenIn` is a real second level of the worktree menu rather than a separate surface:
/// §4 collapsed §4t's two `Open in Finder`/`Open in terminal` rows into one `Open in…`, and an
/// ellipsis promises a further choice, so the row opens that choice - in the same popover
/// component, at the same anchor - instead of silently picking one of the two destinations.
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
///
/// §4u, verbatim: "**Anchored to the pointer**, not the row. Rows are 27px and the pointer is
/// what the user aimed with."
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
///
/// `agent_count` is how many live agents really sit in this worktree; at zero the Archive row
/// stays (so the menu's shape doesn't jump between worktrees) but says so and is disabled.
/// `branch` is `None` on a detached `HEAD`, where there is genuinely no branch name to copy.
/// `remove_armed` is the two-click confirmation's first click having landed on *this* worktree -
/// the same in-menu arming the git graph's own `Delete branch` row uses.
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
///
/// `running` picks between `Pause` and `Resume` - one row, not two with one of them permanently
/// dead, for the same reason §7 rule 3 collapsed Archive and Delete into one.
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
///
/// It does, as of GitHub issue #227: [`crate::rail::strip::SidebarView::History`] is a real
/// sidebar view - the repo → worktree → run index, with its own scope toggle and its own
/// run-transcript centre tab - reached through this overflow rather than through a cell, per
/// `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4t ("a permanent cell in a 5-cell
/// strip is a claim that you switch to it constantly. If you don't, it belongs in the overflow").
///
/// Kept as a named constant rather than collapsed into an ungated row: the row that reads it, the
/// reason it would show while disabled, and this fact are one thing, and §7 rule 1 ("Ship the
/// affordance with the behaviour, or ship neither") is the rule it exists to keep honest.
pub const HISTORY_VIEW_AVAILABLE: bool = true;

/// The sidebar strip's `⋯` overflow (§4u): "History and Settings only, with the glyphs they had
/// in the strip (clock, sliders) so the move out of the strip does not cost their
/// recognisability. Command palette, Keyboard shortcuts and About were filler - the palette has
/// `⌘K` and its own surface."
///
/// `history_available` is whether this build really has a History surface to switch to - see
/// [`HISTORY_VIEW_AVAILABLE`]. A build without one shows the row visibly disabled with that as its
/// reason, rather than as a row that looks live and does nothing.
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

    /// `REVISION-2026-08-14.md` §4's worktree list, in its groups, exactly.
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

    /// §4u: "**No title row.** The menu was captioning its own target." Held structurally - every
    /// row of every rail menu is a real action, so there is nowhere for a caption to hide.
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

    /// A detached `HEAD` has no branch name to copy - the row stays, and says why.
    #[test]
    fn copy_branch_name_is_disabled_without_a_branch() {
        let detached = worktree_menu_groups(None, 1, false);
        let row = &detached[1][0];
        assert_eq!(row.action, RailMenuAction::CopyBranchName);
        assert!(!row.enabled);
        assert!(row.disabled_reason.is_some());
        assert!(worktree_menu_groups(Some("main"), 1, false)[1][0].enabled);
    }

    /// `Remove worktree…` is the only destructive row in the rail, and arming it re-labels that
    /// same row rather than adding a second one.
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

    /// §4/§6's agent list: `Open`, one of `Pause`/`Resume`, then one `Archive run` carrying §6's
    /// hint verbatim - and never a `Delete run…` beside it (§7 rule 3).
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

    /// §4u: the overflow is History and Settings, in that order, each with the glyph it had in
    /// the strip - and nothing else.
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

    /// Every row that shows a keycap must name a real, registered binding
    /// (`crate::default_key_bindings`) - the one rule a hand-typed keycap breaks silently.
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
