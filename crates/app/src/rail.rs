//! The session rail's data model: pure, GPUI-free types and functions for grouping,
//! filtering, and the "by project" worktree-without-a-session inclusion logic
//! (`design_handoff_jerry_ade/README.md`'s Zone 1). Deliberately has no `gpui` dependency so
//! the interesting logic - group order, filter matching, which worktrees get a plain row
//! versus a session row - is unit-testable directly (see the tests below) without a real
//! window, a real terminal, or real git state. `crate::root` is the one place that gathers
//! real signals (a live `TerminalPane`, `wt_core::list_worktrees`,
//! `wt_core::diff::diff_against_base`) into the plain data types this module operates on, and
//! renders the result as real GPUI elements.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::sessions::SessionKind;
use crate::status::Status;
use wt_core::diff::{DiffBase, DiffLineKind, WorktreeDiff, WorktreeMergeStatus};

/// Which of the two rail grouping modes is active - `design_handoff_jerry_ade/README.md`'s
/// `by urgency ▾ / by project ▾` control. Urgency is the documented default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RailMode {
    #[default]
    Urgency,
    Project,
}

impl RailMode {
    pub fn toggled(self) -> Self {
        match self {
            RailMode::Urgency => RailMode::Project,
            RailMode::Project => RailMode::Urgency,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RailMode::Urgency => "by urgency",
            RailMode::Project => "by project",
        }
    }
}

/// One session, already reduced to exactly what the rail row needs to render - built in
/// `crate::root` from a real `crate::sessions::Session` (its `TerminalPane`'s live process
/// signal and grid content) plus a real `wt_core::diff::diff_against_base` result for its
/// worktree. See `crate::status::derive_status` for how `status` itself was computed.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: crate::sessions::SessionId,
    pub kind: SessionKind,
    pub title: String,
    pub cwd: PathBuf,
    pub status: Status,
    pub branch: Option<String>,
    /// Real `+added -deleted` line counts from `wt_core::diff::diff_against_base`, summed
    /// across every changed file. Both `0` if the diff hasn't loaded yet or there are no
    /// changes.
    pub add: usize,
    pub del: usize,
    /// Real tail-of-pty text for a waiting session (`crate::terminal_pane::TerminalPane::
    /// visible_text_lines`, trimmed down to the last non-blank line) - `design_handoff_jerry_ade/
    /// README.md`'s "question preview". Only ever populated for [`Status::Ask`] rows; the
    /// design reserves this UI for waiting sessions specifically.
    pub question_preview: Option<String>,
    /// The real process exit code (`pty_core::ExitStatus::exit_code`), only for
    /// [`Status::Fail`]/[`Status::Review`]/session-exited-Idle rows. `None` while the
    /// process is still running or never started - see `crate::root::AdeApp::
    /// build_session_rows` for where this is read.
    pub exit_code: Option<u32>,
}

impl SessionRow {
    /// Whether this row matches a rail filter query - case-insensitive substring match
    /// against the title, the branch name, and the agent/session kind label. Real filtering
    /// (used by `crate::root`'s filter row), not decorative.
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
/// blank query (including one that's only whitespace) matches everything.
pub fn filter_sessions<'a>(rows: &'a [SessionRow], query: &str) -> Vec<&'a SessionRow> {
    rows.iter()
        .filter(|row| row.matches_filter(query))
        .collect()
}

/// Real `+added -deleted` totals for one worktree/session cwd, summed across every changed
/// file's hunks - built from a real `wt_core::diff::diff_against_base` result via
/// [`sum_diff_stat`], never fabricated. `has_changes` is `files` being non-empty (mirrors
/// `crate::status::derive_status`'s `has_reviewable_diff` input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffSummary {
    pub add: usize,
    pub del: usize,
    pub has_changes: bool,
}

/// Sums added/deleted line counts across every hunk of every file in a real
/// [`WorktreeDiff`] - the rail's `+N -M` stat is this, not a re-derivation of `git diff
/// --stat` (which this module doesn't call; the full line-level diff is already loaded by
/// `wt_core::diff::diff_against_base`, so re-parsing a second `--stat` invocation just to get
/// the same numbers would be redundant I/O).
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

/// One group in "by urgency" mode: a status plus every session row with that status, in the
/// order they were given (stable - `crate::root` is responsible for a sensible input order,
/// e.g. session-creation order).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusGroup {
    pub status: Status,
    pub rows: Vec<SessionRow>,
}

