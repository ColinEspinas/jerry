//! The session rail's data model: pure, GPUI-free types and functions for grouping and
//! filtering (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md`'s Zone 1). No `gpui`
//! dependency, so this logic is unit-testable without a real window, terminal, or git state.
//! `crate::root` gathers the real signals (`TerminalPane`, `wt_core::list_worktrees`,
//! `wt_core::diff::diff_against_base`) into the plain types this module operates on, and renders
//! the result as GPUI elements.
//!
//! Two levels, always (§2.1): **repo group → worktree → agents** - no rail mode toggle. There
//! used to be a second, `RailMode`-switched "by project" structure; the revision that redesigned
//! the rail around repo grouping removed it (§7: "`groupSessions`, `railMode` and `railSort` are
//! all gone").

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::rail::repo::RepoId;
use crate::rail::status::Status;
use crate::work_surface::sessions::SessionKind;
use wt_core::diff::{AheadBehind, DiffBase, DiffLineKind, WorktreeDiff, WorktreeMergeStatus};

/// One session, reduced to exactly what the rail row needs to render - built in `crate::root`
/// from a `crate::work_surface::sessions::Session` plus a `wt_core::diff::diff_against_base` result for its
/// worktree. See `crate::rail::status::derive_status` for how `status` was computed.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: crate::work_surface::sessions::SessionId,
    pub kind: SessionKind,
    pub title: String,
    pub cwd: PathBuf,
    pub status: Status,
    pub branch: Option<String>,
    /// `+added -deleted` line counts from `wt_core::diff::diff_against_base`, summed across
    /// every changed file. Both `0` if the diff hasn't loaded yet or there are no changes.
    pub add: usize,
    pub del: usize,
    /// Tail-of-pty text for a waiting session (`TerminalPane::visible_text_lines`, trimmed to
    /// the last non-blank line) - the design's "question preview". Only populated for
    /// [`Status::Ask`] rows.
    pub question_preview: Option<String>,
    /// The process exit code, only for [`Status::Fail`]/[`Status::Review`]/exited-`Idle`
    /// rows. `None` while still running or never started.
    pub exit_code: Option<u32>,
    /// The agent row's "what it is doing" trailing text for a [`Status::Run`] row -
    /// `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.3: "the live tool call -
    /// `writing auth.rs`, `editing reports.rs`, `bench 3 of 5`, `148 of 312`".
    ///
    /// A sibling field here rather than a payload on [`Status::Run`] itself, mirroring
    /// [`Self::question_preview`]'s own shape immediately above (also status-conditional, also a
    /// row-level fact rather than part of the status enum) - keeping [`Status`] itself a plain
    /// `Copy` value users of `Status::ORDER`/`urgency_rank`/`color` etc. can keep relying on,
    /// rather than every one of those call sites suddenly needing to supply or ignore an
    /// `activity` payload for a `Run` variant they don't care about.
    ///
    /// **Always `None` today.** The real PTY-output-to-activity-text heuristic is a separate,
    /// parallel piece of work (this revision's Phase 0 is data-model-and-persistence only); this
    /// field exists so that work has a real, already-plumbed-through place to write into -
    /// [`crate::rail::render::AdeApp::build_session_rows`] is where a live value would be filled
    /// in, the same real construction site [`Self::question_preview`] is already filled in from.
    pub activity: Option<String>,
    /// Wall-clock time since `crate::work_surface::sessions::Session::spawned_at` - the agent
    /// row's line-1 elapsed time (§2.3: "elapsed 9.5px mono right"). See [`format_elapsed`] for
    /// how this becomes the rendered `4m`/`1h` text.
    pub elapsed: Duration,
    /// How many files this session is a recorded author of, for a [`Status::Review`] row only -
    /// §2.3's "review ready" trailing text (`12 files`), sourced from
    /// `crate::sidebar::changes::Authorship::file_count_for`. `None` for every other status (§2.3:
    /// "Do not put a per-agent file count here" - review-ready is the one documented exception),
    /// and also `None` for a review-ready row whose worktree diff isn't the one currently loaded
    /// in Zone 3 - `crate::root::AdeApp::file_authorship` is scoped to that single loaded diff
    /// (see its own docs), not tracked per-worktree yet, so a row outside it genuinely has no
    /// authorship data to report rather than a fabricated count.
    pub review_file_count: Option<usize>,
}

