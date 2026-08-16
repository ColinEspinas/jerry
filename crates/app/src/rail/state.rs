//! The agent rail's data model: pure, GPUI-free types and functions for grouping and
//! filtering (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md`'s Zone 1). No `gpui`
//! dependency, so this logic is unit-testable without a real window, terminal, or git state.
//! `crate::root` gathers the real signals (`TerminalPane`, `wt_core::list_worktrees`,
//! `wt_core::diff::diff_against_base`) into the plain types this module operates on, and renders
//! the result as GPUI elements.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::rail::repo::RepoId;
use crate::rail::status::Status;
use crate::root::plural;
use crate::work_surface::agents::ProcessKind;
use wt_core::diff::{AheadBehind, DiffLineKind, WorktreeDiff, WorktreeMergeStatus};

/// One agent, reduced to exactly what the rail row needs to render - built in `crate::root`
/// from a `crate::work_surface::agents::Agent` plus a `wt_core::diff::diff_against_base` result for its
/// worktree. See `crate::rail::status::derive_status` for how `status` was computed.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub id: crate::work_surface::agents::AgentId,
    pub kind: ProcessKind,
    pub title: String,
    pub cwd: PathBuf,
    pub status: Status,
    pub branch: Option<String>,
    /// `+added -deleted` line counts from `wt_core::diff::diff_against_base`, summed across
    /// every changed file. Both `0` if the diff hasn't loaded yet or there are no changes.
    pub add: usize,
    pub del: usize,
    /// The process exit code, only for [`Status::Fail`]/[`Status::Review`]/exited-`Idle`
    /// rows. `None` while still running or never started.
    pub exit_code: Option<u32>,
    /// The agent row's "what it is doing" trailing text for a [`Status::Run`] row -
    /// `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.3: "the live tool call -
    /// `writing auth.rs`, `editing reports.rs`, `bench 3 of 5`, `148 of 312`".
    pub activity: Option<String>,
    /// Wall-clock time since `crate::work_surface::agents::Agent::spawned_at` - the agent
    /// row's line-1 elapsed time (§2.3: "elapsed 9.5px mono right"). See [`format_elapsed`] for
    /// how this becomes the rendered `4m`/`1h` text.
    pub elapsed: Duration,
    /// How many files **this agent** has changed since its own review baseline, for a
    /// [`Status::Review`] row only - §2.3's trailing text for that state (`12 files`, rendered
    /// beside the `finished` state word). `None` for every other status (§2.3: "Do not put a
    /// per-agent file count here" - this one state is the documented exception).
    pub review_file_count: Option<usize>,
}

impl AgentRow {
    /// Whether this row matches a rail filter query - case-insensitive substring match
    /// against the title, branch name, and agent kind label.
    pub fn matches_filter(&self, query: &str) -> bool {
        let query = query.trim();
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        self.title.to_lowercase().contains(&query)
            || self.kind.label().to_lowercase().contains(&query)
            || self
                .branch
                .as_deref()
                .is_some_and(|branch| branch.to_lowercase().contains(&query))
    }
}

/// Filters `rows` down to those matching `query` - see [`AgentRow::matches_filter`]. A
/// blank query matches everything.
pub fn filter_agents<'a>(rows: &'a [AgentRow], query: &str) -> Vec<&'a AgentRow> {
    rows.iter()
        .filter(|row| row.matches_filter(query))
        .collect()
}

/// `+added -deleted` totals for one worktree/agent cwd, summed across every changed file's
/// hunks via [`sum_diff_stat`]. `has_changes` mirrors `crate::rail::status::derive_status`'s
/// `has_reviewable_diff` input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffSummary {
    pub add: usize,
    pub del: usize,
    pub has_changes: bool,
}

/// Sums added/deleted line counts across every hunk of every file in a [`WorktreeDiff`] -
/// the full line-level diff is already loaded by `wt_core::diff::diff_against_base`, so this
/// avoids a second, redundant `git diff --stat` invocation.
pub fn sum_diff_stat(diff: &WorktreeDiff) -> (usize, usize) {
    let mut add = 0usize;
    let mut del = 0usize;
    for file in &diff.files {
        for hunk in &file.hunks {
            for line in &hunk.lines {
                match line.kind {
                    DiffLineKind::Added => add += 1,
                    DiffLineKind::Removed => del += 1,
                    DiffLineKind::Context => {}
                }
            }
        }
    }
    (add, del)
}

/// Real per-status counts across every agent row, in [`Status::ORDER`] - the status bar's
/// five urgency-counter squares (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 7).
/// Unlike [`group_worktrees_by_repo`], a status with zero matching rows still gets a real `0`
/// entry rather than being omitted, since the status bar always shows all five squares. Built
/// from the
/// same per-agent [`Status`] every [`AgentRow`] already carries - not a second, independent
/// status classification.
pub fn urgency_counts(rows: &[AgentRow]) -> [(Status, usize); 5] {
    Status::ORDER.map(|status| {
        (
            status,
            rows.iter().filter(|row| row.status == status).count(),
        )
    })
}

/// Clean/merged state for one worktree row in "by project" mode - computed from
/// `wt_core::is_dirty` and `wt_core::diff::merge_status_against_base`. See [`Self::label`]
/// for how this becomes the `checkout · clean` / `merged HH:MM · prunable` text.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeNote {
    /// The main checkout is always labeled `checkout`, never `merged ... prunable` - `git
    /// worktree remove` refuses it outright, and it can't be "merged into" its own base.
    pub is_main: bool,
    /// `None` if `wt_core::is_dirty` itself failed (a blank note rather than a guess).
    pub clean: Option<bool>,
    /// `None` if no base branch could be detected (mirrors `DiffBase::NoBaseFound`) - such a
    /// worktree is never treated as prunable, since "merged into what?" has no answer.
    pub merge: Option<WorktreeMergeStatus>,
    /// From `wt_core::Worktree::is_locked` - a locked worktree is never offered as prunable
    /// even if otherwise merged and clean, since `git worktree lock` is an explicit "don't
    /// touch this" signal. See [`Self::is_prunable`].
    pub is_locked: bool,
}

impl WorktreeNote {
    /// A prune candidate: not the main checkout, not locked, clean (mirroring
    /// `wt_core::remove_worktree`'s own dirty-tree refusal up front), and merged into its
    /// detected base.
    pub fn is_prunable(&self) -> bool {
        !self.is_main
            && !self.is_locked
            && self.clean == Some(true)
            && self.merge.as_ref().is_some_and(|status| status.merged)
    }

    /// The real note text shown on an agent-less worktree row.
    pub fn label(&self) -> String {
        let locked_suffix = if self.is_locked { " · locked" } else { "" };

        if self.is_main {
            return match self.clean {
                Some(true) => format!("checkout · clean{locked_suffix}"),
                Some(false) => format!("checkout · dirty{locked_suffix}"),
                None => format!("checkout{locked_suffix}"),
            };
        }

        let merged_and_clean =
            self.clean == Some(true) && self.merge.as_ref().is_some_and(|status| status.merged);
        if merged_and_clean {
            let time = self
                .merge
                .as_ref()
                .and_then(|status| status.head_committer_unix_seconds)
                .map(format_utc_hhmm)
                .unwrap_or_else(|| "--:--".to_string());
            // Still real information even when locked: it's genuinely merged and clean,
            // just not offered as a prune candidate while the lock is held.
            let tail = if self.is_locked { "locked" } else { "prunable" };
            return format!("merged {time} · {tail}");
        }

        match self.clean {
            Some(true) => format!("clean{locked_suffix}"),
            Some(false) => format!("dirty{locked_suffix}"),
            None if self.is_locked => "locked".to_string(),
            None => String::new(),
        }
    }
}

/// Formats a Unix timestamp as `HH:MM` **in UTC**, not the viewer's local timezone -
/// deliberate: `std` has no timezone database, and pulling one in (`chrono-tz`, or the
/// `time` crate's `local-offset` feature, unsound-by-default on unix) wasn't worth it for a
/// single label.
pub(crate) fn format_utc_hhmm(unix_seconds: i64) -> String {
    let seconds_in_day = unix_seconds.rem_euclid(86_400);
    let hours = seconds_in_day / 3600;
    let minutes = (seconds_in_day % 3600) / 60;
    format!("{hours:02}:{minutes:02}")
}

/// Formats a real elapsed [`Duration`] as the rail's short `Ns`/`Nm`/`Nh` label - the agent row's
/// line-1 elapsed time and the `paused` state word's `resumable · Nh` trailing text (§2.3).
/// Whole units only (no `4m 12s`), matching the mockup's own `4m`/`1m`/`3h` examples.
pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// One worktree, as input to [`build_worktree_rows`] - `crate::root`'s reduction of
/// `wt_core::WorktreeResult` (via `crate::rail::worktrees::WorktreeItem`) plus its separately
/// computed [`WorktreeNote`].
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    pub note: WorktreeNote,
    /// `Some(message)` if this worktree's metadata failed to read (mirrors
    /// `crate::rail::worktrees::WorktreeItem::error`) - kept as a visible error row rather than
    /// filtered out, per that type's documented intent.
    pub error: Option<String>,
}

