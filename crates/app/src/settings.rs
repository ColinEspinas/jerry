//! The Settings surface's pure data model - `design_handoff_jerry_ade/README.md`'s "Settings"
//! section: "a separate surface, not a modal: it replaces the three zones while the title bar
//! and status bar stay." Mirrors `crate::rail`/`crate::palette`/`crate::work_surface`'s own
//! split: only the mapping from already-real app state (which agent binaries are actually on
//! `$PATH`, the real worktree list Phase B already built) to what a settings row should show
//! lives here, directly unit-testable without a live GPUI window; turning the result into
//! actual `gpui::Div` trees happens in `crate::root`, which owns the `Context<AdeApp>` real
//! actions (opening a worktree, pruning) need.
//!
//! ## Which pages are real
//!
//! `design_handoff_jerry_ade/README.md` is explicit: "Pages: General · **Agents** ·
//! **Worktrees** · Keymap · Editor · Language servers · Theme · Notifications · Integrations ·
//! About. Agents and Worktrees are designed; the rest are nav-only in this mockup." This module
//! (and `crate::root`'s rendering of it) takes that at face value: every page listed gets real,
//! working navigation ([`SettingsPage::ALL`], [`nav_groups`]), but only [`SettingsPage::Agents`]
//! and [`SettingsPage::Worktrees`] render real content sourced from live app state. Every other
//! page renders an honest "not designed in this mockup" placeholder - `Jerry.dc.html`'s own
//! `setStub` state's exact real copy (line ~705: `not designed in this mockup`), not a
//! fabricated settings UI for toggles/steppers this app has no backing implementation for. This
//! is a documented act of fidelity to the source design (which itself never specified what
//! those pages should contain), not a shortcut: inventing plausible-looking "Font size" or
//! "Theme" controls that don't actually change anything would be exactly the "component bound
//! to nothing" this project's constraints forbid.
//!
//! ## Why the Agents/Worktrees `setRows` "Behaviour"/"Policy" toggle sections are left out
//!
//! `Jerry.dc.html`'s `settingsRows.agents`/`settingsRows.worktrees` fixtures (a "Plan before
//! editing" toggle, a "Max parallel sessions" stepper, a "Worktree root" path field, and so on)
//! are sample *settings values* - this app has no real settings-persistence layer at all (no
//! `settings.toml` reader/writer, no in-memory settings store) to back a toggle that would
//! actually do anything if flipped. Rendering them anyway - even wired to a plain in-memory
//! `bool` that nothing else reads - would be exactly the kind of decorative, bound-to-nothing
//! control this project's constraints forbid. Only the two sections this phase can back with
//! real, already-loaded application state (the Installed agents card, the Disk worktrees card)
//! are built; see `crate::root`'s Settings render methods for where those real data sources are.

use std::path::PathBuf;

use crate::rail::WorktreeNote;
use crate::sessions::SessionKind;

/// Every real page `design_handoff_jerry_ade/README.md`'s Settings nav lists, in the exact
/// left-to-right, top-to-bottom order the design's three nav groups present them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Agents,
    Worktrees,
    Keymap,
    Editor,
    LanguageServers,
    Theme,
    Notifications,
    Integrations,
    About,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 10] = [
        SettingsPage::General,
        SettingsPage::Agents,
        SettingsPage::Worktrees,
        SettingsPage::Keymap,
        SettingsPage::Editor,
        SettingsPage::LanguageServers,
        SettingsPage::Theme,
        SettingsPage::Notifications,
        SettingsPage::Integrations,
        SettingsPage::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsPage::General => "General",
            SettingsPage::Agents => "Agents",
            SettingsPage::Worktrees => "Worktrees",
            SettingsPage::Keymap => "Keymap",
            SettingsPage::Editor => "Editor",
            SettingsPage::LanguageServers => "Language servers",
            SettingsPage::Theme => "Theme",
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
            SettingsPage::Keymap => "keymap",
            SettingsPage::Editor => "editor",
            SettingsPage::LanguageServers => "lsp",
            SettingsPage::Theme => "theme",
            SettingsPage::Notifications => "notifications",
            SettingsPage::Integrations => "integrations",
            SettingsPage::About => "about",
        }
    }

    /// Whether this page has real, live-state-backed content - see the module docs' "Which
    /// pages are real" section. Every other page is honestly nav-only.
    pub fn is_implemented(self) -> bool {
        matches!(self, SettingsPage::Agents | SettingsPage::Worktrees)
    }

    /// The content column's one-line rationale under the page title
    /// (`design_handoff_jerry_ade/README.md`'s "Content column" section). Real, specific text
    /// for the two implemented pages (rewritten from `Jerry.dc.html`'s own `settingsMeta`
    /// fixture to drop the parts describing the Behaviour/Policy toggle sections this app
    /// doesn't build - see the module docs). This subtitle itself is app-authored explanatory
    /// text, not copy from the mockup - `Jerry.dc.html` has no per-page subtitle fixture at all
    /// for nav-only pages, so every nav-only page shares the same honest, app-written
    /// "not designed in this mockup" explanation below it. (The placeholder page *body*,
    /// separately, in `crate::root::render_settings_placeholder_page`, is the mockup's actual
    /// verbatim `setStub` copy - see that function's docs; this subtitle is not that.)
    pub fn subtitle(self) -> &'static str {
        match self {
            SettingsPage::Agents => {
                "Which agent binaries Jerry can actually find on PATH right now - detected live, not configured."
            }
            SettingsPage::Worktrees => {
                "Every session gets its own worktree. This is where they live, their real disk usage, and what's safe to prune."
            }
            _ => {
                "Not designed in this mockup - design_handoff_jerry_ade/README.md scopes only \
                 Agents and Worktrees to real content for this phase."
            }
        }
    }
}

/// One of the Settings nav's three grouped sections (`design_handoff_jerry_ade/README.md`:
/// "Groups (Workspace, Editor, Other)").
pub struct NavGroup {
    pub label: &'static str,
    pub pages: Vec<SettingsPage>,
}

/// The real, fixed nav structure - `Jerry.dc.html`'s own `settingsNavDefs` grouping, unchanged
/// (every page listed there is included here, in the same order), so every page the design
/// lists is real, clickable navigation even though only two pages render real content past that
/// point.
pub fn nav_groups() -> Vec<NavGroup> {
    vec![
        NavGroup {
            label: "Workspace",
            pages: vec![
                SettingsPage::General,
                SettingsPage::Agents,
                SettingsPage::Worktrees,
                SettingsPage::Keymap,
            ],
        },
        NavGroup {
            label: "Editor",
            pages: vec![
                SettingsPage::Editor,
                SettingsPage::LanguageServers,
                SettingsPage::Theme,
            ],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rail::WorktreeNote;
    use wt_core::diff::WorktreeMergeStatus;

    #[test]
    fn all_ten_pages_are_covered_by_the_three_nav_groups_exactly_once() {
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
    fn only_agents_and_worktrees_are_implemented() {
        for page in SettingsPage::ALL {
            let expected = matches!(page, SettingsPage::Agents | SettingsPage::Worktrees);
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
        let placeholder = SettingsPage::General.subtitle();
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
}