/// Groups `rows` by [`Status`], in the exact urgency order
/// `design_handoff_jerry_ade/README.md` specifies (`Status::ORDER`: `Needs input → Failed →
/// Review ready → Running → Idle`). Empty groups are omitted entirely rather than rendered as
/// a header with zero rows under it.
pub fn group_by_urgency(rows: &[SessionRow]) -> Vec<StatusGroup> {
    Status::ORDER
        .into_iter()
        .filter_map(|status| {
            let matching: Vec<SessionRow> = rows
                .iter()
                .filter(|row| row.status == status)
                .cloned()
                .collect();
            if matching.is_empty() {
                None
            } else {
                Some(StatusGroup {
                    status,
                    rows: matching,
                })
            }
        })
        .collect()
}

/// Real clean/merged state for one worktree row in "by project" mode - computed from
/// `wt_core::is_dirty` and `wt_core::diff::merge_status_against_base`, never fabricated. See
/// [`Self::label`] for how this becomes the design's `checkout · clean` /
/// `merged HH:MM · prunable` text.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeNote {
    /// The main checkout is always labeled `checkout`, never `merged ... prunable` - pruning
    /// the main worktree isn't a real `git worktree remove` operation (git refuses it
    /// outright) and it can't sensibly be "merged into" its own base.
    pub is_main: bool,
    /// `None` if `wt_core::is_dirty` itself failed (surfaced as a blank note rather than a
    /// guess - see [`Self::label`]).
    pub clean: Option<bool>,
    /// `None` if no base branch could be detected for this worktree (mirrors
    /// `wt_core::diff::DiffBase::NoBaseFound`) - a worktree with no detectable base is never
    /// treated as prunable, since "merged into what?" has no real answer.
    pub merge: Option<WorktreeMergeStatus>,
    /// From `wt_core::Worktree::is_locked` (real `git worktree lock` state) - a locked
    /// worktree is never offered as prunable, even if it's otherwise merged and clean:
    /// `git worktree lock` is an explicit user signal ("don't remove or move this"), most
    /// commonly because it lives on removable/networked storage that may be absent right
    /// now rather than genuinely abandoned. See [`Self::is_prunable`].
    pub is_locked: bool,
}

impl WorktreeNote {
    /// A worktree is a real prune candidate exactly when it is not the main checkout, is not
    /// locked, has no uncommitted changes (`wt_core::remove_worktree` would refuse a dirty
    /// one anyway - this mirrors that same safety check up front rather than only
    /// discovering it when `prune` is clicked), and its branch is fully merged into the
    /// detected base.
    ///
    /// This alone is **not** sufficient to actually remove a worktree - it says nothing
    /// about whether a live session is currently running inside it. See
    /// `crate::rail::prunable_worktree_paths` (the function `crate::root::AdeApp::
    /// prune_worktrees` actually calls) for that additional, separate exclusion.
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

/// Formats a Unix timestamp as `HH:MM` **in UTC**, not the viewer's local timezone. A
/// deliberate, documented simplification: `std` has no timezone database, and pulling one in
/// (`chrono-tz`, or the `time` crate's `local-offset` feature, which is unsound-by-default on
/// unix per its own advisory and requires opting back in) was judged not worth the dependency
/// weight for a single `HH:MM` label in a worktree note. Real timestamp, just not localized.
fn format_utc_hhmm(unix_seconds: i64) -> String {
    let seconds_in_day = unix_seconds.rem_euclid(86_400);
    let hours = seconds_in_day / 3600;
    let minutes = (seconds_in_day % 3600) / 60;
    format!("{hours:02}:{minutes:02}")
}

/// One real worktree, as input to [`build_project_children`] - `crate::root`'s reduction of
/// `wt_core::WorktreeResult` (via `crate::worktrees::WorktreeItem`) plus its real, separately
/// computed [`WorktreeNote`].
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    pub note: WorktreeNote,
    /// `Some(message)` if this worktree's metadata failed to read (mirrors
    /// `crate::worktrees::WorktreeItem::error`) - see `crate::root::AdeApp::
    /// build_worktree_entries`'s docs for why these are kept in the list (rendered as a
    /// real, visible error row) rather than silently filtered out, per `worktrees.rs`'s own
    /// documented intent for `WorktreeItem`.
    pub error: Option<String>,
}

/// One child row under a project header in "by project" mode: either a real session (if one
/// is running in that worktree) or a bare worktree row with its real clean/prunable note.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectChild {
    Session(SessionRow),
    Worktree(WorktreeEntry),
}