/// One rail row: a single worktree, with **every** agent currently open in it (not just the
/// first one found) folded in as tabs - the real "one worktree = one rail entry, N agents =
/// N tabs" model this revision introduces, replacing the old `ProjectChild` shape whose
/// `agents.iter().find(...)` silently hid every agent past the first in the same worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeRow {
    pub path: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    /// Clean/merged note - only meaningful (and only ever shown) when [`Self::agents`] is
    /// empty; a worktree with an open agent shows its agents' own real status instead.
    pub note: WorktreeNote,
    /// `Some(message)` if this worktree's metadata failed to read - see [`WorktreeEntry::error`]'s
    /// own docs; a worktree row in this state is never interactive.
    pub error: Option<String>,
    /// Every agent currently open in this worktree, in tab-strip order (creation order,
    /// matching `crate::work_surface::agents::Agents::iter_for_cwd`).
    pub agents: Vec<AgentRow>,
    /// This worktree's real, persisted-but-not-currently-running agents (GitHub issue #227),
    /// most recently active first - see [`crate::hooks::history::past_agents_for_worktree`] for
    /// how "currently running" is excluded. Empty for a worktree with no persisted history, which
    /// the rail renders as no section at all (no empty-state clutter - see
    /// `crate::rail::render::AdeApp::render_worktree_row`).
    pub history: Vec<crate::hooks::history::PastAgent>,
}

impl WorktreeRow {
    /// The aggregate status shown on this row: the most urgent status among its open agents
    /// (`Status::urgency_rank`, lower = more urgent - the same ranking the old
    /// `status_dot_cluster` already used to sort a worktree's per-agent dots), or
    /// [`Status::Idle`] when no agent is open at all - mirroring
    /// `crate::rail::status::derive_status`'s own `ProcessSignal::NoProcess => Status::Idle`, since
    /// "no process running" is exactly what an agent-less worktree is.
    pub fn aggregate_status(&self) -> Status {
        self.agents
            .iter()
            .map(|row| row.status)
            .min_by_key(|status| status.urgency_rank())
            .unwrap_or(Status::Idle)
    }

    /// This row's rank for sorting *inside* a repo group (and, via [`RepoGroup::rank`], the
    /// groups themselves) - `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.1's
    /// full seven-step order: `input → failed → review → running → idle → bare → prunable`.
    /// Lower sorts first (more urgent).
    pub fn urgency_rank(&self) -> u8 {
        if self.agents.is_empty() {
            if self.note.is_prunable() {
                6
            } else {
                5
            }
        } else {
            self.aggregate_status().urgency_rank()
        }
    }

    /// The real `+added -deleted` totals summed across every open agent's own diff summary -
    /// double-counting is impossible since every agent in [`Self::agents`] shares this same
    /// worktree's `cwd`, so they'd all report the identical per-worktree diff anyway; this just
    /// reads the first one rather than literally summing duplicates.
    pub fn diff_totals(&self) -> (usize, usize) {
        self.agents
            .first()
            .map(|row| (row.add, row.del))
            .unwrap_or((0, 0))
    }

    /// This row's diffstat as its two **separately coloured parts**, or `None` when there is no
    /// real diff to show - see [`diff_stat_parts`], which this is [`Self::diff_totals`] fed into.
    pub fn diff_stat_parts(&self) -> Option<(String, Option<String>)> {
        let (add, del) = self.diff_totals();
        diff_stat_parts(add, del)
    }

    /// Whether any agent open in this worktree currently holds `status` - the exact granularity
    /// the repo header's two urgency counts are defined at (see [`RepoGroup::failed_count`]), as
    /// opposed to [`Self::aggregate_status`]'s single most-urgent answer. A worktree holding one
    /// asking agent *and* one failed agent genuinely holds both facts; collapsing it to one
    /// status first is what made the old single amber count claim it was only asking.
    pub fn has_agent_with(&self, status: Status) -> bool {
        self.agents.iter().any(|agent| agent.status == status)
    }

    /// Whether this row matches a rail filter query - its own label/branch/path (see
    /// [`matches_filter_worktree_entry`]) or any of its open agents' own title/branch/kind
    /// (see [`AgentRow::matches_filter`]).
    pub fn matches_filter(&self, query: &str) -> bool {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return true;
        }
        let entry_matches = {
            let query = trimmed.to_lowercase();
            self.label.to_lowercase().contains(&query)
                || self
                    .branch
                    .as_deref()
                    .is_some_and(|branch| branch.to_lowercase().contains(&query))
                || self.path.to_string_lossy().to_lowercase().contains(&query)
        };
        entry_matches || self.agents.iter().any(|row| row.matches_filter(trimmed))
    }
}

/// Builds one [`WorktreeRow`] per worktree, in the given order, folding in **every** agent
/// whose `cwd` matches that worktree's path (not just the first one - the real fix for the bug
/// the old `ProjectChild`-based `build_project_children` had: `agents.iter().find(...)` only
/// ever surfaced one agent per worktree, silently hiding any additional ones). Every worktree
/// appears here, including ones with no agent (e.g. `main`, or a merged/prunable leftover).
pub fn build_worktree_rows(worktrees: &[WorktreeEntry], agents: &[AgentRow]) -> Vec<WorktreeRow> {
    build_worktree_rows_with_history(worktrees, agents, &[])
}

/// [`build_worktree_rows`], plus GitHub issue #227's history rows: every entry in `history` is
/// folded into whichever [`WorktreeRow`] its own [`crate::hooks::history::PastAgent::worktree`]
/// matches, exactly the way `agents` already is. `history` is expected to already be filtered to
/// "genuinely past" (see [`crate::hooks::history::past_agents_for_worktree`]) - this function only
/// groups by path, it does not re-derive which records are live.
pub fn build_worktree_rows_with_history(
    worktrees: &[WorktreeEntry],
    agents: &[AgentRow],
    history: &[crate::hooks::history::PastAgent],
) -> Vec<WorktreeRow> {
    worktrees
        .iter()
        .map(|worktree| {
            let agents: Vec<AgentRow> = agents
                .iter()
                .filter(|agent| agent.cwd == worktree.path)
                .cloned()
                .collect();
            let history: Vec<crate::hooks::history::PastAgent> = history
                .iter()
                .filter(|past| past.worktree == worktree.path)
                .cloned()
                .collect();
            WorktreeRow {
                path: worktree.path.clone(),
                label: worktree.label.clone(),
                branch: worktree.branch.clone(),
                note: worktree.note.clone(),
                error: worktree.error.clone(),
                agents,
                history,
            }
        })
        .collect()
}

/// Filters a [`WorktreeRow`] list down to those matching `query` - applied *after*
/// [`build_worktree_rows`], so which worktrees have open agents folded in is always decided
/// from the complete, unfiltered agent list first.
pub fn filter_worktree_rows<'a>(rows: &'a [WorktreeRow], query: &str) -> Vec<&'a WorktreeRow> {
    rows.iter()
        .filter(|row| row.matches_filter(query))
        .collect()
}

/// One repo, with its already-built [`WorktreeRow`]s - `crate::root`'s reduction of one
/// [`crate::rail::repo::Repo`] plus its own live worktree rows (the focused repo's come from
/// [`crate::rail::render::AdeApp::build_worktree_rows`], every other repo's from its own
/// [`crate::rail::repo::Repo::worktrees`] - see [`crate::rail::render::AdeApp::
/// build_repo_groups`]'s own docs), as input to [`group_worktrees_by_repo`].
#[derive(Debug, Clone, PartialEq)]
pub struct RepoWorktrees {
    pub repo_id: RepoId,
    pub repo_name: String,
    /// This repo's real, complete worktree list - unaffected by the rail's filter box. The
    /// source of the group header's `N wt` and `N worktrees waiting` counts
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.0: "a repo you have
    /// scrolled past still reports that something in it wants a human" - a header that shrank
    /// or grew as the *filter box* was typed into would break that same promise for the repo
    /// you're currently looking at). See [`Self::rows`] for the separate, filtered list.
    pub all_rows: Vec<WorktreeRow>,
    /// The rows actually rendered/expanded under this repo's header - may be narrower than
    /// [`Self::all_rows`] when the rail's filter box has a query in it. Deliberately **not**
    /// read by [`RepoGroup::waiting_count`] or the header's `N wt` count: which rows are
    /// currently visible on screen is a real, separate UI concern from what the header reports
    /// about the repo's real state.
    pub rows: Vec<WorktreeRow>,
    /// Whether `all_rows`/`rows` reflect this repo's real, live worktree data - always `true` for
    /// the focused repo (its own data path predates and is unaffected by per-repo loading), and
    /// for any other repo mirrors [`crate::rail::repo::Repo::worktrees_loaded`]: `true` once a
    /// real `wt_core::list_worktrees_porcelain` fetch for it has completed at least once
    /// (`crate::root::AdeApp::load_repo_worktrees`/`crate::root::AdeApp::
    /// start_repo_worktrees_polling`), `false` only in the brief window before that first fetch
    /// resolves (e.g. immediately after `crate::root::AdeApp::add_repo`). An empty `all_rows`
    /// with `rows_loaded: false` means "unpopulated" - a repo that may genuinely have several
    /// worktrees on disk that just haven't been fetched yet - and must never be rendered the same
    /// way as an empty `all_rows` with `rows_loaded: true`, which really does mean zero
    /// worktrees.
    pub rows_loaded: bool,
}

/// One repo group in the rail - `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md`
/// §2.0-2.1: the rail's only grouping axis, always present (even for a single repo), with its
/// own worktree rows ranked most-urgent-first inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoGroup {
    pub repo_id: RepoId,
    pub repo_name: String,
    /// This repo's real, complete worktree list, ranked by [`WorktreeRow::urgency_rank`] -
    /// see [`RepoWorktrees::all_rows`]. Header counts ([`Self::waiting_count`], the `N wt`
    /// text at the render site) always read this field, never [`Self::rows`].
    pub all_rows: Vec<WorktreeRow>,
    /// The rows to actually render/expand below the header - see [`RepoWorktrees::rows`].
    /// Ranked by [`WorktreeRow::urgency_rank`], most urgent first - see
    /// [`group_worktrees_by_repo`].
    pub rows: Vec<WorktreeRow>,
    /// See [`RepoWorktrees::rows_loaded`] - carried through unchanged by
    /// [`group_worktrees_by_repo`]. The render side must consult this before treating an empty
    /// `all_rows` as a real "zero worktrees" claim.
    pub rows_loaded: bool,
}