impl SessionRow {
    /// Whether this row matches a rail filter query - case-insensitive substring match
    /// against the title, branch name, and session kind label.
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

/// Filters `rows` down to those matching `query` - see [`SessionRow::matches_filter`]. A
/// blank query matches everything.
pub fn filter_sessions<'a>(rows: &'a [SessionRow], query: &str) -> Vec<&'a SessionRow> {
    rows.iter()
        .filter(|row| row.matches_filter(query))
        .collect()
}

/// `+added -deleted` totals for one worktree/session cwd, summed across every changed file's
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

/// Real per-status counts across every session row, in [`Status::ORDER`] - the status bar's
/// five urgency-counter squares (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s change 7).
/// Unlike [`group_worktrees_by_repo`], a status with zero matching rows still gets a real `0`
/// entry rather than being omitted, since the status bar always shows all five squares. Built
/// from the
/// same per-session [`Status`] every [`SessionRow`] already carries - not a second, independent
/// status classification.
pub fn urgency_counts(rows: &[SessionRow]) -> [(Status, usize); 5] {
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
    ///
    /// Not sufficient on its own to remove a worktree - says nothing about a live session
    /// running inside it. See [`prunable_worktree_paths`] for that additional exclusion.
    pub fn is_prunable(&self) -> bool {
        !self.is_main
            && !self.is_locked
            && self.clean == Some(true)
            && self.merge.as_ref().is_some_and(|status| status.merged)
    }

    /// The real note text shown on a session-less worktree row.
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
fn format_utc_hhmm(unix_seconds: i64) -> String {
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

/// One rail row: a single worktree, with **every** session currently open in it (not just the
/// first one found) folded in as tabs - the real "one worktree = one rail entry, N sessions =
/// N tabs" model this revision introduces, replacing the old `ProjectChild` shape whose
/// `sessions.iter().find(...)` silently hid every session past the first in the same worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeRow {
    pub path: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    /// Clean/merged note - only meaningful (and only ever shown) when [`Self::sessions`] is
    /// empty; a worktree with an open session shows its sessions' own real status instead.
    pub note: WorktreeNote,
    /// `Some(message)` if this worktree's metadata failed to read - see [`WorktreeEntry::error`]'s
    /// own docs; a worktree row in this state is never interactive.
    pub error: Option<String>,
    /// Every session currently open in this worktree, in tab-strip order (creation order,
    /// matching `crate::work_surface::sessions::Sessions::iter_for_cwd`).
    pub sessions: Vec<SessionRow>,
}

impl WorktreeRow {
    /// The aggregate status shown on this row: the most urgent status among its open sessions
    /// (`Status::urgency_rank`, lower = more urgent - the same ranking the old
    /// `status_dot_cluster` already used to sort a worktree's per-session dots), or
    /// [`Status::Idle`] when no session is open at all - mirroring
    /// `crate::rail::status::derive_status`'s own `ProcessSignal::NoProcess => Status::Idle`, since
    /// "no process running" is exactly what a session-less worktree is.
    pub fn aggregate_status(&self) -> Status {
        self.sessions
            .iter()
            .map(|row| row.status)
            .min_by_key(|status| status.urgency_rank())
            .unwrap_or(Status::Idle)
    }

    /// This row's rank for sorting *inside* a repo group (and, via [`RepoGroup::rank`], the
    /// groups themselves) - `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.1's
    /// full seven-step order: `input → failed → review → running → idle → bare → prunable`.
    /// Lower sorts first (more urgent).
    ///
    /// The first five steps are exactly [`Status::urgency_rank`] (§2.3's own agent state words -
    /// `needs input`/`failed`/`review ready`/`running`/`paused` - map one-to-one onto
    /// [`Status::Ask`]/[`Status::Fail`]/[`Status::Review`]/[`Status::Run`]/[`Status::Idle`], see
    /// that enum's own docs), reused rather than re-derived. The last two only apply to a
    /// session-less row, which [`Self::aggregate_status`] alone can't distinguish from a row
    /// whose one open session is genuinely [`Status::Idle`] - both would otherwise rank 4. A
    /// worktree with no agents at all is never the "same kind of quiet" as one with an idle
    /// agent still attached, so this splits that case using [`WorktreeNote::is_prunable`], the
    /// same real merged/clean/locked facts §2.2's "Bare worktrees `#22262a`, prunable
    /// `#2f353a`" left-edge colours already key off.
    pub fn urgency_rank(&self) -> u8 {
        if self.sessions.is_empty() {
            if self.note.is_prunable() {
                6
            } else {
                5
            }
        } else {
            self.aggregate_status().urgency_rank()
        }
    }

    /// The real `+added -deleted` totals summed across every open session's own diff summary -
    /// double-counting is impossible since every session in [`Self::sessions`] shares this same
    /// worktree's `cwd`, so they'd all report the identical per-worktree diff anyway; this just
    /// reads the first one rather than literally summing duplicates.
    pub fn diff_totals(&self) -> (usize, usize) {
        self.sessions
            .first()
            .map(|row| (row.add, row.del))
            .unwrap_or((0, 0))
    }

    /// Whether this row matches a rail filter query - its own label/branch/path (see
    /// [`matches_filter_worktree_entry`]) or any of its open sessions' own title/branch/kind
    /// (see [`SessionRow::matches_filter`]).
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
        entry_matches || self.sessions.iter().any(|row| row.matches_filter(trimmed))
    }
}