/// Builds the real "by project" child list: one entry per worktree, in the same order
/// `worktrees` was given, each replaced by its matching session row if one is currently open
/// in that exact worktree path. This is the concrete implementation of the README's "the
/// reason this mode exists" claim - **every** worktree appears here, including ones with no
/// session at all (e.g. `main`, or a merged/prunable leftover), not just the ones that happen
/// to have an active session.
pub fn build_project_children(
    worktrees: &[WorktreeEntry],
    sessions: &[SessionRow],
) -> Vec<ProjectChild> {
    worktrees
        .iter()
        .map(
            |worktree| match sessions.iter().find(|session| session.cwd == worktree.path) {
                Some(session) => ProjectChild::Session(session.clone()),
                None => ProjectChild::Worktree(worktree.clone()),
            },
        )
        .collect()
}

/// The project header's right-aligned status-dot cluster: every open session's status, sorted
/// by urgency (most urgent first) - worktree-only children (no session) contribute no dot,
/// since they have no [`Status`] of their own.
pub fn status_dot_cluster(children: &[ProjectChild]) -> Vec<Status> {
    let mut statuses: Vec<Status> = children
        .iter()
        .filter_map(|child| match child {
            ProjectChild::Session(row) => Some(row.status),
            ProjectChild::Worktree(_) => None,
        })
        .collect();
    statuses.sort_by_key(|status| status.urgency_rank());
    statuses
}

/// Whether a bare worktree row (no open session) matches a rail filter query - the "by
/// project" equivalent of [`SessionRow::matches_filter`], matched against its label, branch,
/// and real filesystem path. The path is a real, additional search target (not just
/// label/branch): `crate::worktrees::WorktreeItem::label` is always a *short* name (the
/// branch, or at most one directory-name segment - see that module's `label_for`), never a
/// full path, so a query for an ancestor directory component (e.g. a leftover-worktrees
/// container directory) would otherwise never match anything.
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

/// Whether a project-mode child row matches a rail filter query, dispatching to
/// [`SessionRow::matches_filter`] or [`matches_filter_worktree_entry`].
pub fn project_child_matches(child: &ProjectChild, query: &str) -> bool {
    match child {
        ProjectChild::Session(row) => row.matches_filter(query),
        ProjectChild::Worktree(entry) => matches_filter_worktree_entry(entry, query),
    }
}

/// Filters a real "by project" child list down to those matching `query` - applied *after*
/// [`build_project_children`], so which worktrees get a session row versus a plain worktree
/// row is always decided from the complete, unfiltered session list first (see
/// `crate::root`'s docs on why filtering must not itself change that assignment).
pub fn filter_project_children<'a>(
    children: &'a [ProjectChild],
    query: &str,
) -> Vec<&'a ProjectChild> {
    children
        .iter()
        .filter(|child| project_child_matches(child, query))
        .collect()
}

/// One real, already-completed round of the rail's periodic background refresh (see
/// `crate::root`'s status-polling task): real `+N -M` diff totals for every session's
/// worktree, and real clean/merged notes for every listed worktree. Performs blocking I/O
/// (spawns `git diff`/`git status` and reads the object database via `gix` for each distinct
/// path) - always run this via a background executor, never on a GPUI foreground thread; see
/// `wt_core`'s own crate-level docs for the same rule.
pub struct StatusSnapshot {
    /// Keyed by worktree/session cwd (real diffs are per-directory, and more than one open
    /// session can share the same worktree, so this is deduplicated by path rather than by
    /// session id).
    pub diffs: HashMap<PathBuf, DiffSummary>,
    /// Keyed by worktree path.
    pub worktree_notes: HashMap<PathBuf, WorktreeNote>,
}

/// One real worktree to compute a [`WorktreeNote`] for, as input to
/// [`compute_status_snapshot`] - `crate::root::AdeApp::start_status_polling`'s reduction of
/// `crate::worktrees::WorktreeItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeQuery {
    pub path: PathBuf,
    pub is_main: bool,
    pub is_locked: bool,
}

/// Computes one real [`StatusSnapshot`]: a `wt_core::diff::diff_against_base` for every
/// distinct path in `diff_paths` (deduplicated), plus a real `wt_core::is_dirty` +
/// `wt_core::diff::merge_status_against_base` for every [`WorktreeQuery`] in `worktrees`. A
/// failure computing any single path's diff or note is treated as "unknown for this path"
/// (an absent/default entry) rather than aborting the whole snapshot - one unreadable
/// worktree (e.g. one mid-deletion by something outside the app) must not blank out every
/// other row's real status.
pub fn compute_status_snapshot(
    worktrees: &[WorktreeQuery],
    diff_paths: &[PathBuf],
) -> StatusSnapshot {
    let mut unique_diff_paths: Vec<PathBuf> = diff_paths.to_vec();
    unique_diff_paths.sort();
    unique_diff_paths.dedup();

    let mut diffs = HashMap::with_capacity(unique_diff_paths.len());
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
    }
}