impl RepoGroup {
    /// The group header's **red** urgency count: worktrees in this repo holding at least one
    /// failed agent, hidden at zero.
    pub fn failed_count(&self) -> usize {
        self.all_rows
            .iter()
            .filter(|row| row.has_agent_with(Status::Fail))
            .count()
    }

    /// The group header's **amber** urgency count: worktrees holding at least one asking agent
    /// **and no failed one**, hidden at zero.
    pub fn needs_input_count(&self) -> usize {
        self.all_rows
            .iter()
            .filter(|row| !row.has_agent_with(Status::Fail) && row.has_agent_with(Status::Ask))
            .count()
    }

    /// This group's own rank for ordering groups - its most urgent worktree's
    /// [`WorktreeRow::urgency_rank`] (§2.0: "repos are ordered by their own most urgent
    /// worktree, using the same rank function as the rows"). Reads [`Self::all_rows`] for the
    /// same reason [`Self::waiting_count`] does: group order must not reshuffle just because the
    /// filter box narrowed [`Self::rows`]. A worktree-less repo (no data loaded for it yet -
    /// see [`group_worktrees_by_repo`]'s docs) sorts last, behind every repo that has at least
    /// one real row.
    fn rank(&self) -> u8 {
        self.all_rows
            .iter()
            .map(WorktreeRow::urgency_rank)
            .min()
            .unwrap_or(u8::MAX)
    }
}

/// One flattened row in the rail's Worktrees body - the real fix for the rail becoming
/// unresponsive to hover with many worktrees/agents open (live user report, GitHub issue #364).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailListItem {
    /// One repo group's header band (name, `N wt`, urgency counts).
    RepoHeader { group_index: usize },
    /// The inline "not loaded yet" / "no worktrees open yet" / "no worktrees match this filter"
    /// message shown in place of a repo group's rows when it has none to show.
    RepoEmptyMessage { group_index: usize },
    /// One worktree row's own header band - always present, whether or not it is expanded.
    WorktreeRow {
        group_index: usize,
        row_index: usize,
    },
    /// One open agent under an expanded worktree row, in the same tab-strip order
    /// [`WorktreeRow::agents`] already carries. Never emitted for a collapsed row or an errored
    /// one (an errored [`WorktreeRow`] renders only its [`Self::WorktreeRow`] item - see that
    /// row's own `error` field docs: it is never interactive and never shows children).
    AgentRow {
        group_index: usize,
        row_index: usize,
        agent_index: usize,
    },
    /// The `\u{21ba} N earlier runs` line under a worktree row (GitHub issue #227,
    /// `REVISION-2026-08-13.md` \u{a7}6).
    EarlierRunsLink {
        group_index: usize,
        row_index: usize,
    },
}

impl RailListItem {
    /// Whether this is the visually last item in its worktree's own block - the one real
    /// consumer being the 7px gap `crate::rail::render::AdeApp::render_worktree_row` used to
    /// paint once, on the div wrapping the whole block (header plus its expanded children).
    /// Flattened, each worktree's block is a variable number of separate list items rather than
    /// one wrapping div, so the gap moves onto whichever item now actually paints last - the
    /// [`Self::WorktreeRow`] itself when collapsed or childless, otherwise the last
    /// [`Self::AgentRow`] or, on a worktree with no live agent but real history, its own
    /// [`Self::EarlierRunsLink`].
    pub fn is_last_in_worktree_block(&self, groups: &[RepoGroup], expanded: bool) -> bool {
        let row = match self {
            RailListItem::RepoHeader { .. } | RailListItem::RepoEmptyMessage { .. } => {
                return false;
            }
            RailListItem::WorktreeRow {
                group_index,
                row_index,
            } => match groups
                .get(*group_index)
                .and_then(|g| g.rows.get(*row_index))
            {
                Some(row) => row,
                None => return false,
            },
            RailListItem::AgentRow {
                group_index,
                row_index,
                agent_index,
            } => {
                let Some(row) = groups
                    .get(*group_index)
                    .and_then(|g| g.rows.get(*row_index))
                else {
                    return false;
                };
                // No `history.is_empty()` term any more: a worktree with a live agent never gets
                // an `EarlierRunsLink` at all (GitHub issue #227 / \u{a7}6's own gate), so the last
                // agent row really is the last item in the block.
                return row.error.is_none() && *agent_index + 1 == row.agents.len();
            }
            // Always last when present: it is emitted after the row's children, and only for a
            // worktree that has none.
            RailListItem::EarlierRunsLink {
                group_index,
                row_index,
            } => {
                return groups
                    .get(*group_index)
                    .and_then(|g| g.rows.get(*row_index))
                    .is_some_and(|row| row.error.is_none());
            }
        };
        // A worktree row is last in its own block unless something follows it. Exactly two things
        // can, and they are mutually exclusive by construction (see [`flatten_rail_list_items`]):
        // its own agent rows, when it is expanded and has any, and - only on a worktree with no
        // live agent - its `\u{21ba} N earlier runs` line.
        let agent_rows_follow = expanded && !row.agents.is_empty();
        let earlier_runs_line_follows = row.agents.is_empty() && !row.history.is_empty();
        row.error.is_none() && !agent_rows_follow && !earlier_runs_line_follows
    }
}

/// Flattens `groups` into the real sequence [`crate::rail::render::AdeApp::render_rail_list`]'s
/// `gpui::list` renders - see [`RailListItem`]'s own docs for why this exists at all.
pub fn flatten_rail_list_items(
    groups: &[RepoGroup],
    mut expanded: impl FnMut(&WorktreeRow) -> bool,
) -> Vec<RailListItem> {
    let mut items = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        items.push(RailListItem::RepoHeader { group_index });
        if group.rows.is_empty() {
            items.push(RailListItem::RepoEmptyMessage { group_index });
            continue;
        }
        for (row_index, row) in group.rows.iter().enumerate() {
            items.push(RailListItem::WorktreeRow {
                group_index,
                row_index,
            });
            // Mirrors `crate::rail::render::AdeApp::render_worktree_row`'s own early return for
            // an errored row: no agents, no history, whatever the real data says.
            if row.error.is_some() {
                continue;
            }
            // GitHub issue #227: history is no longer one of a row's children, so "has children"
            // is back to meaning exactly "has live agents" - which is also what the disclosure
            // caret means again.
            if !row.agents.is_empty() && expanded(row) {
                for agent_index in 0..row.agents.len() {
                    items.push(RailListItem::AgentRow {
                        group_index,
                        row_index,
                        agent_index,
                    });
                }
            }
            // \u{a7}6's line, and \u{a7}6's gate: only on a worktree with no live agent, and
            // outside the expansion gate, because it sits *under* the row rather than inside it.
            if row.agents.is_empty() && !row.history.is_empty() {
                items.push(RailListItem::EarlierRunsLink {
                    group_index,
                    row_index,
                });
            }
        }
    }
    items
}

/// The tooltip on the repo header's **amber** dot+count pair: `"2 worktrees here need input"`.
pub fn needs_input_tooltip(count: usize) -> String {
    format!(
        "{} here {} input",
        plural::count(count, "worktree", None),
        plural::form(count, "needs", "need")
    )
}

/// The tooltip on the repo header's **red** dot+count pair: `"1 worktree here has a failed
/// agent"` / `"2 worktrees here have failed agents"`. See [`needs_input_tooltip`] for why the
/// sentence lives in a tooltip at all, and for the conjugation rule.
pub fn failed_tooltip(count: usize) -> String {
    format!(
        "{} here {} failed {}",
        plural::count(count, "worktree", None),
        plural::form(count, "has a", "have"),
        plural::form(count, "agent", "agents")
    )
}

/// A rail worktree row's diffstat, as the two parts it is **coloured** in - `("+152",
/// Some("−11"))` - or `None` when there is no real diff to show.
pub fn diff_stat_parts(add: usize, del: usize) -> Option<(String, Option<String>)> {
    if add == 0 && del == 0 {
        return None;
    }
    Some((
        format!("+{add}"),
        (del > 0).then(|| format!("\u{2212}{del}")),
    ))
}

/// The rail footer's and Settings → Disk's shared idle line: `"3 worktrees · 1.2 GB"`.
pub fn worktree_disk_label(worktree_count: usize, disk_label: &str) -> String {
    format!(
        "{} \u{b7} {disk_label}",
        plural::count(worktree_count, "worktree", None)
    )
}

/// The rail footer's prune control's tooltip - `"Prune merged worktrees \u{2014} 1 prunable,
/// frees 214 MB"`.
pub fn prune_tooltip(prunable_count: usize, freed_bytes: Option<(u64, bool)>) -> String {
    if prunable_count == 0 {
        return "Prune merged worktrees \u{2014} nothing prunable".to_string();
    }
    let freed = match freed_bytes {
        Some((bytes, truncated)) => {
            let suffix = if truncated { "+" } else { "" };
            format!(", frees {}{suffix}", format_bytes(bytes))
        }
        None => String::new(),
    };
    format!(
        "Prune merged worktrees \u{2014} {} prunable{freed}",
        prunable_count
    )
}

/// The prune control's armed-state tooltip - the second half of the two-click arm/confirm the
/// icon inherits unchanged from the text button it replaced.
pub fn prune_armed_tooltip(prunable_count: usize) -> String {
    format!(
        "Click again to remove {}",
        plural::count(prunable_count, "worktree", None)
    )
}

/// The prune button's arming prompt: `"click prune again to remove 2 worktrees"`.
pub fn prune_confirm_label(candidate_count: usize) -> String {
    format!(
        "click prune again to remove {}",
        plural::count(candidate_count, "worktree", None)
    )
}