/// Builds one [`WorktreeRow`] per worktree, in the given order, folding in **every** session
/// whose `cwd` matches that worktree's path (not just the first one - the real fix for the bug
/// the old `ProjectChild`-based `build_project_children` had: `sessions.iter().find(...)` only
/// ever surfaced one session per worktree, silently hiding any additional ones). Every worktree
/// appears here, including ones with no session (e.g. `main`, or a merged/prunable leftover).
pub fn build_worktree_rows(
    worktrees: &[WorktreeEntry],
    sessions: &[SessionRow],
) -> Vec<WorktreeRow> {
    worktrees
        .iter()
        .map(|worktree| {
            let sessions: Vec<SessionRow> = sessions
                .iter()
                .filter(|session| session.cwd == worktree.path)
                .cloned()
                .collect();
            WorktreeRow {
                path: worktree.path.clone(),
                label: worktree.label.clone(),
                branch: worktree.branch.clone(),
                note: worktree.note.clone(),
                error: worktree.error.clone(),
                sessions,
            }
        })
        .collect()
}

/// Filters a [`WorktreeRow`] list down to those matching `query` - applied *after*
/// [`build_worktree_rows`], so which worktrees have open sessions folded in is always decided
/// from the complete, unfiltered session list first.
pub fn filter_worktree_rows<'a>(rows: &'a [WorktreeRow], query: &str) -> Vec<&'a WorktreeRow> {
    rows.iter()
        .filter(|row| row.matches_filter(query))
        .collect()
}

/// One repo, with its already-built [`WorktreeRow`]s - `crate::root`'s reduction of one
/// [`crate::rail::repo::Repo`] plus (for the currently focused repo only) live worktree rows,
/// as input to [`group_worktrees_by_repo`]. See that function's own docs for why every other
/// repo currently supplies an empty `rows`.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoWorktrees {
    pub repo_id: RepoId,
    pub repo_name: String,
    pub rows: Vec<WorktreeRow>,
}

/// One repo group in the rail - `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md`
/// §2.0-2.1: the rail's only grouping axis, always present (even for a single repo), with its
/// own worktree rows ranked most-urgent-first inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoGroup {
    pub repo_id: RepoId,
    pub repo_name: String,
    /// Ranked by [`WorktreeRow::urgency_rank`], most urgent first - see
    /// [`group_worktrees_by_repo`].
    pub rows: Vec<WorktreeRow>,
}