/// Whether `path` is a real prune candidate on its own merits: not the main checkout, not
/// locked, clean, and merged - see [`WorktreeNote::is_prunable`]. This does **not** know
/// about live sessions; see [`prunable_worktree_paths`] for the function that actually
/// combines this with the live-session exclusion before anything is ever offered for
/// removal.
pub fn is_prunable(worktree_notes: &HashMap<PathBuf, WorktreeNote>, path: &Path) -> bool {
    worktree_notes
        .get(path)
        .is_some_and(WorktreeNote::is_prunable)
}

/// The real, final list of worktree paths `crate::root::AdeApp::prune_worktrees` is allowed
/// to remove: every path that is a prune candidate per [`is_prunable`] **and** has no live
/// session currently running with its cwd inside it.
///
/// This second condition is not implied by `is_prunable`'s own dirty check: a running
/// process with no uncommitted changes doesn't make git consider the worktree dirty (an
/// agent that's just sitting there, or a shell with nothing typed, leaves a perfectly clean
/// tree), but removing the worktree directory out from under a still-running process is real
/// data loss and a real broken process - not merely an unwanted deletion `wt_core::
/// remove_worktree`'s own safety check would catch. This function exists specifically so
/// that exclusion happens once, before `wt_core::remove_worktree` is ever called for any
/// candidate, rather than being left to that lower-level safety net alone.
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
/// reporting a truncated (real, but incomplete lower-bound) total - see [`disk_usage_bytes`].
/// This project's own repository (which its rail footer walks when the app is run against
/// itself for verification) contains a full nested `vendor/zed` checkout - tens of thousands
/// of files on its own - so an unbounded recursive walk would make the rail footer's cost
/// unpredictable. 50,000 keeps a realistic worktree's walk complete while still bounding the
/// pathological case.
pub const DISK_USAGE_WALK_FILE_CAP: usize = 50_000;