/// The in-flight prune status: `"pruning 2 worktrees…"`.
pub fn pruning_label(candidate_count: usize) -> String {
    format!(
        "pruning {}\u{2026}",
        plural::count(candidate_count, "worktree", None)
    )
}

/// The finished-prune status: `"pruned 1 worktree"`.
pub fn pruned_label(removed_count: usize) -> String {
    format!("pruned {}", plural::count(removed_count, "worktree", None))
}

/// Builds the rail's repo groups: each [`RepoWorktrees`]' rows sorted by
/// [`WorktreeRow::urgency_rank`] (§2.1's "worktrees are ranked by their most urgent agent"), then
/// the groups themselves sorted by [`RepoGroup::rank`] (§2.0's "repos are ordered by their own
/// most urgent worktree") - **never** alphabetically or by insertion order, per that section's
/// explicit warning. Both sorts are stable (`slice::sort_by_key`), so two rows/groups tied on
/// rank keep their caller-supplied relative order rather than reshuffling on every render.
pub fn group_worktrees_by_repo(repos: Vec<RepoWorktrees>) -> Vec<RepoGroup> {
    let mut groups: Vec<RepoGroup> = repos
        .into_iter()
        .map(|repo| {
            let mut all_rows = repo.all_rows;
            all_rows.sort_by_key(WorktreeRow::urgency_rank);
            let mut rows = repo.rows;
            rows.sort_by_key(WorktreeRow::urgency_rank);
            RepoGroup {
                repo_id: repo.repo_id,
                repo_name: repo.repo_name,
                all_rows,
                rows,
                rows_loaded: repo.rows_loaded,
            }
        })
        .collect();
    groups.sort_by_key(RepoGroup::rank);
    groups
}

/// Whether a bare worktree row (no open agent) matches a rail filter query - the "by
/// project" equivalent of [`AgentRow::matches_filter`], matched against its label, branch,
/// and full filesystem path. The path is an additional search target because
/// `crate::rail::worktrees::WorktreeItem::label` is always a short name (never a full path), so a
/// query for an ancestor directory component would otherwise never match.
pub fn matches_filter_worktree_entry(entry: &WorktreeEntry, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    entry.label.to_lowercase().contains(&query)
        || entry
            .branch
            .as_deref()
            .is_some_and(|branch| branch.to_lowercase().contains(&query))
        || entry.path.to_string_lossy().to_lowercase().contains(&query)
}

/// One completed round of the rail's periodic background refresh: `+N -M` diff totals for
/// every agent's worktree, and clean/merged notes for every listed worktree. Performs
/// blocking I/O (`git diff`/`git status`, `gix` object-database reads) - always run via a
/// background executor, never on the GPUI foreground thread.
pub struct StatusSnapshot {
    /// Keyed by worktree/agent cwd - deduplicated by path since more than one open agent
    /// can share a worktree.
    pub diffs: HashMap<PathBuf, DiffSummary>,
    /// Keyed by worktree path.
    pub worktree_notes: HashMap<PathBuf, WorktreeNote>,
    /// Real `wt_core::diff::ahead_behind_against_base` result per worktree/agent cwd - the
    /// status bar's `↑2 ↓0` indicator. Keyed and deduplicated the same way as [`Self::diffs`];
    /// a path with no detectable base (or whose `ahead_behind_against_base` call itself failed)
    /// simply has no entry, rather than a fabricated `{0, 0}`.
    pub ahead_behind: HashMap<PathBuf, AheadBehind>,
}

/// One worktree to compute a [`WorktreeNote`] for, as input to [`compute_status_snapshot`] -
/// `crate::root::AdeApp::start_status_polling`'s reduction of `crate::rail::worktrees::WorktreeItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeQuery {
    pub path: PathBuf,
    pub is_main: bool,
    pub is_locked: bool,
}

/// Computes one [`StatusSnapshot`]: a `diff_against_base` for every distinct path in
/// `diff_paths` (deduplicated), plus an `is_dirty` + `merge_status_against_base` for every
/// [`WorktreeQuery`] in `worktrees`. A failure computing any single path's diff or note is
/// treated as "unknown for this path" rather than aborting the whole snapshot - one
/// unreadable worktree must not blank out every other row's status.
pub fn compute_status_snapshot(
    worktrees: &[WorktreeQuery],
    diff_paths: &[PathBuf],
) -> StatusSnapshot {
    let mut unique_diff_paths: Vec<PathBuf> = diff_paths.to_vec();
    unique_diff_paths.sort();
    unique_diff_paths.dedup();

    let mut diffs = HashMap::with_capacity(unique_diff_paths.len());
    let mut ahead_behind = HashMap::with_capacity(unique_diff_paths.len());
    for path in unique_diff_paths {
        let summary = match wt_core::diff::diff_against_base(&path) {
            // `DiffBase::diff()` covers both a real base-branch diff and GitHub issue #108's
            // on-default-branch/no-base uncommitted-vs-HEAD fallback - either way, real content
            // worth reflecting in this row's summary.
            Ok(base) => match base.diff() {
                Some(diff) => {
                    let (add, del) = sum_diff_stat(diff);
                    DiffSummary {
                        add,
                        del,
                        has_changes: !diff.files.is_empty(),
                    }
                }
                // `HEAD` itself is unborn: a real error reading this one path is treated the
                // same way - no reviewable diff, but not a reason to fail the whole snapshot.
                None => DiffSummary::default(),
            },
            Err(_) => DiffSummary::default(),
        };
        if let Ok(Some(counts)) = wt_core::diff::ahead_behind_against_base(&path) {
            ahead_behind.insert(path.clone(), counts);
        }
        diffs.insert(path, summary);
    }

    let mut worktree_notes = HashMap::with_capacity(worktrees.len());
    for query in worktrees {
        let clean = wt_core::is_dirty(&query.path).ok().map(|dirty| !dirty);
        let merge = if query.is_main {
            None
        } else {
            wt_core::diff::merge_status_against_base(&query.path)
                .ok()
                .flatten()
        };
        worktree_notes.insert(
            query.path.clone(),
            WorktreeNote {
                is_main: query.is_main,
                clean,
                merge,
                is_locked: query.is_locked,
            },
        );
    }

    StatusSnapshot {
        diffs,
        worktree_notes,
        ahead_behind,
    }
}

/// Whether `path` is a prune candidate on its own merits - see [`WorktreeNote::is_prunable`].
/// Does **not** know about live agents; see [`prunable_worktree_paths`] for the function
/// that combines this with the live-agent exclusion before anything is offered for removal.
pub fn is_prunable(worktree_notes: &HashMap<PathBuf, WorktreeNote>, path: &Path) -> bool {
    worktree_notes
        .get(path)
        .is_some_and(WorktreeNote::is_prunable)
}

/// The final list of worktree paths `crate::root::AdeApp::prune_worktrees` is allowed to
/// remove: every path that is a prune candidate per [`is_prunable`] **and** has no live
/// agent running with its cwd inside it.
pub fn prunable_worktree_paths(
    worktree_paths: &[PathBuf],
    worktree_notes: &HashMap<PathBuf, WorktreeNote>,
    live_agent_cwds: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    worktree_paths
        .iter()
        .filter(|path| is_prunable(worktree_notes, path))
        .filter(|path| !live_agent_cwds.contains(*path))
        .cloned()
        .collect()
}

/// Cap on how many files a single worktree's disk-usage walk will sum before giving up and
/// reporting a truncated (lower-bound) total - see [`disk_usage_bytes`]. This project's own
/// repository contains a full nested `vendor/zed` checkout (tens of thousands of files), so
/// an unbounded walk would make the rail footer's cost unpredictable.
pub const DISK_USAGE_WALK_FILE_CAP: usize = 50_000;

/// Sums file sizes recursively under `root`. Returns `(total_bytes, truncated)`; `truncated`
/// is `true` if [`DISK_USAGE_WALK_FILE_CAP`] was hit, meaning `total_bytes` is a real but
/// incomplete lower bound. Symlinks are not followed (`DirEntry::metadata` on unix is
/// `lstat`-based), so a cyclic symlink can't loop this walk. Unreadable entries are skipped
/// rather than aborting the whole walk.
pub fn disk_usage_bytes(root: &Path) -> (u64, bool) {
    let mut total = 0u64;
    let mut visited_files = 0usize;
    let mut stack = vec![root.to_path_buf()];
    let mut truncated = false;

    'walk: while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if visited_files >= DISK_USAGE_WALK_FILE_CAP {
                truncated = true;
                break 'walk;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total += metadata.len();
                visited_files += 1;
            }
        }
    }

    (total, truncated)
}