impl RepoGroup {
    /// The group header's right-aligned amber `N worktrees waiting` (§2.1: "counting worktrees
    /// in that repo holding an asking or failed agent, hidden at zero"). Deliberately worktree-
    /// level, not agent-level: a worktree with two asking agents still counts once, since the
    /// header is answering "how many rows in this group want me", not "how many agents".
    pub fn waiting_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| {
                !row.sessions.is_empty()
                    && matches!(row.aggregate_status(), Status::Ask | Status::Fail)
            })
            .count()
    }

    /// This group's own rank for ordering groups - its most urgent worktree's
    /// [`WorktreeRow::urgency_rank`] (§2.0: "repos are ordered by their own most urgent
    /// worktree, using the same rank function as the rows"). A worktree-less repo (no data
    /// loaded for it yet - see [`group_worktrees_by_repo`]'s docs) sorts last, behind every repo
    /// that has at least one real row.
    fn rank(&self) -> u8 {
        self.rows
            .iter()
            .map(WorktreeRow::urgency_rank)
            .min()
            .unwrap_or(u8::MAX)
    }
}

/// Builds the rail's repo groups: each [`RepoWorktrees`]' rows sorted by
/// [`WorktreeRow::urgency_rank`] (§2.1's "worktrees are ranked by their most urgent agent"), then
/// the groups themselves sorted by [`RepoGroup::rank`] (§2.0's "repos are ordered by their own
/// most urgent worktree") - **never** alphabetically or by insertion order, per that section's
/// explicit warning. Both sorts are stable (`slice::sort_by_key`), so two rows/groups tied on
/// rank keep their caller-supplied relative order rather than reshuffling on every render.
///
/// A repo currently supplies an empty `rows` unless it's the focused one:
/// `crate::rail::repo::Repo::worktrees` isn't wired to live `wt_core::list_worktrees` data yet
/// for any *other* repo (see that field's own docs) - there is no UI to add a second repo yet
/// either (this revision deliberately doesn't build one), so in practice `repos` holds exactly
/// one entry and this is a no-op in every real session; the general mechanism is still built
/// correctly so a later phase that wires up multi-repo data has a real, tested place to plug
/// into rather than a single-repo special case to unwind.
pub fn group_worktrees_by_repo(repos: Vec<RepoWorktrees>) -> Vec<RepoGroup> {
    let mut groups: Vec<RepoGroup> = repos
        .into_iter()
        .map(|repo| {
            let mut rows = repo.rows;
            rows.sort_by_key(WorktreeRow::urgency_rank);
            RepoGroup {
                repo_id: repo.repo_id,
                repo_name: repo.repo_name,
                rows,
            }
        })
        .collect();
    groups.sort_by_key(RepoGroup::rank);
    groups
}