/// Sums real file sizes (`std::fs::Metadata::len`) recursively under `root`, via
/// `std::fs::read_dir` - a genuine, if bounded and best-effort, disk-usage figure, not a
/// fabricated placeholder. Returns `(total_bytes, truncated)`; `truncated` is `true` if
/// [`DISK_USAGE_WALK_FILE_CAP`] was hit, meaning `total_bytes` is real but incomplete (a real
/// lower bound, never an overcount). Symlinks are not followed (`DirEntry::metadata` on unix
/// is `lstat`-based, i.e. does not dereference symlinks), so a cyclic symlink can't loop this
/// walk. Unreadable entries (permission errors, or a concurrent delete racing this read) are
/// skipped rather than aborting the whole walk - a disk-usage estimate should degrade
/// gracefully, not go blank because of one unreadable subdirectory.
///
/// Performs blocking filesystem I/O; callers must offload this to a background executor (see
/// `crate::root::AdeApp::load_disk_usage`), never call it from a GPUI foreground thread.
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
        }
    }

    #[test]
    fn group_by_urgency_orders_groups_needs_input_first_idle_last() {
        let rows = vec![
            row(1, Status::Idle, "idle one", "/a"),
            row(2, Status::Run, "run one", "/b"),
            row(3, Status::Ask, "ask one", "/c"),
            row(4, Status::Fail, "fail one", "/d"),
            row(5, Status::Review, "review one", "/e"),
        ];
        let groups = group_by_urgency(&rows);
        let statuses: Vec<Status> = groups.iter().map(|g| g.status).collect();
        assert_eq!(
            statuses,
            vec![
                Status::Ask,
                Status::Fail,
                Status::Review,
                Status::Run,
                Status::Idle
            ]
        );
    }

    #[test]
    fn group_by_urgency_omits_empty_statuses_and_keeps_every_row_of_present_ones() {
        let rows = vec![
            row(1, Status::Ask, "ask one", "/a"),
            row(2, Status::Ask, "ask two", "/b"),
        ];
        let groups = group_by_urgency(&rows);
        assert_eq!(groups.len(), 1, "only Ask is present, so only one group");
        assert_eq!(groups[0].status, Status::Ask);
        assert_eq!(groups[0].rows.len(), 2);
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
        // A locked worktree can be genuinely merged and clean - `git worktree lock` is an
        // explicit "don't touch this" signal independent of merge/dirty state (e.g. it lives
        // on removable storage) - so it must never be offered as prunable, even though every
        // other prunability condition holds.
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
        // Realistic label shape: `crate::worktrees::WorktreeItem::label` (via `label_for`)
        // is always a short name - the branch, or at most one directory-name segment - never
        // a full path. A test helper that instead sets `label` to the whole path would
        // exercise filter behavior the real app can never produce.
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
    fn build_project_children_includes_worktrees_with_no_session_as_worktree_rows() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/leftover", clean_note(false)),
        ];
        let sessions: Vec<SessionRow> = Vec::new();

        let children = build_project_children(&worktrees, &sessions);
        assert_eq!(children.len(), 2);
        assert!(matches!(children[0], ProjectChild::Worktree(_)));
        assert!(matches!(children[1], ProjectChild::Worktree(_)));
    }

    #[test]
    fn build_project_children_replaces_a_worktree_with_its_session_when_one_is_open() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/active", clean_note(false)),
        ];
        let sessions = vec![row(1, Status::Run, "Fix bug", "/repo-wt/active")];

        let children = build_project_children(&worktrees, &sessions);
        assert_eq!(
            children.len(),
            2,
            "every worktree still produces exactly one row"
        );
        assert!(matches!(children[0], ProjectChild::Worktree(_)));
        match &children[1] {
            ProjectChild::Session(session) => assert_eq!(session.id, 1),
            other => panic!("expected the active worktree to show its session, got {other:?}"),
        }
    }

    #[test]
    fn status_dot_cluster_only_counts_sessions_sorted_by_urgency() {
        let worktrees = vec![
            worktree_entry("/repo", clean_note(true)),
            worktree_entry("/repo-wt/a", clean_note(false)),
            worktree_entry("/repo-wt/b", clean_note(false)),
        ];
        let sessions = vec![
            row(1, Status::Run, "run", "/repo-wt/a"),
            row(2, Status::Ask, "ask", "/repo-wt/b"),
        ];
        let children = build_project_children(&worktrees, &sessions);
        let dots = status_dot_cluster(&children);
        assert_eq!(
            dots,
            vec![Status::Ask, Status::Run],
            "worktree-only child contributes no dot; sessions are sorted by urgency"
        );
    }

    #[test]
    fn rail_mode_toggles_and_defaults_to_urgency() {
        assert_eq!(RailMode::default(), RailMode::Urgency);
        assert_eq!(RailMode::Urgency.toggled(), RailMode::Project);
        assert_eq!(RailMode::Project.toggled(), RailMode::Urgency);
    }

    #[test]
    fn filter_project_children_matches_session_title_worktree_label_or_worktree_path() {
        let session_child = ProjectChild::Session(row(1, Status::Run, "Fix rate limiter", "/a"));
        // Real `WorktreeEntry::label` shape (via `worktree_entry`'s helper, matching
        // `crate::worktrees::label_for`) is just the leaf name - "leftover-branch" - never
        // the full path, so a query for the *container* directory
        // ("repo-worktrees") can only ever match through the real path-search fallback in
        // `matches_filter_worktree_entry`, not through the label.
        let worktree_child = ProjectChild::Worktree(worktree_entry(
            "/repo-worktrees/leftover-branch",
            clean_note(false),
        ));
        let children = vec![session_child, worktree_child];

        assert_eq!(filter_project_children(&children, "").len(), 2);
        assert_eq!(
            filter_project_children(&children, "rate").len(),
            1,
            "matches only the session row, via its title"
        );
        assert_eq!(
            filter_project_children(&children, "leftover").len(),
            1,
            "matches only the worktree row, via its real (leaf-name) label"
        );
        assert_eq!(
            filter_project_children(&children, "repo-worktrees").len(),
            1,
            "matches only the worktree row, via its real path - the label alone never \
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
        // The critical fix: a worktree that is genuinely merged and clean (a real prune
        // candidate per `is_prunable` alone) must never be offered for removal while a
        // session's cwd is inside it - `wt_core::is_dirty` has no way to know a live,
        // clean-tree process is running there, so this exclusion must happen independently.
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

    /// Real end-to-end proof (a genuine tempdir git repo, real `git`/`gix` calls - the same
    /// pattern `wt_core`'s own tests use) that [`compute_status_snapshot`] reports a
    /// prunable, clean, merged worktree correctly, and a dirty one as not prunable, in one
    /// real snapshot covering both the diff side and the worktree-note side at once.
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