/// Formats a real byte count as a short, human-readable `B`/`KB`/`MB`/`GB` label, for the
/// rail footer's aggregate disk-usage stat.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GB {
        format!("{:.1} GB", bytes_f / GB)
    } else if bytes_f >= MB {
        format!("{:.1} MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.0} KB", bytes_f / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, status: Status, title: &str, cwd: &str) -> AgentRow {
        AgentRow {
            id,
            kind: ProcessKind::claude(),
            title: title.to_string(),
            cwd: PathBuf::from(cwd),
            status,
            branch: Some("feature-x".to_string()),
            add: 0,
            del: 0,
            exit_code: None,
            activity: None,
            elapsed: Duration::ZERO,
            review_file_count: None,
        }
    }

    #[test]
    fn activity_is_independent_of_filtering_and_defaults_to_none() {
        let idle_row = row(1, Status::Run, "agent-a", "/a");
        assert_eq!(idle_row.activity, None);

        let mut running_row = row(2, Status::Run, "agent-b", "/b");
        running_row.activity = Some("writing auth.rs".to_string());
        assert_eq!(running_row.activity.as_deref(), Some("writing auth.rs"));
        // Matching is still driven by title/kind/branch alone - an unrelated `activity` value
        // must never make an otherwise-non-matching row start matching a filter query, or vice
        // versa.
        assert!(running_row.matches_filter("agent-b"));
        assert!(!running_row.matches_filter("writing auth.rs"));
    }

    #[test]
    fn urgency_counts_covers_every_status_in_order_including_zero_counts() {
        let rows = vec![
            row(1, Status::Ask, "a", "/a"),
            row(2, Status::Ask, "b", "/b"),
            row(3, Status::Review, "c", "/c"),
        ];
        assert_eq!(
            urgency_counts(&rows),
            [
                (Status::Ask, 2),
                (Status::Fail, 0),
                (Status::Review, 1),
                (Status::Run, 0),
                (Status::Idle, 0),
            ],
            "unlike group_by_urgency, every status must appear even when its count is zero"
        );
    }

    #[test]
    fn urgency_counts_with_no_agents_is_all_zero_not_omitted() {
        let counts = urgency_counts(&[]);
        assert_eq!(counts.len(), 5);
        assert!(counts.iter().all(|(_, count)| *count == 0));
    }

    #[test]
    fn filter_agents_matches_title_branch_and_kind_case_insensitively() {
        let rows = vec![
            row(1, Status::Run, "Fix Rate Limiter", "/a"),
            row(2, Status::Run, "Unrelated Work", "/b"),
        ];

        assert_eq!(filter_agents(&rows, "rate").len(), 1);
        assert_eq!(filter_agents(&rows, "RATE").len(), 1);
        assert_eq!(
            filter_agents(&rows, "feature-x").len(),
            2,
            "both share the same branch"
        );
        assert_eq!(
            filter_agents(&rows, "claude").len(),
            2,
            "both are Claude agents"
        );
        assert_eq!(filter_agents(&rows, "nonexistent").len(), 0);
        assert_eq!(
            filter_agents(&rows, "  ").len(),
            2,
            "blank query matches everything"
        );
        assert_eq!(filter_agents(&rows, "").len(), 2);
    }

    #[test]
    fn worktree_note_main_checkout_is_never_prunable_even_if_merged() {
        let note = WorktreeNote {
            is_main: true,
            clean: Some(true),
            merge: Some(WorktreeMergeStatus {
                base_branch: "main".to_string(),
                merged: true,
                head_committer_unix_seconds: Some(0),
            }),
            is_locked: false,
        };
        assert!(!note.is_prunable());
        assert_eq!(note.label(), "checkout · clean");
    }

    #[test]
    fn worktree_note_dirty_main_checkout_label() {
        let note = WorktreeNote {
            is_main: true,
            clean: Some(false),
            merge: None,
            is_locked: false,
        };
        assert_eq!(note.label(), "checkout · dirty");
    }

    #[test]
    fn worktree_note_merged_and_clean_linked_worktree_is_prunable() {
        let seconds_since_midnight = 11 * 3600 + 4 * 60;
        let note = WorktreeNote {
            is_main: false,
            clean: Some(true),
            merge: Some(WorktreeMergeStatus {
                base_branch: "main".to_string(),
                merged: true,
                head_committer_unix_seconds: Some(seconds_since_midnight),
            }),
            is_locked: false,
        };
        assert!(note.is_prunable());
        assert_eq!(note.label(), "merged 11:04 · prunable");
    }

    #[test]
    fn worktree_note_merged_but_dirty_linked_worktree_is_not_prunable() {
        let note = WorktreeNote {
            is_main: false,
            clean: Some(false),
            merge: Some(WorktreeMergeStatus {
                base_branch: "main".to_string(),
                merged: true,
                head_committer_unix_seconds: Some(0),
            }),
            is_locked: false,
        };
        assert!(
            !note.is_prunable(),
            "a dirty worktree must never be offered as prunable, even if its branch is merged \
             - wt_core::remove_worktree would refuse it anyway"
        );
        assert_eq!(note.label(), "dirty");
    }

    #[test]
    fn worktree_note_unmerged_linked_worktree_is_not_prunable() {
        let note = WorktreeNote {
            is_main: false,
            clean: Some(true),
            merge: Some(WorktreeMergeStatus {
                base_branch: "main".to_string(),
                merged: false,
                head_committer_unix_seconds: Some(0),
            }),
            is_locked: false,
        };
        assert!(!note.is_prunable());
        assert_eq!(note.label(), "clean");
    }

    #[test]
    fn worktree_note_with_no_detectable_base_is_never_prunable() {
        let note = WorktreeNote {
            is_main: false,
            clean: Some(true),
            merge: None,
            is_locked: false,
        };
        assert!(!note.is_prunable());
    }

    #[test]
    fn worktree_note_locked_merged_clean_worktree_is_never_prunable_but_label_says_locked() {
        // A locked worktree can be genuinely merged and clean - lock state is independent of
        // merge/dirty state.
        let note = WorktreeNote {
            is_main: false,
            clean: Some(true),
            merge: Some(WorktreeMergeStatus {
                base_branch: "main".to_string(),
                merged: true,
                head_committer_unix_seconds: Some(0),
            }),
            is_locked: true,
        };
        assert!(
            !note.is_prunable(),
            "a locked worktree must never be offered as prunable regardless of merge/clean state"
        );
        assert_eq!(
            note.label(),
            "merged 00:00 · locked",
            "the merged fact should still be visible, just not offered as prunable"
        );
    }

    fn worktree_entry(path: &str, note: WorktreeNote) -> WorktreeEntry {
        let path_buf = PathBuf::from(path);
        // `crate::rail::worktrees::WorktreeItem::label` is always a short name, never a full path -
        // match that shape here so this exercises real filter behavior.
        let label = path_buf
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        WorktreeEntry {
            path: path_buf,
            label,
            branch: Some("main".to_string()),
            note,
            error: None,
        }
    }

    fn clean_note(is_main: bool) -> WorktreeNote {
        WorktreeNote {
            is_main,
            clean: Some(true),
            merge: None,
            is_locked: false,
        }
    }

    #[test]
    fn build_worktree_rows_includes_worktrees_with_no_agent_as_empty_rows() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/leftover", clean_note(false)),
        ];
        let agents: Vec<AgentRow> = Vec::new();

        let rows = build_worktree_rows(&worktrees, &agents);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].agents.is_empty());
        assert!(rows[1].agents.is_empty());
        assert_eq!(rows[0].aggregate_status(), Status::Idle);
    }

    #[test]
    fn build_worktree_rows_folds_every_agent_in_a_worktree_not_just_the_first() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/active", clean_note(false)),
        ];
        // Two agents in the SAME worktree - the real bug the old `ProjectChild`-based
        // `build_project_children` had: `agents.iter().find(...)` only ever surfaced the
        // first, silently hiding the second.
        let agents = vec![
            row(1, Status::Run, "Fix bug", "/repo-wt/active"),
            row(2, Status::Ask, "Second tab", "/repo-wt/active"),
        ];

        let rows = build_worktree_rows(&worktrees, &agents);
        assert_eq!(
            rows.len(),
            2,
            "every worktree still produces exactly one row"
        );
        assert!(rows[0].agents.is_empty());
        assert_eq!(
            rows[1].agents.len(),
            2,
            "both agents in the same worktree must be folded into its one row, not just the \
             first one found"
        );
        assert_eq!(rows[1].agents[0].id, 1);
        assert_eq!(rows[1].agents[1].id, 2);
    }

    #[test]
    fn the_earlier_runs_line_is_emitted_only_for_a_worktree_with_no_live_agent() {
        let past = |worktree: &str| crate::hooks::history::PastAgent {
            key: format!("{worktree}|Claude|1"),
            worktree: PathBuf::from(worktree),
            kind: crate::work_surface::agents::AgentKind::Claude,
            spawned_at_unix: 1,
            status: Status::Idle,
            activity: None,
            question: None,
            updated_at_unix: 100,
            session_id: None,
            title: None,
            turns: 0,
            ended_at_unix: None,
            diffstat: None,
        };
        let worktrees = vec![
            worktree_entry("/repo-wt/busy", clean_note(false)),
            worktree_entry("/repo-wt/quiet", clean_note(false)),
            worktree_entry("/repo-wt/fresh", clean_note(false)),
        ];
        let agents = vec![row(1, Status::Run, "agent-a", "/repo-wt/busy")];
        let rows = build_worktree_rows_with_history(
            &worktrees,
            &agents,
            &[past("/repo-wt/busy"), past("/repo-wt/quiet")],
        );
        let groups = vec![RepoGroup {
            repo_id: crate::rail::repo::RepoId(1),
            repo_name: "repo".to_string(),
            all_rows: rows.clone(),
            rows,
            rows_loaded: true,
        }];

        for expanded in [false, true] {
            let items = flatten_rail_list_items(&groups, |_| expanded);
            let links: Vec<usize> = items
                .iter()
                .filter_map(|item| match item {
                    RailListItem::EarlierRunsLink { row_index, .. } => Some(*row_index),
                    _ => None,
                })
                .collect();
            assert_eq!(
                links,
                vec![1],
                "expanded={expanded}: only `quiet` (history, no live agent) gets the line -                  \u{a7}6's own gate, and it does not hide behind the caret"
            );

            let link = items
                .iter()
                .find(|item| matches!(item, RailListItem::EarlierRunsLink { .. }))
                .expect("the line");
            assert!(
                link.is_last_in_worktree_block(&groups, expanded),
                "expanded={expanded}: the line is what ends `quiet`'s block, so it carries the gap"
            );
            let quiet_row = items
                .iter()
                .find(|item| matches!(item, RailListItem::WorktreeRow { row_index: 1, .. }))
                .expect("quiet's own row");
            assert!(
                !quiet_row.is_last_in_worktree_block(&groups, expanded),
                "expanded={expanded}: and the row above it therefore is not"
            );
        }
    }

    #[test]
    fn build_worktree_rows_with_history_folds_past_agents_into_their_own_matching_worktree() {
        // GitHub issue #227: a worktree with no live agent at all must still be able to carry
        // real persisted history - grouping is by path, exactly like `agents` already is, and
        // must not implicitly require a live agent to be present too.
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/bare-with-history", clean_note(false)),
            worktree_entry("/repo-wt/no-history", clean_note(false)),
        ];
        let past = crate::hooks::history::PastAgent {
            key: "past-1".to_string(),
            worktree: PathBuf::from("/repo-wt/bare-with-history"),
            kind: crate::work_surface::agents::AgentKind::Claude,
            spawned_at_unix: 1,
            status: Status::Idle,
            activity: None,
            question: None,
            updated_at_unix: 100,
            session_id: None,
            title: None,
            turns: 0,
            ended_at_unix: None,
            diffstat: None,
        };

        let rows = build_worktree_rows_with_history(&worktrees, &[], std::slice::from_ref(&past));
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].history.is_empty(),
            "the unrelated /repo row must not receive another worktree's history"
        );
        assert_eq!(
            rows[1].history,
            vec![past],
            "the matching worktree must receive its own real history entry"
        );
        assert!(
            rows[2].history.is_empty(),
            "a worktree with no persisted history must show none - no empty-state entry either"
        );
    }

    #[test]
    fn aggregate_status_picks_the_most_urgent_contained_agent() {
        let worktrees = vec![worktree_entry("/repo-wt/a", clean_note(false))];
        let agents = vec![
            row(1, Status::Run, "run", "/repo-wt/a"),
            row(2, Status::Ask, "ask", "/repo-wt/a"),
            row(3, Status::Idle, "idle", "/repo-wt/a"),
        ];
        let rows = build_worktree_rows(&worktrees, &agents);
        assert_eq!(
            rows[0].aggregate_status(),
            Status::Ask,
            "Ask is the most urgent of Run/Ask/Idle per Status::ORDER"
        );
    }

    #[test]
    fn urgency_rank_matches_status_rank_for_a_worktree_with_agents() {
        let worktrees = vec![
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
        ];
        let agents = vec![
            row(1, Status::Ask, "ask", "/repo-wt/asking"),
            row(2, Status::Run, "run", "/repo-wt/running"),
        ];
        let rows = build_worktree_rows(&worktrees, &agents);
        assert_eq!(rows[0].urgency_rank(), Status::Ask.urgency_rank());
        assert_eq!(rows[1].urgency_rank(), Status::Run.urgency_rank());
    }

    #[test]
    fn urgency_rank_splits_bare_and_prunable_below_every_agent_status() {
        let mut prunable_note = clean_note(false);
        prunable_note.merge = Some(WorktreeMergeStatus {
            base_branch: "main".to_string(),
            merged: true,
            head_committer_unix_seconds: Some(0),
        });
        assert!(prunable_note.is_prunable(), "sanity check");

        let worktrees = vec![
            worktree_entry("/repo-wt/bare", clean_note(false)),
            worktree_entry("/repo-wt/prunable", prunable_note),
        ];
        let rows = build_worktree_rows(&worktrees, &[]);

        assert_eq!(
            rows[0].urgency_rank(),
            5,
            "an agent-less, non-prunable worktree ranks 'bare' - below every real agent \
             status but above prunable"
        );
        assert_eq!(
            rows[1].urgency_rank(),
            6,
            "an agent-less, prunable worktree ranks last of all - §2.1's \
             'input → failed → review → running → idle → bare → prunable'"
        );
        assert!(
            rows[0].urgency_rank() < rows[1].urgency_rank(),
            "bare must outrank prunable"
        );
        assert!(
            Status::ORDER
                .iter()
                .all(|status| status.urgency_rank() < rows[0].urgency_rank()),
            "every real agent status must outrank a bare worktree"
        );
    }

    /// A [`RepoWorktrees`] whose `all_rows` and `rows` are the same list - the common case in
    /// these tests, where nothing distinguishes "this repo's real worktree set" from "what's
    /// currently rendered under it". `rows_loaded` is always `true` here (these tests model a
    /// repo whose data really has been loaded) - [`repo_worktrees_split`] below is for the tests
    /// that need `all_rows`/`rows` to diverge, and [`repo_worktrees_unloaded`] for the tests that
    /// need `rows_loaded: false`.
    fn repo_worktrees(id: u64, name: &str, rows: Vec<WorktreeRow>) -> RepoWorktrees {
        repo_worktrees_split(id, name, rows.clone(), rows)
    }

    /// A [`RepoWorktrees`] with independently-set `all_rows` (the repo's real, complete
    /// worktree list - what the header counters must read) and `rows` (what's actually
    /// rendered/expanded below the header - what a filter query or a non-focused repo can
    /// legitimately narrow). See [`RepoWorktrees::all_rows`]'s own docs for why the two must
    /// stay independent. `rows_loaded` is always `true` here - see [`repo_worktrees_unloaded`]
    /// for the unpopulated case.
    fn repo_worktrees_split(
        id: u64,
        name: &str,
        all_rows: Vec<WorktreeRow>,
        rows: Vec<WorktreeRow>,
    ) -> RepoWorktrees {
        RepoWorktrees {
            repo_id: RepoId(id),
            repo_name: name.to_string(),
            all_rows,
            rows,
            rows_loaded: true,
        }
    }

    /// A [`RepoWorktrees`] whose data was never fetched - `rows_loaded: false`, `all_rows`/`rows`
    /// both empty regardless of how many worktrees this repo may really have on disk. Models
    /// every non-focused repo in [`crate::rail::render::AdeApp::build_repo_groups`]'s own real
    /// output - see [`RepoWorktrees::rows_loaded`]'s docs for why this must render differently
    /// from a repo that was really loaded and really has zero worktrees.
    fn repo_worktrees_unloaded(id: u64, name: &str) -> RepoWorktrees {
        RepoWorktrees {
            repo_id: RepoId(id),
            repo_name: name.to_string(),
            all_rows: Vec::new(),
            rows: Vec::new(),
            rows_loaded: false,
        }
    }

    #[test]
    fn group_worktrees_by_repo_sorts_rows_inside_a_group_by_urgency_rank() {
        let worktrees = vec![
            worktree_entry("/repo-wt/bare", clean_note(false)),
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
        ];
        let agents = vec![
            row(1, Status::Run, "run", "/repo-wt/running"),
            row(2, Status::Ask, "ask", "/repo-wt/asking"),
        ];
        // Deliberately built in a non-urgency order, so a passing test proves the function
        // itself sorts rather than merely preserving input order.
        let rows = build_worktree_rows(&worktrees, &agents);

        let groups = group_worktrees_by_repo(vec![repo_worktrees(0, "jerry-core", rows)]);
        assert_eq!(groups.len(), 1);
        let labels: Vec<&str> = groups[0]
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(
            labels,
            vec!["asking", "running", "bare"],
            "asking (rank 0) before running (rank 3) before an agent-less bare row (rank 5)"
        );
    }

    #[test]
    fn group_worktrees_by_repo_orders_groups_by_their_own_most_urgent_worktree() {
        let quiet_worktrees = vec![worktree_entry("/quiet-repo/main", clean_note(true))];
        let quiet_rows = build_worktree_rows(&quiet_worktrees, &[]);

        let urgent_worktrees = vec![worktree_entry("/urgent-repo/wt", clean_note(false))];
        let urgent_agents = vec![row(1, Status::Ask, "ask", "/urgent-repo/wt")];
        let urgent_rows = build_worktree_rows(&urgent_worktrees, &urgent_agents);

        // Built with the quiet (less urgent) repo listed first, so a passing test proves this
        // is a real sort, not insertion order surviving by coincidence.
        let groups = group_worktrees_by_repo(vec![
            repo_worktrees(1, "quiet-repo", quiet_rows),
            repo_worktrees(2, "urgent-repo", urgent_rows),
        ]);

        let names: Vec<&str> = groups.iter().map(|g| g.repo_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["urgent-repo", "quiet-repo"],
            "the repo holding the asking agent must sort first, never alphabetically or by \
             insertion order"
        );
    }

    #[test]
    fn group_worktrees_by_repo_still_renders_a_single_repo_with_no_worktrees() {
        let groups = group_worktrees_by_repo(vec![repo_worktrees(0, "jerry-core", Vec::new())]);
        assert_eq!(
            groups.len(),
            1,
            "a repo group must render even with zero worktree rows - §2.0 says not to \
             special-case a single (or empty) repo away"
        );
        assert_eq!(groups[0].repo_name, "jerry-core");
        assert!(groups[0].rows.is_empty());
    }

    #[test]
    fn group_worktrees_by_repo_carries_rows_loaded_through_unchanged() {
        let groups = group_worktrees_by_repo(vec![
            repo_worktrees(0, "loaded-repo", Vec::new()),
            repo_worktrees_unloaded(1, "unloaded-repo"),
        ]);

        let loaded = groups
            .iter()
            .find(|g| g.repo_name == "loaded-repo")
            .expect("loaded-repo's group must exist");
        assert!(
            loaded.rows_loaded,
            "a repo built via `repo_worktrees` really has its data loaded"
        );

        let unloaded = groups
            .iter()
            .find(|g| g.repo_name == "unloaded-repo")
            .expect("unloaded-repo's group must exist");
        assert!(
            !unloaded.rows_loaded,
            "a repo whose worktree data was never fetched must carry that through to its \
             `RepoGroup`, not silently become indistinguishable from a real zero-worktree repo"
        );
        assert!(
            unloaded.all_rows.is_empty() && unloaded.rows.is_empty(),
            "sanity check: the unloaded repo's rows are empty for the same reason its data was \
             never fetched, not because it really has zero worktrees"
        );
    }

    #[test]
    fn repo_group_urgency_counts_report_asking_and_failed_worktrees_separately() {
        let worktrees = vec![
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/failed", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
            worktree_entry("/repo-wt/bare", clean_note(false)),
        ];
        let agents = vec![
            row(1, Status::Ask, "ask", "/repo-wt/asking"),
            row(2, Status::Fail, "fail", "/repo-wt/failed"),
            row(3, Status::Run, "run", "/repo-wt/running"),
        ];
        let rows = build_worktree_rows(&worktrees, &agents);
        let groups = group_worktrees_by_repo(vec![repo_worktrees(0, "jerry-core", rows)]);
        assert_eq!(
            groups[0].needs_input_count(),
            1,
            "the amber count is the asking worktree alone - never summed with the failed one \
             (§7 rule 4: two states distinguished anywhere are never summed anywhere)"
        );
        assert_eq!(
            groups[0].failed_count(),
            1,
            "the red count is the failed worktree alone, not the running or bare ones"
        );

        let all_quiet = build_worktree_rows(
            &[worktree_entry("/repo-wt/running-only", clean_note(false))],
            &[row(1, Status::Run, "run", "/repo-wt/running-only")],
        );
        let quiet_groups = group_worktrees_by_repo(vec![repo_worktrees(0, "quiet", all_quiet)]);
        assert_eq!(quiet_groups[0].needs_input_count(), 0);
        assert_eq!(
            quiet_groups[0].failed_count(),
            0,
            "both counts are zero for a repo whose only agent is running - the render side hides \
             each pair at zero"
        );
    }

    #[test]
    fn a_worktree_holding_both_an_asking_and_a_failed_agent_counts_once_as_failed() {
        let worktrees = vec![worktree_entry("/repo-wt/both", clean_note(false))];
        let agents = vec![
            row(1, Status::Ask, "ask", "/repo-wt/both"),
            row(2, Status::Fail, "fail", "/repo-wt/both"),
        ];
        let rows = build_worktree_rows(&worktrees, &agents);
        assert_eq!(
            rows[0].aggregate_status(),
            Status::Ask,
            "sanity check: the aggregate really does rank Ask above Fail, so counting off it \
             would put this worktree in the amber column"
        );

        let groups = group_worktrees_by_repo(vec![repo_worktrees(0, "jerry-core", rows)]);
        assert_eq!(
            groups[0].failed_count(),
            1,
            "the worse state wins: this worktree is counted in red"
        );
        assert_eq!(
            groups[0].needs_input_count(),
            0,
            "and never also in amber - counted once, not twice"
        );
    }

    #[test]
    fn repo_group_header_counts_read_the_real_worktree_list_not_the_displayed_rows() {
        let real_worktrees = vec![
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
            worktree_entry("/repo-wt/bare", clean_note(false)),
        ];
        let real_agents = vec![
            row(1, Status::Ask, "ask", "/repo-wt/asking"),
            row(2, Status::Run, "run", "/repo-wt/running"),
        ];
        let all_rows = build_worktree_rows(&real_worktrees, &real_agents);

        // A completely different, unrelated (and shorter) "displayed" set - standing in for
        // either a filter query that hid most of the repo's rows, or a non-focused repo whose
        // body isn't currently rendered at all.
        let displayed_rows = vec![WorktreeRow {
            path: PathBuf::from("/repo-wt/bare"),
            label: "bare".to_string(),
            branch: None,
            note: clean_note(false),
            error: None,
            agents: Vec::new(),
            history: Vec::new(),
        }];

        let groups = group_worktrees_by_repo(vec![repo_worktrees_split(
            0,
            "jerry-core",
            all_rows,
            displayed_rows,
        )]);

        assert_eq!(
            groups[0].all_rows.len(),
            3,
            "the header's `N wt` count must reflect the repo's real, complete worktree list"
        );
        assert_eq!(
            groups[0].rows.len(),
            1,
            "sanity check: the displayed rows really are a narrower, different set"
        );
        assert_eq!(
            groups[0].needs_input_count(),
            1,
            "the amber urgency count must be derived from the real worktree list too - the \
             asking worktree exists in `all_rows` even though it isn't in the displayed `rows`"
        );
    }

    #[test]
    fn group_worktrees_by_repo_orders_groups_by_all_rows_not_displayed_rows() {
        let urgent_worktrees = vec![worktree_entry("/urgent-repo/wt", clean_note(false))];
        let urgent_agents = vec![row(1, Status::Ask, "ask", "/urgent-repo/wt")];
        let urgent_all_rows = build_worktree_rows(&urgent_worktrees, &urgent_agents);
        // The urgent repo's displayed rows are empty - e.g. a filter query matched nothing in
        // it, or it isn't the focused repo - but its real state is still urgent.
        let urgent_displayed_rows = Vec::new();

        let quiet_worktrees = vec![worktree_entry("/quiet-repo/main", clean_note(true))];
        let quiet_rows = build_worktree_rows(&quiet_worktrees, &[]);

        let groups = group_worktrees_by_repo(vec![
            repo_worktrees(1, "quiet-repo", quiet_rows),
            repo_worktrees_split(2, "urgent-repo", urgent_all_rows, urgent_displayed_rows),
        ]);

        let names: Vec<&str> = groups.iter().map(|g| g.repo_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["urgent-repo", "quiet-repo"],
            "the repo holding the real (not merely displayed) asking worktree must still sort \
             first"
        );
    }

    #[test]
    fn the_two_urgency_tooltips_agree_with_their_own_counts_in_noun_and_verb() {
        assert_eq!(needs_input_tooltip(1), "1 worktree here needs input");
        assert_eq!(needs_input_tooltip(2), "2 worktrees here need input");
        assert_eq!(failed_tooltip(1), "1 worktree here has a failed agent");
        assert_eq!(failed_tooltip(3), "3 worktrees here have failed agents");
    }

    #[test]
    fn diff_stat_parts_splits_the_rail_diffstat_and_drops_an_empty_deletion() {
        assert_eq!(
            diff_stat_parts(152, 11),
            Some(("+152".to_string(), Some("\u{2212}11".to_string())))
        );
        assert_eq!(diff_stat_parts(9, 0), Some(("+9".to_string(), None)));
        assert_eq!(
            diff_stat_parts(0, 4),
            Some(("+0".to_string(), Some("\u{2212}4".to_string()))),
            "a pure-deletion diff still states its (zero) additions - the pair is how a diffstat \
             reads, and `+0 \u{2212}4` is a real answer where a bare `\u{2212}4` would be a \
             differently-shaped one"
        );
        assert_eq!(
            diff_stat_parts(0, 0),
            None,
            "no diff at all renders no diffstat - the row's neutral prose fallback occupies that \
             slot instead"
        );
    }

    #[test]
    fn the_prune_tooltip_states_the_count_and_only_a_measured_size() {
        assert_eq!(
            prune_tooltip(1, Some((214 * 1024 * 1024, false))),
            "Prune merged worktrees \u{2014} 1 prunable, frees 214.0 MB"
        );
        assert_eq!(
            prune_tooltip(3, Some((2 * 1024 * 1024 * 1024, true))),
            "Prune merged worktrees \u{2014} 3 prunable, frees 2.0 GB+",
            "a truncated scan keeps the `+` suffix `disk_usage_label` already uses - the number \
             is a floor, and the tooltip must not round it into a claim"
        );
        assert_eq!(
            prune_tooltip(1, None),
            "Prune merged worktrees \u{2014} 1 prunable",
            "an unmeasured candidate drops the whole clause rather than reporting a size that \
             leaves it out"
        );
        assert_eq!(
            prune_tooltip(0, Some((0, false))),
            "Prune merged worktrees \u{2014} nothing prunable",
            "zero candidates changes the sentence's shape rather than reading `0 prunable`"
        );
    }

    #[test]
    fn the_armed_prune_tooltip_conjugates_its_own_count() {
        assert_eq!(prune_armed_tooltip(1), "Click again to remove 1 worktree");
        assert_eq!(prune_armed_tooltip(2), "Click again to remove 2 worktrees");
    }

    #[test]
    fn worktree_disk_label_conjugates_at_zero_one_and_two() {
        assert_eq!(worktree_disk_label(0, "0 B"), "0 worktrees \u{b7} 0 B");
        assert_eq!(worktree_disk_label(1, "4.2 GB"), "1 worktree \u{b7} 4.2 GB");
        assert_eq!(
            worktree_disk_label(2, "4.2 GB"),
            "2 worktrees \u{b7} 4.2 GB"
        );
    }

    #[test]
    fn prune_labels_conjugate_at_zero_one_and_two() {
        assert_eq!(
            prune_confirm_label(0),
            "click prune again to remove 0 worktrees"
        );
        assert_eq!(
            prune_confirm_label(1),
            "click prune again to remove 1 worktree"
        );
        assert_eq!(
            prune_confirm_label(2),
            "click prune again to remove 2 worktrees"
        );

        assert_eq!(pruning_label(0), "pruning 0 worktrees\u{2026}");
        assert_eq!(pruning_label(1), "pruning 1 worktree\u{2026}");
        assert_eq!(pruning_label(2), "pruning 2 worktrees\u{2026}");

        assert_eq!(pruned_label(0), "pruned 0 worktrees");
        assert_eq!(pruned_label(1), "pruned 1 worktree");
        assert_eq!(pruned_label(2), "pruned 2 worktrees");
    }

    #[test]
    fn no_prune_label_uses_the_parenthesised_plural_escape_hatch() {
        for n in 0..4 {
            for label in [prune_confirm_label(n), pruning_label(n), pruned_label(n)] {
                assert!(
                    !label.contains("(s)"),
                    "prune label must conjugate, not dodge: {label}"
                );
            }
        }
    }

    #[test]
    fn format_elapsed_picks_the_largest_whole_unit() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m");
        assert_eq!(format_elapsed(Duration::from_secs(4 * 60)), "4m");
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59m");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h");
        assert_eq!(format_elapsed(Duration::from_secs(3 * 3600 + 1800)), "3h");
    }

    #[test]
    fn filter_worktree_rows_matches_agent_title_worktree_label_or_worktree_path() {
        let with_agent = {
            let worktrees = vec![worktree_entry("/a", clean_note(false))];
            let agents = vec![row(1, Status::Run, "Fix rate limiter", "/a")];
            build_worktree_rows(&worktrees, &agents).remove(0)
        };
        // "leftover-branch" is the label (leaf name only); "repo-worktrees" can only match
        // via the path fallback, not the label.
        let agent_less = {
            let worktrees = vec![worktree_entry(
                "/repo-worktrees/leftover-branch",
                clean_note(false),
            )];
            build_worktree_rows(&worktrees, &[]).remove(0)
        };
        let rows = vec![with_agent, agent_less];

        assert_eq!(filter_worktree_rows(&rows, "").len(), 2);
        assert_eq!(
            filter_worktree_rows(&rows, "rate").len(),
            1,
            "matches only the row with the agent, via its title"
        );
        assert_eq!(
            filter_worktree_rows(&rows, "leftover").len(),
            1,
            "matches only the agent-less row, via its real (leaf-name) label"
        );
        assert_eq!(
            filter_worktree_rows(&rows, "repo-worktrees").len(),
            1,
            "matches only the agent-less row, via its real path - the label alone never \
             contains a directory component like this"
        );
    }

    #[test]
    fn is_prunable_helper_looks_up_by_path() {
        let mut notes = HashMap::new();
        notes.insert(
            PathBuf::from("/repo-wt/merged"),
            WorktreeNote {
                is_main: false,
                clean: Some(true),
                merge: Some(WorktreeMergeStatus {
                    base_branch: "main".to_string(),
                    merged: true,
                    head_committer_unix_seconds: Some(0),
                }),
                is_locked: false,
            },
        );
        assert!(is_prunable(&notes, Path::new("/repo-wt/merged")));
        assert!(!is_prunable(&notes, Path::new("/repo-wt/unknown")));
    }

    #[test]
    fn prunable_worktree_paths_excludes_a_path_with_a_live_agent_even_if_otherwise_prunable() {
        let merged_clean_path = PathBuf::from("/repo-wt/merged-clean-but-in-use");
        let mut notes = HashMap::new();
        notes.insert(
            merged_clean_path.clone(),
            WorktreeNote {
                is_main: false,
                clean: Some(true),
                merge: Some(WorktreeMergeStatus {
                    base_branch: "main".to_string(),
                    merged: true,
                    head_committer_unix_seconds: Some(0),
                }),
                is_locked: false,
            },
        );
        assert!(
            is_prunable(&notes, &merged_clean_path),
            "sanity check: this path must be a real prune candidate on its own merits"
        );

        let worktree_paths = vec![merged_clean_path.clone()];
        let mut live_agent_cwds = HashSet::new();
        live_agent_cwds.insert(merged_clean_path.clone());

        let candidates = prunable_worktree_paths(&worktree_paths, &notes, &live_agent_cwds);
        assert!(
            candidates.is_empty(),
            "a worktree with a live agent tracked against its path must never appear in \
             the prune candidate list, even though it is otherwise prunable"
        );

        let candidates_without_agent =
            prunable_worktree_paths(&worktree_paths, &notes, &HashSet::new());
        assert_eq!(candidates_without_agent, vec![merged_clean_path]);
    }

    #[test]
    fn prunable_worktree_paths_only_excludes_the_exact_matching_path() {
        let a = PathBuf::from("/repo-wt/a");
        let b = PathBuf::from("/repo-wt/b");
        let mut notes = HashMap::new();
        for path in [&a, &b] {
            notes.insert(
                path.clone(),
                WorktreeNote {
                    is_main: false,
                    clean: Some(true),
                    merge: Some(WorktreeMergeStatus {
                        base_branch: "main".to_string(),
                        merged: true,
                        head_committer_unix_seconds: Some(0),
                    }),
                    is_locked: false,
                },
            );
        }

        let worktree_paths = vec![a.clone(), b.clone()];
        let mut live_agent_cwds = HashSet::new();
        live_agent_cwds.insert(a.clone());

        let candidates = prunable_worktree_paths(&worktree_paths, &notes, &live_agent_cwds);
        assert_eq!(
            candidates,
            vec![b],
            "only the worktree with a live agent should be excluded; unrelated prunable \
             worktrees are unaffected"
        );
    }

    #[test]
    fn sum_diff_stat_counts_added_and_removed_lines_across_files_and_hunks() {
        use wt_core::diff::{DiffFile, DiffHunk, DiffLine, FileChangeStatus};

        let diff = WorktreeDiff {
            base_branch: "main".to_string(),
            base_commit: "deadbeef".to_string(),
            truncated: false,
            files: vec![DiffFile {
                path: PathBuf::from("a.txt"),
                old_path: None,
                status: FileChangeStatus::Modified,
                is_binary: false,
                truncated: false,
                hunks: vec![DiffHunk {
                    header: "@@ -1,2 +1,3 @@".to_string(),
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Context,
                            content: "unchanged".to_string(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Added,
                            content: "new one".to_string(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Added,
                            content: "new two".to_string(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Removed,
                            content: "gone".to_string(),
                        },
                    ],
                }],
            }],
        };

        assert_eq!(sum_diff_stat(&diff), (2, 1));
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("file.txt"), "hello\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "initial commit"]);
        dir
    }

    #[test]
    fn compute_status_snapshot_reports_real_diff_and_merge_state() {
        let repo = init_repo();

        // A linked worktree with an uncommitted change - should show up in `diffs` with a
        // real added-line count, and not be considered clean.
        let dirty_wt_dir = tempfile::TempDir::new().expect("tempdir");
        let dirty_wt_path = dirty_wt_dir.path().join("dirty-wt");
        drop(dirty_wt_dir);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "dirty-branch",
                dirty_wt_path.to_str().expect("utf8"),
            ],
        );
        std::fs::write(dirty_wt_path.join("file.txt"), "hello\nnew line\n").expect("write");

        // A linked worktree that's clean and fully merged (no unique commits) - a real prune
        // candidate.
        let merged_wt_dir = tempfile::TempDir::new().expect("tempdir");
        let merged_wt_path = merged_wt_dir.path().join("merged-wt");
        drop(merged_wt_dir);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "merged-branch",
                merged_wt_path.to_str().expect("utf8"),
            ],
        );

        let worktrees = vec![
            WorktreeQuery {
                path: repo.path().to_path_buf(),
                is_main: true,
                is_locked: false,
            },
            WorktreeQuery {
                path: dirty_wt_path.clone(),
                is_main: false,
                is_locked: false,
            },
            WorktreeQuery {
                path: merged_wt_path.clone(),
                is_main: false,
                is_locked: false,
            },
        ];
        let diff_paths = vec![dirty_wt_path.clone(), merged_wt_path.clone()];

        let snapshot = compute_status_snapshot(&worktrees, &diff_paths);

        let dirty_diff = snapshot
            .diffs
            .get(&dirty_wt_path)
            .expect("dirty worktree should have a diff entry");
        assert!(dirty_diff.has_changes);
        assert!(dirty_diff.add >= 1);

        let dirty_note = snapshot
            .worktree_notes
            .get(&dirty_wt_path)
            .expect("dirty worktree should have a note");
        assert_eq!(dirty_note.clean, Some(false));
        assert!(!dirty_note.is_prunable());

        let merged_note = snapshot
            .worktree_notes
            .get(&merged_wt_path)
            .expect("merged worktree should have a note");
        assert_eq!(merged_note.clean, Some(true));
        assert!(
            merged_note.is_prunable(),
            "a clean worktree whose branch has no unique commits must be a real prune candidate"
        );

        let main_note = snapshot
            .worktree_notes
            .get(repo.path())
            .expect("main worktree should have a note");
        assert!(main_note.is_main);
        assert!(
            !main_note.is_prunable(),
            "the main checkout is never prunable"
        );

        // Neither linked worktree has diverged from `main` (both branched off it with no new
        // commits on either side), so a real `ahead_behind` entry should exist for each and
        // report zero on both sides.
        let dirty_ahead_behind = snapshot
            .ahead_behind
            .get(&dirty_wt_path)
            .expect("dirty worktree should have a real ahead_behind entry");
        assert_eq!(dirty_ahead_behind.ahead, 0);
        assert_eq!(dirty_ahead_behind.behind, 0);
        let merged_ahead_behind = snapshot
            .ahead_behind
            .get(&merged_wt_path)
            .expect("merged worktree should have a real ahead_behind entry");
        assert_eq!(merged_ahead_behind.ahead, 0);
        assert_eq!(merged_ahead_behind.behind, 0);
    }

    #[test]
    fn disk_usage_bytes_sums_real_file_sizes_recursively() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), vec![b'x'; 100]).expect("write");
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).expect("mkdir");
        std::fs::write(sub.join("b.txt"), vec![b'y'; 250]).expect("write");

        let (total, truncated) = disk_usage_bytes(dir.path());
        assert_eq!(total, 350);
        assert!(!truncated);
    }

    #[test]
    fn disk_usage_bytes_on_a_nonexistent_path_is_zero_not_an_error() {
        let (total, truncated) = disk_usage_bytes(Path::new("/definitely/not/a/real/path/xyz"));
        assert_eq!(total, 0);
        assert!(!truncated);
    }

    #[test]
    fn format_bytes_picks_a_sensible_unit() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