/// Whether a bare worktree row (no open session) matches a rail filter query - the "by
/// project" equivalent of [`SessionRow::matches_filter`], matched against its label, branch,
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
/// every session's worktree, and clean/merged notes for every listed worktree. Performs
/// blocking I/O (`git diff`/`git status`, `gix` object-database reads) - always run via a
/// background executor, never on the GPUI foreground thread.
pub struct StatusSnapshot {
    /// Keyed by worktree/session cwd - deduplicated by path since more than one open session
    /// can share a worktree.
    pub diffs: HashMap<PathBuf, DiffSummary>,
    /// Keyed by worktree path.
    pub worktree_notes: HashMap<PathBuf, WorktreeNote>,
    /// Real `wt_core::diff::ahead_behind_against_base` result per worktree/session cwd - the
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
            Ok(DiffBase::Diff(diff)) => {
                let (add, del) = sum_diff_stat(&diff);
                DiffSummary {
                    add,
                    del,
                    has_changes: !diff.files.is_empty(),
                }
            }
            // On the default branch, no base found, or a real error reading this one path:
            // no reviewable diff, but not a reason to fail the whole snapshot.
            Ok(DiffBase::OnDefaultBranch { .. } | DiffBase::NoBaseFound) | Err(_) => {
                DiffSummary::default()
            }
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
/// Does **not** know about live sessions; see [`prunable_worktree_paths`] for the function
/// that combines this with the live-session exclusion before anything is offered for removal.
pub fn is_prunable(worktree_notes: &HashMap<PathBuf, WorktreeNote>, path: &Path) -> bool {
    worktree_notes
        .get(path)
        .is_some_and(WorktreeNote::is_prunable)
}

/// The final list of worktree paths `crate::root::AdeApp::prune_worktrees` is allowed to
/// remove: every path that is a prune candidate per [`is_prunable`] **and** has no live
/// session running with its cwd inside it.
///
/// The live-session check isn't implied by `is_prunable`'s dirty check: a running process
/// with no uncommitted changes still leaves a clean tree, but removing its worktree directory
/// out from under it is real data loss - `wt_core::remove_worktree`'s own safety check has no
/// way to catch that. This exclusion happens once here, before `remove_worktree` is called
/// for any candidate.
pub fn prunable_worktree_paths(
    worktree_paths: &[PathBuf],
    worktree_notes: &HashMap<PathBuf, WorktreeNote>,
    live_session_cwds: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    worktree_paths
        .iter()
        .filter(|path| is_prunable(worktree_notes, path))
        .filter(|path| !live_session_cwds.contains(*path))
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
///
/// Performs blocking filesystem I/O; callers must offload this to a background executor.
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

    fn row(id: u64, status: Status, title: &str, cwd: &str) -> SessionRow {
        SessionRow {
            id,
            kind: SessionKind::Claude,
            title: title.to_string(),
            cwd: PathBuf::from(cwd),
            status,
            branch: Some("feature-x".to_string()),
            add: 0,
            del: 0,
            question_preview: None,
            exit_code: None,
            activity: None,
            elapsed: Duration::ZERO,
            review_file_count: None,
        }
    }

    /// [`SessionRow::activity`] is a plain, independent field - carrying a value must not change
    /// filtering (it isn't title/branch/kind text) or anything else about the row's identity.
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
    fn urgency_counts_with_no_sessions_is_all_zero_not_omitted() {
        let counts = urgency_counts(&[]);
        assert_eq!(counts.len(), 5);
        assert!(counts.iter().all(|(_, count)| *count == 0));
    }

    #[test]
    fn filter_sessions_matches_title_branch_and_kind_case_insensitively() {
        let rows = vec![
            row(1, Status::Run, "Fix Rate Limiter", "/a"),
            row(2, Status::Run, "Unrelated Work", "/b"),
        ];

        assert_eq!(filter_sessions(&rows, "rate").len(), 1);
        assert_eq!(filter_sessions(&rows, "RATE").len(), 1);
        assert_eq!(
            filter_sessions(&rows, "feature-x").len(),
            2,
            "both share the same branch"
        );
        assert_eq!(
            filter_sessions(&rows, "claude").len(),
            2,
            "both are Claude sessions"
        );
        assert_eq!(filter_sessions(&rows, "nonexistent").len(), 0);
        assert_eq!(
            filter_sessions(&rows, "  ").len(),
            2,
            "blank query matches everything"
        );
        assert_eq!(filter_sessions(&rows, "").len(), 2);
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
        // 11:04 UTC == 11 * 3600 + 4 * 60 seconds into the day.
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
    fn build_worktree_rows_includes_worktrees_with_no_session_as_empty_rows() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/leftover", clean_note(false)),
        ];
        let sessions: Vec<SessionRow> = Vec::new();

        let rows = build_worktree_rows(&worktrees, &sessions);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].sessions.is_empty());
        assert!(rows[1].sessions.is_empty());
        assert_eq!(rows[0].aggregate_status(), Status::Idle);
    }

    #[test]
    fn build_worktree_rows_folds_every_session_in_a_worktree_not_just_the_first() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/active", clean_note(false)),
        ];
        // Two sessions in the SAME worktree - the real bug the old `ProjectChild`-based
        // `build_project_children` had: `sessions.iter().find(...)` only ever surfaced the
        // first, silently hiding the second.
        let sessions = vec![
            row(1, Status::Run, "Fix bug", "/repo-wt/active"),
            row(2, Status::Ask, "Second tab", "/repo-wt/active"),
        ];

        let rows = build_worktree_rows(&worktrees, &sessions);
        assert_eq!(
            rows.len(),
            2,
            "every worktree still produces exactly one row"
        );
        assert!(rows[0].sessions.is_empty());
        assert_eq!(
            rows[1].sessions.len(),
            2,
            "both sessions in the same worktree must be folded into its one row, not just the \
             first one found"
        );
        assert_eq!(rows[1].sessions[0].id, 1);
        assert_eq!(rows[1].sessions[1].id, 2);
    }

    #[test]
    fn aggregate_status_picks_the_most_urgent_contained_session() {
        let worktrees = vec![worktree_entry("/repo-wt/a", clean_note(false))];
        let sessions = vec![
            row(1, Status::Run, "run", "/repo-wt/a"),
            row(2, Status::Ask, "ask", "/repo-wt/a"),
            row(3, Status::Idle, "idle", "/repo-wt/a"),
        ];
        let rows = build_worktree_rows(&worktrees, &sessions);
        assert_eq!(
            rows[0].aggregate_status(),
            Status::Ask,
            "Ask is the most urgent of Run/Ask/Idle per Status::ORDER"
        );
    }

    #[test]
    fn urgency_rank_matches_status_rank_for_a_worktree_with_sessions() {
        let worktrees = vec![
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
        ];
        let sessions = vec![
            row(1, Status::Ask, "ask", "/repo-wt/asking"),
            row(2, Status::Run, "run", "/repo-wt/running"),
        ];
        let rows = build_worktree_rows(&worktrees, &sessions);
        assert_eq!(rows[0].urgency_rank(), Status::Ask.urgency_rank());
        assert_eq!(rows[1].urgency_rank(), Status::Run.urgency_rank());
    }

    #[test]
    fn urgency_rank_splits_bare_and_prunable_below_every_session_status() {
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
            "a session-less, non-prunable worktree ranks 'bare' - below every real session \
             status but above prunable"
        );
        assert_eq!(
            rows[1].urgency_rank(),
            6,
            "a session-less, prunable worktree ranks last of all - §2.1's \
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
            "every real session status must outrank a bare worktree"
        );
    }

    fn repo_worktrees(id: u64, name: &str, rows: Vec<WorktreeRow>) -> RepoWorktrees {
        RepoWorktrees {
            repo_id: RepoId(id),
            repo_name: name.to_string(),
            rows,
        }
    }

    #[test]
    fn group_worktrees_by_repo_sorts_rows_inside_a_group_by_urgency_rank() {
        let worktrees = vec![
            worktree_entry("/repo-wt/bare", clean_note(false)),
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
        ];
        let sessions = vec![
            row(1, Status::Run, "run", "/repo-wt/running"),
            row(2, Status::Ask, "ask", "/repo-wt/asking"),
        ];
        // Deliberately built in a non-urgency order, so a passing test proves the function
        // itself sorts rather than merely preserving input order.
        let rows = build_worktree_rows(&worktrees, &sessions);

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
            "asking (rank 0) before running (rank 3) before a session-less bare row (rank 5)"
        );
    }

    #[test]
    fn group_worktrees_by_repo_orders_groups_by_their_own_most_urgent_worktree() {
        let quiet_worktrees = vec![worktree_entry("/quiet-repo/main", clean_note(true))];
        let quiet_rows = build_worktree_rows(&quiet_worktrees, &[]);

        let urgent_worktrees = vec![worktree_entry("/urgent-repo/wt", clean_note(false))];
        let urgent_sessions = vec![row(1, Status::Ask, "ask", "/urgent-repo/wt")];
        let urgent_rows = build_worktree_rows(&urgent_worktrees, &urgent_sessions);

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
    fn repo_group_waiting_count_counts_worktrees_asking_or_failed_hiding_zero_otherwise() {
        let worktrees = vec![
            worktree_entry("/repo-wt/asking", clean_note(false)),
            worktree_entry("/repo-wt/failed", clean_note(false)),
            worktree_entry("/repo-wt/running", clean_note(false)),
            worktree_entry("/repo-wt/bare", clean_note(false)),
        ];
        let sessions = vec![
            row(1, Status::Ask, "ask", "/repo-wt/asking"),
            row(2, Status::Fail, "fail", "/repo-wt/failed"),
            row(3, Status::Run, "run", "/repo-wt/running"),
        ];
        let rows = build_worktree_rows(&worktrees, &sessions);
        let groups = group_worktrees_by_repo(vec![repo_worktrees(0, "jerry-core", rows)]);
        assert_eq!(
            groups[0].waiting_count(),
            2,
            "counts the asking and the failed worktree, not the running or bare ones"
        );

        let all_quiet = build_worktree_rows(
            &[worktree_entry("/repo-wt/running-only", clean_note(false))],
            &[row(1, Status::Run, "run", "/repo-wt/running-only")],
        );
        let quiet_groups = group_worktrees_by_repo(vec![repo_worktrees(0, "quiet", all_quiet)]);
        assert_eq!(
            quiet_groups[0].waiting_count(),
            0,
            "hidden at zero - no asking or failed worktrees"
        );
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
    fn filter_worktree_rows_matches_session_title_worktree_label_or_worktree_path() {
        let with_session = {
            let worktrees = vec![worktree_entry("/a", clean_note(false))];
            let sessions = vec![row(1, Status::Run, "Fix rate limiter", "/a")];
            build_worktree_rows(&worktrees, &sessions).remove(0)
        };
        // "leftover-branch" is the label (leaf name only); "repo-worktrees" can only match
        // via the path fallback, not the label.
        let session_less = {
            let worktrees = vec![worktree_entry(
                "/repo-worktrees/leftover-branch",
                clean_note(false),
            )];
            build_worktree_rows(&worktrees, &[]).remove(0)
        };
        let rows = vec![with_session, session_less];

        assert_eq!(filter_worktree_rows(&rows, "").len(), 2);
        assert_eq!(
            filter_worktree_rows(&rows, "rate").len(),
            1,
            "matches only the row with the session, via its title"
        );
        assert_eq!(
            filter_worktree_rows(&rows, "leftover").len(),
            1,
            "matches only the session-less row, via its real (leaf-name) label"
        );
        assert_eq!(
            filter_worktree_rows(&rows, "repo-worktrees").len(),
            1,
            "matches only the session-less row, via its real path - the label alone never \
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
    fn prunable_worktree_paths_excludes_a_path_with_a_live_session_even_if_otherwise_prunable() {
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
        let mut live_session_cwds = HashSet::new();
        live_session_cwds.insert(merged_clean_path.clone());

        let candidates = prunable_worktree_paths(&worktree_paths, &notes, &live_session_cwds);
        assert!(
            candidates.is_empty(),
            "a worktree with a live session tracked against its path must never appear in \
             the prune candidate list, even though it is otherwise prunable"
        );

        // Same worktree, no live session anywhere near it - now it's really a candidate.
        let candidates_without_session =
            prunable_worktree_paths(&worktree_paths, &notes, &HashSet::new());
        assert_eq!(candidates_without_session, vec![merged_clean_path]);
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
        let mut live_session_cwds = HashSet::new();
        live_session_cwds.insert(a.clone());

        let candidates = prunable_worktree_paths(&worktree_paths, &notes, &live_session_cwds);
        assert_eq!(
            candidates,
            vec![b],
            "only the worktree with a live session should be excluded; unrelated prunable \
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

    /// End-to-end, against a real tempdir git repo: [`compute_status_snapshot`] reports a
    /// prunable, clean, merged worktree correctly, and a dirty one as not prunable, in one
    /// snapshot covering both the diff side and the worktree-note side.
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
