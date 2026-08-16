//! Interactive rebase - GitHub issue #242 phase B: the graph pane's own **mode**, not a modal or
//! a separate screen (design spec §1). This file owns the mode's state and every mutation
//! (entering/leaving, editing the in-memory plan, driving `wt_core::rebase` for real); rendering
//! lives in `crate::graph_view::rebase_render`.
//!
//! ## What this is a thin front-end over
//!
//! Every real git mutation goes straight through `wt_core::rebase` (`start_interactive_rebase`/
//! `continue_rebase`/`skip_rebase_commit`/`abort_rebase`) - see that module's own docs for the
//! real stopping semantics this UI has to reflect honestly: `edit` and message-less `reword`
//! both stop; `pick`/`squash`/`fixup`/`drop` and message-supplied `reword` never do. This module
//! never simulates an outcome - every phase transition below is driven by a real
//! `wt_core::rebase::RebaseOutcome` a real subprocess produced.
//!
//! ## Plan order
//!
//! [`RebaseModeState::plan`] is oldest-first, applied top to bottom - `wt_core::rebase`'s own
//! `RebasePlanEntry` slice order (see [`AdeApp::enter_rebase_mode`], which builds it from
//! `wt_core::rebase::commits_to_rebase`, itself already oldest-first). The graph pane's own
//! commit list is newest-first; this mode deliberately reverses it.

use super::*;
use crate::text_history::TextField;
use crate::work_surface::agents::AgentId;
use gpui::{FocusHandle, KeyDownEvent, Task};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use wt_core::rebase::{RebaseAction, RebaseOutcome, RebasePlanEntry};

/// One of git's six interactive-rebase verbs, as the UI edits it - a thin mirror of
/// `wt_core::rebase::RebaseAction` that keeps `Reword`'s optional message in
/// [`RebasePlanRow::reword_message`] instead of folded into the variant itself, since the UI
/// needs a real text field (with its own focus handle) for every reword row regardless of
/// whether a message has been typed yet - see [`RebasePlanRow::to_plan_entry`] for where the two
/// shapes reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebaseActionKind {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

impl RebaseActionKind {
    pub(crate) const ALL: [RebaseActionKind; 6] = [
        RebaseActionKind::Pick,
        RebaseActionKind::Reword,
        RebaseActionKind::Edit,
        RebaseActionKind::Squash,
        RebaseActionKind::Fixup,
        RebaseActionKind::Drop,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            RebaseActionKind::Pick => "pick",
            RebaseActionKind::Reword => "reword",
            RebaseActionKind::Edit => "edit",
            RebaseActionKind::Squash => "squash",
            RebaseActionKind::Fixup => "fixup",
            RebaseActionKind::Drop => "drop",
        }
    }

    /// The action menu's own one-line hint (design spec §1.4's action table, "Menu hint" column,
    /// verbatim). Lives beside [`Self::label`] rather than in `crate::graph_view::rebase_render`
    /// because it is content, not presentation: the six of them being one exhaustive `match` is
    /// what keeps §4's "menu hints one line each" from being satisfiable for only some actions.
    pub(crate) fn hint(self) -> &'static str {
        match self {
            RebaseActionKind::Pick => "keep the commit as it is",
            RebaseActionKind::Reword => "stop to edit the message",
            RebaseActionKind::Edit => "stop to amend the contents",
            RebaseActionKind::Squash => "fold up, keep both messages",
            RebaseActionKind::Fixup => "fold up, discard this message",
            RebaseActionKind::Drop => "remove the commit",
        }
    }

    /// Whether this action folds its own row into the previous resulting block (design spec
    /// §1.4's fold-elbow indicator, §1.6's "N commits folded in") - `squash`/`fixup` only.
    pub(crate) fn folds_into_previous(self) -> bool {
        matches!(self, RebaseActionKind::Squash | RebaseActionKind::Fixup)
    }
}

/// One row of the in-memory, user-editable plan - the UI's own richer twin of
/// `wt_core::rebase::RebasePlanEntry` (which only ever exists transiently, built fresh by
/// [`Self::to_plan_entry`] the moment a real rebase call needs one).
pub(crate) struct RebasePlanRow {
    /// The real, full commit object id - never abbreviated, matching
    /// `wt_core::rebase::RebasePlanEntry::commit`'s own contract.
    pub commit: String,
    pub short_sha: String,
    /// The commit's real original subject line - never mutated once loaded; a reword row's
    /// *proposed* replacement lives in [`Self::reword_message`] instead, so this always stays
    /// available as the "differs from the original" baseline [`Self::has_supplied_reword_message`]
    /// checks against.
    pub original_subject: String,
    /// The real count of files this commit changed (`wt_core::graph::commit_changed_files`),
    /// loaded once when the plan is built. `None` only if that real lookup itself failed (a
    /// genuine error, not "still loading" - the whole plan is built in one background pass
    /// before [`RebaseModeState`] ever shows a row at all) - never fabricated as `0`.
    pub files_changed: Option<usize>,
    pub action: RebaseActionKind,
    /// A real, editable single-line text field, always present (regardless of `action`) and
    /// pre-filled with `original_subject` via `TextField::seeded` - only actually rendered/read
    /// while `action == Reword`, but kept alive across an action change so switching a row to
    /// `Reword` and back doesn't lose whatever the user already typed.
    pub reword_message: TextField,
    pub reword_focus_handle: FocusHandle,
}

impl RebasePlanRow {
    /// Design spec §1.4's own definition: a reword row counts as having a supplied message when
    /// its field is non-empty **and differs from the original subject** - the pre-filled
    /// original text is never itself treated as an answer.
    pub(crate) fn has_supplied_reword_message(&self) -> bool {
        let text = self.reword_message.as_str();
        !text.is_empty() && text != self.original_subject
    }

    /// The real `RebasePlanEntry` this row contributes when `Start rebase`/the plan is sent to
    /// `wt_core::rebase`. `Reword` maps to `Some(message)` only when
    /// [`Self::has_supplied_reword_message`] is true - a message-less reword genuinely means
    /// "stop here" ([`wt_core::rebase::RebaseAction::Reword`]'s own docs), never fabricated.
    pub(crate) fn to_plan_entry(&self) -> RebasePlanEntry {
        let action = match self.action {
            RebaseActionKind::Pick => RebaseAction::Pick,
            RebaseActionKind::Edit => RebaseAction::Edit,
            RebaseActionKind::Squash => RebaseAction::Squash,
            RebaseActionKind::Fixup => RebaseAction::Fixup,
            RebaseActionKind::Drop => RebaseAction::Drop,
            RebaseActionKind::Reword => {
                if self.has_supplied_reword_message() {
                    RebaseAction::Reword(Some(self.reword_message.as_str().to_string()))
                } else {
                    RebaseAction::Reword(None)
                }
            }
        };
        RebasePlanEntry {
            commit: self.commit.clone(),
            action,
        }
    }

    /// Design spec §1.5: whether this row is a **planned** pause point, computed live from the
    /// current in-memory plan alone (before `Start rebase` has ever run a real rebase) - any
    /// `edit` row, or any `reword` row with no supplied message yet.
    pub(crate) fn is_planned_pause(&self) -> bool {
        matches!(self.action, RebaseActionKind::Edit)
            || (self.action == RebaseActionKind::Reword && !self.has_supplied_reword_message())
    }
}

/// Which phase the mode is in - design spec §1.2's "Planning phase" vs. "Stopped phase" banner
/// contents. There is no separate "Completed" phase: a real `RebaseOutcome::Completed` leaves
/// rebase mode entirely (see [`AdeApp::apply_rebase_outcome`]), returning the graph pane to its
/// ordinary commit list - the real, freshly-rebased history is exactly what that list should be
/// showing next, not a lingering "it worked" screen this module would have to fabricate content
/// for.
#[derive(Debug)]
pub(crate) enum RebasePhase {
    Planning,
    /// `outcome` is always `RebaseOutcome::StoppedForEdit`/`RebaseOutcome::StoppedForConflict` -
    /// never `Completed` (see this enum's own docs for why a completed rebase leaves the mode
    /// entirely rather than being representable here).
    Stopped {
        outcome: RebaseOutcome,
    },
}

/// The interactive-rebase mode's whole real state - `Some` only while the graph pane is showing
/// it, owned by `GraphTabState::rebase`. `None` is the pane's ordinary commit-list mode.
pub(crate) struct RebaseModeState {
    /// The real worktree this rebase was entered against, captured once from `self.diff_root` at
    /// [`AdeApp::enter_rebase_mode`] time and used for every real git call this mode ever makes -
    /// never `self.diff_root` read fresh at click time. GitHub issue #242 phase B's own real,
    /// independently-reproduced bug: without this, switching worktrees/repos in the rail while
    /// rebase mode was still live silently re-pointed every subsequent click (`Continue`,
    /// `Start`, ...) at whatever `self.diff_root` had since become - since every worktree of the
    /// same repo shares one real object database, commit ids from the *original* worktree
    /// resolved fine there too, so a `Continue` genuinely amended the *new* worktree's `HEAD` and
    /// a `Start` genuinely rewrote its branch. [`AdeApp::reset_repo_scoped_state`]/[`AdeApp::
    /// close_git_graph_tab`] are the real primary defense now (they leave rebase mode outright on
    /// any such switch - see [`AdeApp::leave_rebase_mode`]); this field is the backstop
    /// (`AdeApp::rebase_worktree_root`'s own docs) for the case that primary defense somehow
    /// doesn't catch.
    pub worktree_root: PathBuf,
    pub branch: String,
    /// The real commit-ish `wt_core::rebase::start_interactive_rebase` bases the plan onto - the
    /// row the user opened "Rebase onto this commit" on. Never itself a row in
    /// [`Self::plan`] (exactly `git rebase -i <onto>`'s own semantics: `onto` becomes the new
    /// parent, not a picked commit).
    pub onto: String,
    pub onto_short: String,
    /// Oldest-first, applied top to bottom - see this module's own docs.
    pub plan: Vec<RebasePlanRow>,
    pub phase: RebasePhase,
    /// Which plan row is selected (design spec §1.4: "Row selection: 2px left edge `#3f5b74` on
    /// bg `#1a1e21`"), and the row every keyboard action in §1.4's footer hint strip
    /// (`P pick · S squash · D drop`, `alt+↑↓ reorder`) acts on. A plain index rather than a
    /// commit id, unlike [`Self::dragging_row`]: selection has to survive a reorder as *"the row
    /// I am working on stays under the cursor"*, and [`AdeApp::move_selected_rebase_plan_row`]
    /// moves it deliberately alongside the row it moved. Clamped by [`Self::selected_index`] so a
    /// plan that shrinks (or has not loaded yet) can never index out of bounds.
    pub selected_row: usize,
    /// Which row's action-chip dropdown (design spec §1.4: `<action> ▾`) is open, if any.
    pub action_menu_open: Option<usize>,
    /// `true` while the plan is still being built (the background `commits_to_rebase`/
    /// `commit_changed_files`/`commits_already_on_upstream` pass) or a real
    /// `start_interactive_rebase`/`continue_rebase`/`skip_rebase_commit`/`abort_rebase` call is
    /// in flight - guards every button against a double-click starting a second, overlapping
    /// real git subprocess.
    pub op_in_flight: bool,
    /// Real ids `wt_core::graph::commits_already_on_upstream` reported for this plan's commits -
    /// design spec §1.6 warning 2 ("a force-with-lease push will be needed afterward"). Empty
    /// covers both "no commits are already pushed" and "no upstream configured at all" - either
    /// way, there is nothing to warn about, so the two cases collapsing is correct, not a loss of
    /// information the UI needs.
    pub already_on_upstream: Vec<String>,
    /// Every [`AgentId`] this session's own "Pause now" (design spec §1.6 warning 1) has really
    /// suspended via `TerminalPane::pause` - resumed automatically
    /// ([`AdeApp::resume_paused_rebase_agents`]) the moment the mode reaches a terminal state
    /// (`Completed`, or the user leaves via Cancel/Abort). Never includes an agent this session
    /// didn't itself pause.
    pub paused_agents: Vec<AgentId>,
    /// Which row is being dragged for reordering (design spec §1.4's drag handle - pre-`Start`
    /// only), and which row/side the cursor is currently hovering - both real commit ids, never
    /// a raw `Vec` index (an index shifts under a remove/insert mid-drag; identity doesn't).
    /// Mirrors `work_surface::state`'s own `DraggedTab`/`tab_drag_insertion` shape and its
    /// identical "match by stable identity, not position" reasoning
    /// ([`move_rebase_plan_row`]'s own docs mirror `work_surface::state::move_tab_order`'s).
    pub dragging_row: Option<String>,
    pub drag_insertion: Option<(String, bool)>,
    pub _task: Option<Task<()>>,
}

impl RebaseModeState {
    /// [`Self::selected_row`], clamped to a row that really exists right now - `None` only for a
    /// genuinely empty plan (the mode's own loading phase, before the background pass has built a
    /// single row). Every reader goes through this rather than indexing `plan[selected_row]`
    /// directly: the plan is rebuilt from scratch on a real reload, and a stale index outliving
    /// the row it pointed at would panic.
    pub(crate) fn selected_index(&self) -> Option<usize> {
        if self.plan.is_empty() {
            return None;
        }
        Some(self.selected_row.min(self.plan.len() - 1))
    }
}

/// One block of [`derive_result_blocks`]'s output - the Result panel's own per-resulting-commit
/// row (design spec §1.6).
pub(crate) struct ResultBlock {
    /// The reworded text live from the field, or the original subject for any other action.
    pub subject: String,
    pub short_sha: String,
    /// How many `squash`/`fixup` rows folded into this block.
    pub folded_count: usize,
    pub status: ResultBlockStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultBlockStatus {
    Normal,
    Reworded,
    StopsForMessage,
    StopsToAmend,
}

/// Design spec §1.2/§1.6's `N -> M commits`: walk the plan, `drop` skips, `squash`/`fixup` fold
/// into the previous row, everything else counts as one resulting commit. Just
/// `derive_result_blocks(plan).len()` - kept as its own named function since both the banner and
/// the Result panel header quote this number without needing every block's own detail.
pub(crate) fn derive_result_commit_count(plan: &[RebasePlanRow]) -> usize {
    derive_result_blocks(plan).len()
}

/// Design spec §1.6: walk the plan **oldest to newest, exactly as git applies it** - `drop` skips
/// the row entirely, `squash`/`fixup` fold into the *previous* resulting block rather than
/// creating a new one, everything else appends a new block. A `squash`/`fixup` row with no
/// preceding block (a transient, mid-edit plan shape - e.g. the very first row was just changed
/// to `squash`) folds into nothing and is silently dropped from the count, rather than fabricating
/// a block for it to fold into.
pub(crate) fn derive_result_blocks(plan: &[RebasePlanRow]) -> Vec<ResultBlock> {
    let mut blocks: Vec<ResultBlock> = Vec::new();
    for row in plan {
        if row.action.folds_into_previous() {
            if let Some(last) = blocks.last_mut() {
                last.folded_count += 1;
            }
            continue;
        }
        if row.action == RebaseActionKind::Drop {
            continue;
        }
        let (subject, status) = match row.action {
            RebaseActionKind::Edit => (
                row.original_subject.clone(),
                ResultBlockStatus::StopsToAmend,
            ),
            RebaseActionKind::Reword => {
                if row.has_supplied_reword_message() {
                    (
                        row.reword_message.as_str().to_string(),
                        ResultBlockStatus::Reworded,
                    )
                } else {
                    (
                        row.original_subject.clone(),
                        ResultBlockStatus::StopsForMessage,
                    )
                }
            }
            _ => (row.original_subject.clone(), ResultBlockStatus::Normal),
        };
        blocks.push(ResultBlock {
            subject,
            short_sha: row.short_sha.clone(),
            folded_count: 0,
            status,
        });
    }
    blocks
}

/// Design spec §1.6 warning 3: `Stops N times` - the count of `edit` rows plus message-less
/// `reword` rows, exactly [`RebasePlanRow::is_planned_pause`]'s own definition summed over the
/// plan.
pub(crate) fn derive_stop_count(plan: &[RebasePlanRow]) -> usize {
    plan.iter().filter(|row| row.is_planned_pause()).count()
}

/// The real reorder math behind design spec §1.4's drag handle - moves `dragged_commit`'s row to
/// sit immediately before/after `target_commit`'s row, entirely in memory. Real git is never
/// touched here; that only happens once `Start rebase` is clicked. Mirrors
/// `work_surface::state::move_tab_order` exactly: matched by stable identity (a real commit id),
/// not by position, and a silent no-op if either id isn't found or they're the same row.
/// Dropping a row on its own slot is always a no-op, and a stale drag (e.g. the plan reloaded
/// mid-drag) must never panic or corrupt the plan.
pub(crate) fn move_rebase_plan_row(
    plan: &mut Vec<RebasePlanRow>,
    dragged_commit: &str,
    target_commit: &str,
    insert_after: bool,
) {
    if dragged_commit == target_commit {
        return;
    }
    let Some(from) = plan.iter().position(|row| row.commit == dragged_commit) else {
        return;
    };
    if !plan.iter().any(|row| row.commit == target_commit) {
        return;
    }
    let row = plan.remove(from);
    let mut to = plan
        .iter()
        .position(|row| row.commit == target_commit)
        .unwrap_or(plan.len());
    if insert_after {
        to += 1;
    }
    plan.insert(to, row);
}

/// Design spec §1.5's **filled** marker: the real commit id a `RebaseOutcome::StoppedForEdit`/
/// `StoppedForConflict` reports as where the rebase actually stopped, if any. `Completed` has no
/// such commit - unreachable in practice via [`RebasePhase::Stopped`] (see that variant's own
/// docs), handled here anyway rather than assuming the caller upholds it.
pub(crate) fn outcome_stopped_commit(outcome: &RebaseOutcome) -> Option<&str> {
    match outcome {
        RebaseOutcome::StoppedForEdit { commit, .. } => Some(commit.as_str()),
        RebaseOutcome::StoppedForConflict { commit, .. } => Some(commit.as_str()),
        RebaseOutcome::Completed => None,
    }
}

impl AdeApp {
    /// The row menu's "Rebase onto this commit" action (design spec §1) - enters rebase mode with
    /// `from_row_index`'s own commit as the real `onto` target
    /// (`wt_core::rebase::commits_to_rebase`'s own contract: every commit `HEAD` has that isn't
    /// reachable from `onto`, oldest first - exactly `git rebase -i <onto>`'s default todo). A
    /// no-op for the synthetic "Working tree" row (`row.commit.id` is empty there - never a
    /// real rebase target).
    ///
    /// GitHub issue #241 made this the row menu's *only* rebase entry: a second, separate
    /// "rebase onto this commit, immediately, with no plan shown" row ran the same replay while
    /// skipping the Planning banner entirely, which is exactly the review step the banner's own
    /// one-click `Start rebase` already makes cheap. One entry, one banner, no capability lost.
    pub(crate) fn enter_rebase_mode(&mut self, from_row_index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.current_graph_row(from_row_index) else {
            return;
        };
        let onto = row.commit.id.clone();
        let onto_short = row.commit.short_id.clone();
        self.enter_rebase_mode_inner(onto, onto_short, cx);
    }

    /// The Branches panel's branch menu "Rebase current branch on Branch…" action (GitHub issue
    /// #241) - the same rebase mode [`Self::enter_rebase_mode`] enters, targeting `branch`'s real
    /// tip commit.
    ///
    /// Resolves the branch to a real commit first (`wt_core::graph::resolve_commit`, a real `git
    /// log -1` in the focused worktree) rather than handing the branch *name* to the rebase
    /// engine, for two real reasons: a branch is a moving pointer, so pinning the tip that was
    /// really there when the user clicked is what makes the plan the banner then shows honest;
    /// and [`RebaseModeState::onto`] is a commit id everywhere else, which the banner and
    /// `wt_core::rebase` both already depend on.
    ///
    /// That resolution is real blocking I/O, so it runs on the background executor and calls
    /// [`Self::enter_rebase_mode_inner`] when it lands - never inline on the UI thread. A branch
    /// that no longer resolves (deleted since the panel last loaded) reports git's own real error
    /// through the graph tab's status line and enters no mode at all.
    pub(in crate::graph_view) fn enter_rebase_mode_onto_branch(
        &mut self,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.branch_menu_open = None;
        self.graph_state.delete_branch_confirm_armed = None;
        if self.graph_state.rebase.is_some() {
            // Checked here as well as in `enter_rebase_mode_inner` (which re-checks after the
            // await, and is the real guard): no reason to spawn a resolve for a mode that already
            // cannot be entered.
            return;
        }
        let root = self.diff_root.clone();
        self.graph_state.status_message = Some(format!("Rebase onto {branch}\u{2026}"));
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let branch_for_bg = branch.clone();
            let resolved = cx
                .background_executor()
                .spawn(async move { wt_core::graph::resolve_commit(&root, &branch_for_bg) })
                .await;
            let _ = this.update(cx, |this, cx| match resolved {
                Ok(commit) => {
                    if this.graph_state.rebase.is_some() {
                        // A rebase mode appeared while this resolve was in flight;
                        // `enter_rebase_mode_inner` would refuse silently, so the refusal is
                        // reported here instead of leaving a pending "Rebase onto x…" line that
                        // nothing will ever resolve.
                        this.graph_state.status_message = Some(format!(
                            "Rebase onto {branch} failed: a rebase is already in progress"
                        ));
                        cx.notify();
                        return;
                    }
                    // Rebase mode replaces the toolbar this message paints in with its own
                    // banner, so leaving a stale "Rebase onto x…" behind would only ever be read
                    // later, after the mode is left, as if something were still pending.
                    this.graph_state.status_message = None;
                    this.enter_rebase_mode_inner(commit.id, commit.short_id, cx);
                }
                Err(err) => {
                    this.graph_state.status_message =
                        Some(format!("Rebase onto {branch} failed: {err}"));
                    cx.notify();
                }
            });
        });
        self.graph_state._branch_resolve_task = Some(task);
    }

    /// The body of [`Self::enter_rebase_mode`], taking the real `onto` commit id (and its short
    /// form, for display) rather than a row index - so nothing downstream depends on that commit
    /// still occupying a particular row of the currently loaded graph. A background reload between
    /// opening the row menu and clicking genuinely renumbers rows, while a commit id does not move.
    ///
    /// GitHub issue #242 is what this rides on, and the reason the row menu's rebase entry stopped
    /// calling `wt_core::rewrite::rebase_onto` (a plain `git rebase <onto>`) at all: a conflict
    /// stops in [`RebasePhase::Stopped`], where the existing banner offers real
    /// `Continue`/`Skip`/`Abort` and `Resolve in the diff view`. The plain `git rebase` left the
    /// worktree genuinely mid-rebase with nothing in this app able to continue, skip or abort it -
    /// a real dead end, recoverable only from a terminal.
    pub(in crate::graph_view) fn enter_rebase_mode_inner(
        &mut self,
        onto: String,
        onto_short: String,
        cx: &mut Context<Self>,
    ) {
        self.graph_state.row_menu_open = None;
        self.graph_state.hard_reset_confirm_armed = None;
        // The synthetic "Working tree" row carries an empty commit id - never a real
        // rebase target.
        if onto.is_empty() {
            return;
        }
        if self.graph_state.rebase.is_some() {
            // A rebase mode is already live (its own banner owns the recovery surface for it) -
            // starting a second one over the top would strand the first. The row menu itself
            // cannot reach this (it is unreachable while rebase mode is showing); this is the
            // backstop for any other caller.
            return;
        }
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == self.diff_root)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| "(detached)".to_string());

        let worktree_root = self.diff_root.clone();
        self.graph_state.rebase = Some(RebaseModeState {
            worktree_root: worktree_root.clone(),
            branch,
            onto: onto.clone(),
            onto_short,
            plan: Vec::new(),
            phase: RebasePhase::Planning,
            selected_row: 0,
            action_menu_open: None,
            op_in_flight: true,
            already_on_upstream: Vec::new(),
            paused_agents: Vec::new(),
            dragging_row: None,
            drag_insertion: None,
            _task: None,
        });
        cx.notify();

        let root = worktree_root;
        let task = cx.spawn(async move |this, cx| {
            let root_for_bg = root.clone();
            let onto_for_bg = onto.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let commits = wt_core::rebase::commits_to_rebase(&root_for_bg, &onto_for_bg)?;
                    let already_on_upstream =
                        wt_core::graph::commits_already_on_upstream(&root_for_bg, &commits)
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                    let files: Vec<(String, Option<usize>)> = commits
                        .iter()
                        .map(|id| {
                            let count = wt_core::graph::commit_changed_files(&root_for_bg, id)
                                .ok()
                                .map(|files| files.len());
                            (id.clone(), count)
                        })
                        .collect();
                    Ok::<_, wt_core::Error>((commits, already_on_upstream, files))
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if this.graph_state.rebase.is_none() {
                    // The mode was left (Cancel, a worktree/repo switch, the tab was closed)
                    // while this real background load was still running - nothing left to
                    // populate.
                    return;
                }
                match result {
                    Ok((commits, already_on_upstream, files)) => {
                        let subjects = this.rebase_commit_subjects(&commits);
                        let Some(rebase_state) = this.graph_state.rebase.as_mut() else {
                            // Re-checked rather than assumed: `rebase_commit_subjects` above
                            // takes no `&mut self`, but nothing guarantees the mode is still
                            // open by the time this line runs on a future edit - never a real
                            // panic in production code for a state that can genuinely change
                            // out from under an in-flight task.
                            return;
                        };
                        rebase_state.plan = commits
                            .iter()
                            .zip(files)
                            .map(|(id, (_, files_changed))| {
                                let (short_sha, subject) = subjects
                                    .get(id)
                                    .cloned()
                                    .unwrap_or_else(|| (id.chars().take(7).collect(), id.clone()));
                                RebasePlanRow {
                                    commit: id.clone(),
                                    short_sha,
                                    original_subject: subject.clone(),
                                    files_changed,
                                    action: RebaseActionKind::Pick,
                                    reword_message: TextField::seeded(&subject),
                                    reword_focus_handle: cx.focus_handle(),
                                }
                            })
                            .collect();
                        rebase_state.already_on_upstream = already_on_upstream;
                        rebase_state.op_in_flight = false;
                    }
                    Err(err) => {
                        this.graph_state.status_message =
                            Some(format!("Interactive rebase failed to load: {err}"));
                        // GitHub issue #242 phase B fix: a real "Pause now" click during this
                        // load must still be resumed for real if the load itself then fails -
                        // an independent review found this path used to drop `graph_state.rebase`
                        // directly, silently leaking any agent this session had paused.
                        this.leave_rebase_mode(cx);
                    }
                }
                cx.notify();
            });
        });
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            rebase_state._task = Some(task);
        }
    }

    /// Looks up each of `commits`' real `(short_sha, subject)` from the currently loaded graph -
    /// every commit `wt_core::rebase::commits_to_rebase` can return for a row opened from this
    /// pane is, by construction, already loaded (it's an ancestor of the clicked row, which was
    /// itself already on screen). Never spawns further real I/O itself; a ancestor genuinely not
    /// found (an edge case - e.g. the graph was reloaded out from under an in-flight plan build)
    /// is left for the caller to fall back on the raw id, rather than fabricated here.
    fn rebase_commit_subjects(&self, commits: &[String]) -> HashMap<String, (String, String)> {
        let mut out = HashMap::new();
        if let GraphLoadState::Loaded(graph) = &self.graph_state.load {
            for row in &graph.rows {
                if commits.contains(&row.commit.id) {
                    out.insert(
                        row.commit.id.clone(),
                        (row.commit.short_id.clone(), row.commit.subject.clone()),
                    );
                }
            }
        }
        out
    }

    /// The Planning-phase banner's `Cancel` (design spec §1.2) - discards the in-progress plan
    /// and returns to the normal commit list, resuming any agent this session's own "Pause now"
    /// suspended. Guarded on [`RebaseModeState::op_in_flight`] exactly like every other banner
    /// button now (see `crate::graph_view::rebase_render`'s own docs on
    /// `render_rebase_banner_actions`) - GitHub issue #242 phase B fix: an independent review
    /// found Cancel had no guard at all, so clicking it while `Start rebase`'s real subprocess
    /// was still running dropped `graph_state.rebase` out from under it, leaving the repository
    /// genuinely mid-rebase with no banner left to recover it from. Once `Start`/`Continue`/
    /// `Skip`/`Abort` are all disabled during their own real operation, this button is too, so
    /// there is never a window where the only real recovery surface can be discarded mid-flight.
    pub(crate) fn cancel_rebase_mode(&mut self, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        if rebase_state.op_in_flight {
            return;
        }
        self.leave_rebase_mode(cx);
        cx.notify();
    }

    /// The single, real exit path every place that leaves rebase mode funnels through - resumes
    /// any paused agents first (real `SIGCONT`, [`Self::resume_paused_rebase_agents`]), then
    /// clears the mode. GitHub issue #242 phase B fix: an independent review found several real
    /// exit paths (`close_git_graph_tab`, `reset_repo_scoped_state`'s worktree/repo switch, the
    /// plan-load error arm) used to clear `graph_state.rebase` directly, silently leaking any
    /// agent this session had paused - a permanent, silent `SIGSTOP` with no surface left to
    /// resume it from. Every real exit now goes through this one function instead.
    pub(crate) fn leave_rebase_mode(&mut self, cx: &mut Context<Self>) {
        self.resume_paused_rebase_agents(cx);
        self.graph_state.rebase = None;
    }

    /// Real `SIGCONT` (`crate::work_surface::agents::Agents::resume_agents`) against every agent
    /// [`RebaseModeState::paused_agents`] this session's own "Pause now" suspended -
    /// design spec §1.6 warning 1's "Resume after" contract: automatic the moment the mode
    /// reaches a terminal state. Called from every real exit path via [`Self::leave_rebase_mode`].
    ///
    /// Known, accepted gap (documented rather than silently unhandled - GitHub issue #242 phase B
    /// review): a real app crash or `SIGKILL` with no destructor run leaves a paused process
    /// exactly as stopped as `pause` left it, with no next-launch recovery in this revision - a
    /// `SIGSTOP`'d process cannot even react to the pty's own master-close `SIGHUP` while
    /// stopped. Recovering from that would mean persisting paused pids to disk and `SIGCONT`ing
    /// them on the next real startup; not implemented here.
    fn resume_paused_rebase_agents(&mut self, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_mut() else {
            return;
        };
        let ids = std::mem::take(&mut rebase_state.paused_agents);
        if !ids.is_empty() {
            self.agents.resume_agents(&ids, cx);
        }
    }

    /// Real defense in depth for the same bug [`RebaseModeState::worktree_root`]'s own docs
    /// describe: every rebase-mutating op reads the worktree this mode was really entered
    /// against from there, never `self.diff_root` fresh at click time. [`Self::
    /// reset_repo_scoped_state`]/[`Self::close_git_graph_tab`] are the real primary defense (they
    /// leave rebase mode outright via [`Self::leave_rebase_mode`] on any worktree/repo switch);
    /// this is the backstop in case that primary defense somehow doesn't catch a given path -
    /// refuses (`None`) rather than silently running a real git mutation against whatever
    /// `self.diff_root` now happens to be if the two have still drifted apart.
    fn rebase_worktree_root(&self) -> Option<PathBuf> {
        let rebase_state = self.graph_state.rebase.as_ref()?;
        if rebase_state.worktree_root != self.diff_root {
            log::warn!(
                "refusing a rebase-mode git operation: worktree_root {:?} no longer matches the \
                 currently focused diff_root {:?} - the rebase-mode-exit guards should have \
                 already left the mode on this switch",
                rebase_state.worktree_root,
                self.diff_root
            );
            return None;
        }
        Some(rebase_state.worktree_root.clone())
    }

    /// Design spec §1.6 warning 1's `Pause now` - real `SIGSTOP` against every real agent session
    /// currently open in this mode's own real worktree (`crate::work_surface::agents::Agents::
    /// pause_agents_for_cwd`, [`Self::rebase_worktree_root`] - never `self.diff_root` fresh),
    /// recording exactly which ones this call really paused so [`Self::
    /// resume_paused_rebase_agents`] resumes precisely that set later, never an agent some other
    /// mechanism paused.
    pub(crate) fn pause_rebase_agents(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.rebase_worktree_root() else {
            return;
        };
        let paused = self.agents.pause_agents_for_cwd(&cwd, cx);
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            for id in paused {
                if !rebase_state.paused_agents.contains(&id) {
                    rebase_state.paused_agents.push(id);
                }
            }
        }
        cx.notify();
    }

    /// The action-chip dropdown's row click (design spec §1.4) - changes `row_index`'s action for
    /// real, live-recomputing every derived value (`N -> M commits`, the Result panel, the
    /// pause-indicator column) the very next render, since none of those are cached.
    pub(crate) fn set_rebase_row_action(
        &mut self,
        row_index: usize,
        action: RebaseActionKind,
        cx: &mut Context<Self>,
    ) {
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            if let Some(row) = rebase_state.plan.get_mut(row_index) {
                row.action = action;
            }
            rebase_state.action_menu_open = None;
        }
        cx.notify();
    }

    /// Design spec §1.4's row selection - a real click anywhere on a plan row that isn't the
    /// action chip or the reword field (both of which `cx.stop_propagation()`).
    pub(crate) fn select_rebase_plan_row(&mut self, row_index: usize, cx: &mut Context<Self>) {
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            if row_index < rebase_state.plan.len() && rebase_state.selected_row != row_index {
                rebase_state.selected_row = row_index;
                cx.notify();
            }
        }
    }

    /// Design spec §1.4's footer hint `P pick · S squash · D drop`: set the **selected** row's
    /// action from the keyboard. Deliberately routed through [`Self::set_rebase_row_action`]
    /// rather than reaching into the plan itself, so a keyboard action and a real menu click can
    /// never diverge (the menu-closing, the `cx.notify()`, and every derived recount are one code
    /// path).
    pub(crate) fn set_selected_rebase_row_action(
        &mut self,
        action: RebaseActionKind,
        cx: &mut Context<Self>,
    ) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        // Never while a real git call is in flight, for the same reason every banner button is
        // disabled then (see `Self::render_rebase_banner_actions`'s own docs): the plan being
        // edited under a running rebase is exactly the double-click bug class that guard exists
        // for, and a keystroke is no less real a mutation than a click.
        if rebase_state.op_in_flight || !matches!(rebase_state.phase, RebasePhase::Planning) {
            return;
        }
        let Some(index) = rebase_state.selected_index() else {
            return;
        };
        self.set_rebase_row_action(index, action, cx);
    }

    /// Design spec §1.4's footer hint `alt+↑↓ reorder`: the keyboard counterpart to the drag
    /// handle. Reuses [`move_rebase_plan_row`] (identity-matched, not index-matched) rather than
    /// swapping in place, so the drag path and the keyboard path really are the same reorder.
    /// The selection follows the row it moved - otherwise a held `alt+↑` would walk the selection
    /// down the plan while shuffling a different row each time.
    pub(crate) fn move_selected_rebase_plan_row(&mut self, up: bool, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_mut() else {
            return;
        };
        if rebase_state.op_in_flight || !matches!(rebase_state.phase, RebasePhase::Planning) {
            return;
        }
        let Some(index) = rebase_state.selected_index() else {
            return;
        };
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|next| *next < rebase_state.plan.len())
        };
        let Some(target) = target else {
            return;
        };
        let moved = rebase_state.plan[index].commit.clone();
        let neighbour = rebase_state.plan[target].commit.clone();
        move_rebase_plan_row(&mut rebase_state.plan, &moved, &neighbour, !up);
        rebase_state.selected_row = target;
        rebase_state.action_menu_open = None;
        cx.notify();
    }

    pub(crate) fn toggle_rebase_action_menu(&mut self, row_index: usize, cx: &mut Context<Self>) {
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            rebase_state.action_menu_open = if rebase_state.action_menu_open == Some(row_index) {
                None
            } else {
                Some(row_index)
            };
        }
        cx.notify();
    }

    pub(crate) fn close_rebase_action_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            if rebase_state.action_menu_open.take().is_some() {
                cx.notify();
            }
        }
    }

    /// The reword field's own key handler - append/backspace only, mirroring
    /// `Self::handle_branches_filter_key_down`'s established shape (see `crate::root::new_file`'s
    /// module docs for why this codebase's inline text fields are hand-rolled rather than a real
    /// `EntityInputHandler`).
    pub(in crate::graph_view) fn handle_rebase_reword_key_down(
        &mut self,
        row_index: usize,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) =
            crate::root::widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        self.reset_caret_blink(cx);
        let Some(rebase_state) = self.graph_state.rebase.as_mut() else {
            return;
        };
        let Some(row) = rebase_state.plan.get_mut(row_index) else {
            return;
        };
        let changed = row.reword_message.handle_editing_key(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            modifiers,
            Instant::now(),
        );
        if changed {
            cx.notify();
            cx.stop_propagation();
        }
    }

    // ------------------------------------------------------------ drag-to-reorder (pre-Start)

    pub(in crate::graph_view) fn start_dragging_rebase_row(
        &mut self,
        commit: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            rebase_state.dragging_row = Some(commit);
        }
        cx.notify();
    }

    pub(in crate::graph_view) fn update_rebase_row_drag_insertion(
        &mut self,
        hovered_commit: &str,
        insert_after: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(rebase_state) = self.graph_state.rebase.as_mut() {
            if rebase_state.dragging_row.as_deref() == Some(hovered_commit) {
                return;
            }
            let wanted = (hovered_commit.to_string(), insert_after);
            if rebase_state.drag_insertion.as_ref() != Some(&wanted) {
                rebase_state.drag_insertion = Some(wanted);
                cx.notify();
            }
        }
    }

    /// The real reorder: moves the dragged row to sit immediately before/after `target_commit`'s
    /// row (whichever side `Self::update_rebase_row_drag_insertion` last recorded), entirely in
    /// memory - this never touches real git until `Start rebase` is clicked (design spec §1.4).
    pub(in crate::graph_view) fn drop_dragged_rebase_row(
        &mut self,
        dragged_commit: String,
        target_commit: String,
        cx: &mut Context<Self>,
    ) {
        let Some(rebase_state) = self.graph_state.rebase.as_mut() else {
            return;
        };
        let insert_after = rebase_state
            .drag_insertion
            .as_ref()
            .is_some_and(|(hovered, after)| hovered == &target_commit && *after);
        rebase_state.dragging_row = None;
        rebase_state.drag_insertion = None;
        move_rebase_plan_row(
            &mut rebase_state.plan,
            &dragged_commit,
            &target_commit,
            insert_after,
        );
        cx.notify();
    }

    pub(crate) fn cancel_rebase_row_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(rebase_state) = self.graph_state.rebase.as_mut() else {
            return false;
        };
        let cleared_insertion = rebase_state.drag_insertion.take().is_some();
        let cleared_dragging = rebase_state.dragging_row.take().is_some();
        if cleared_insertion || cleared_dragging {
            cx.notify();
        }
        cleared_insertion || cleared_dragging
    }

    // ------------------------------------------------------------ real wt_core::rebase driving

    /// The Planning-phase banner's `Start rebase` (design spec §1.2) - builds the real
    /// `RebasePlanEntry` plan from the in-memory rows and calls
    /// `wt_core::rebase::start_interactive_rebase` for real, applying whatever real
    /// `RebaseOutcome` comes back.
    pub(crate) fn start_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        if rebase_state.op_in_flight {
            return;
        }
        let Some(root) = self.rebase_worktree_root() else {
            return;
        };
        let rebase_state = self.graph_state.rebase.as_ref().expect("checked above");
        let entries: Vec<RebasePlanEntry> = rebase_state
            .plan
            .iter()
            .map(RebasePlanRow::to_plan_entry)
            .collect();
        let onto = rebase_state.onto.clone();
        self.run_rebase_op(cx, move || {
            wt_core::rebase::start_interactive_rebase(&root, &onto, &entries)
        });
    }

    /// The Stopped-phase banner's `Continue` (design spec §1.2). Guarded on
    /// [`RebaseModeState::op_in_flight`] - GitHub issue #242 phase B fix: an independent review
    /// reproduced a real double-click bug here. `run_rebase_op` itself now refuses a second
    /// overlapping call (see that method's own docs), but the *message* this function reads for
    /// the amend below is captured from `rebase_state.phase` *before* that guard would matter -
    /// without this function's own early check, two rapid clicks could each capture the message
    /// visible at that instant and both proceed to spawn their own real `amend_head_message` +
    /// `continue_rebase` pair, with the second one racing to amend whatever commit the *first*
    /// call's own `continue_rebase` had, by then, already advanced `HEAD` to.
    pub(crate) fn continue_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        if rebase_state.op_in_flight {
            return;
        }
        let Some(root) = self.rebase_worktree_root() else {
            return;
        };
        let rebase_state = self.graph_state.rebase.as_ref().expect("checked above");
        // Real, load-bearing gap `wt_core::rebase`'s own module docs call out: the reword-message
        // queue its `GIT_EDITOR` script reads from is fixed at `start_interactive_rebase` time -
        // a message obtained only *after* a message-less-reword stop can never be picked up by
        // that queue retroactively. The real fix, matching this module's own test precedent
        // (`reword_with_no_message_stops_and_reports_the_right_commit_and_reason`), is a real
        // `git commit --amend` against the stopped commit before `git rebase --continue` runs -
        // exactly what a command-line user would do by hand. `amend_head_message` is handed the
        // real stopped commit id as `expected_head_original` - a real, on-disk identity check
        // (`git rev-parse HEAD` must still match it) that refuses rather than amending whatever
        // commit `HEAD` happens to be by the time this real background call actually runs.
        let amend = if let RebasePhase::Stopped {
            outcome:
                RebaseOutcome::StoppedForEdit {
                    commit,
                    reason: Some(wt_core::rebase::StopReason::RewordNeedsMessage),
                },
        } = &rebase_state.phase
        {
            rebase_state
                .plan
                .iter()
                .find(|row| &row.commit == commit)
                .filter(|row| row.has_supplied_reword_message())
                .map(|row| (commit.clone(), row.reword_message.as_str().to_string()))
        } else {
            None
        };
        self.run_rebase_op(cx, move || {
            if let Some((expected_head, message)) = amend {
                wt_core::rebase::amend_head_message(&root, &expected_head, &message)?;
            }
            wt_core::rebase::continue_rebase(&root)
        });
    }

    /// The Stopped-phase banner's `Skip` (design spec §1.2). Guarded on [`RebaseModeState::
    /// op_in_flight`] the same as [`Self::continue_rebase`] - see that method's own docs for the
    /// real double-click bug this closes.
    pub(crate) fn skip_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        if rebase_state.op_in_flight {
            return;
        }
        let Some(root) = self.rebase_worktree_root() else {
            return;
        };
        self.run_rebase_op(cx, move || wt_core::rebase::skip_rebase_commit(&root));
    }

    /// The Stopped-phase banner's `Abort` (design spec §1.2) - real `wt_core::rebase::
    /// abort_rebase`, returning to the normal commit list exactly like [`Self::cancel_rebase_mode`]
    /// (real agent resume included, via [`Self::leave_rebase_mode`]), reloading the graph since
    /// `abort_rebase` really moves `HEAD` back.
    pub(crate) fn abort_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        if rebase_state.op_in_flight {
            return;
        }
        let Some(root) = self.rebase_worktree_root() else {
            return;
        };
        if let Some(rs) = self.graph_state.rebase.as_mut() {
            rs.op_in_flight = true;
        }
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::rebase::abort_rebase(&root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.leave_rebase_mode(cx);
                        this.load_graph(cx);
                    }
                    Err(err) => {
                        if let Some(rs) = this.graph_state.rebase.as_mut() {
                            rs.op_in_flight = false;
                        }
                        this.graph_state.status_message = Some(format!("Abort failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        if let Some(rs) = self.graph_state.rebase.as_mut() {
            rs._task = Some(task);
        }
    }

    /// Shared real-git-driving plumbing for [`Self::start_rebase`]/[`Self::continue_rebase`]/
    /// [`Self::skip_rebase`] - each hands this a closure that performs exactly one real
    /// `wt_core::rebase` call on the background executor, and this applies whatever real
    /// `RebaseOutcome` (or error) comes back via [`Self::apply_rebase_outcome`]. Mirrors
    /// `Self::run_graph_remote_op`'s own background-spawn/repaint shape (see that method's docs),
    /// but returns a real `RebaseOutcome` rather than `Result<(), Error>`, so it's a distinct
    /// function rather than a reuse of that one.
    ///
    /// Refuses (a no-op) if a rebase operation is already in flight - the shared half of the
    /// real double-click guard every caller also checks itself before doing any real work (see
    /// [`Self::continue_rebase`]'s own docs for why the caller-side check still matters too).
    fn run_rebase_op(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce() -> Result<RebaseOutcome, wt_core::Error> + Send + 'static,
    ) {
        let Some(rs) = self.graph_state.rebase.as_mut() else {
            return;
        };
        if rs.op_in_flight {
            return;
        }
        rs.op_in_flight = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                this.apply_rebase_outcome(result, cx);
            });
        });
        if let Some(rs) = self.graph_state.rebase.as_mut() {
            rs._task = Some(task);
        }
    }

    /// Applies a real `Result<RebaseOutcome, Error>` from `Self::run_rebase_op` - a real
    /// `Completed` leaves rebase mode entirely (via [`Self::leave_rebase_mode`], real agent
    /// resume included) and reloads the graph (the freshly rewritten history); a real stop
    /// transitions to [`RebasePhase::Stopped`]; a genuine error is surfaced as a status message
    /// with the mode left exactly as it was (never silently discarded - the user can retry
    /// `Continue`/`Skip`/`Abort`).
    fn apply_rebase_outcome(
        &mut self,
        result: Result<RebaseOutcome, wt_core::Error>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(RebaseOutcome::Completed) => {
                self.leave_rebase_mode(cx);
                self.load_graph(cx);
            }
            Ok(outcome) => {
                if let Some(rs) = self.graph_state.rebase.as_mut() {
                    rs.op_in_flight = false;
                    rs.phase = RebasePhase::Stopped { outcome };
                }
                cx.notify();
            }
            Err(err) => {
                if let Some(rs) = self.graph_state.rebase.as_mut() {
                    rs.op_in_flight = false;
                }
                self.graph_state.status_message = Some(format!("Interactive rebase failed: {err}"));
                cx.notify();
            }
        }
    }

    /// Design spec §1.7's `Resolve in the diff view` link - routes the user to this app's
    /// existing conflict-resolution surface for the first real conflicted file
    /// (`RebaseOutcome::StoppedForConflict::conflicted_files`), exactly the same real
    /// `open_file_view` navigation `crate::graph_view::render`'s own row-menu test coverage
    /// already exercises for a conflicted path. This app has no dedicated conflict-marker editor
    /// beyond opening the real file in the File view (see this revision's own report for why -
    /// no existing surface does per-file conflict routing from a graph-triggered conflict today),
    /// so that is exactly what this does: the file, real conflict markers and all, in the same
    /// editor every other file opens in.
    pub(crate) fn resolve_rebase_conflict_in_diff_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return;
        };
        let RebasePhase::Stopped {
            outcome:
                RebaseOutcome::StoppedForConflict {
                    conflicted_files, ..
                },
        } = &rebase_state.phase
        else {
            return;
        };
        let Some(first) = conflicted_files.first() else {
            return;
        };
        let Some(root) = self.rebase_worktree_root() else {
            return;
        };
        let absolute = root.join(first);
        self.open_file_view(absolute, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_row(
        commit: &str,
        subject: &str,
        action: RebaseActionKind,
        cx: &gpui::TestAppContext,
    ) -> RebasePlanRow {
        let focus_handle = cx.update(|cx| cx.focus_handle());
        RebasePlanRow {
            commit: commit.to_string(),
            short_sha: commit.chars().take(7).collect(),
            original_subject: subject.to_string(),
            files_changed: Some(1),
            action,
            reword_message: TextField::seeded(subject),
            reword_focus_handle: focus_handle,
        }
    }

    fn set_reword(row: &mut RebasePlanRow, text: &str) {
        row.reword_message.set(text, std::time::Instant::now());
    }

    // --- RebasePlanRow::has_supplied_reword_message / is_planned_pause / to_plan_entry --------

    #[gpui::test]
    fn a_reword_row_with_the_untouched_prefilled_subject_has_no_supplied_message(
        cx: &mut gpui::TestAppContext,
    ) {
        let row = plan_row("c1", "original subject", RebaseActionKind::Reword, cx);
        assert!(!row.has_supplied_reword_message());
        assert!(row.is_planned_pause());
        assert_eq!(
            row.to_plan_entry(),
            RebasePlanEntry {
                commit: "c1".to_string(),
                action: RebaseAction::Reword(None),
            }
        );
    }

    #[gpui::test]
    fn a_reword_row_with_a_real_edit_has_a_supplied_message(cx: &mut gpui::TestAppContext) {
        let mut row = plan_row("c1", "original subject", RebaseActionKind::Reword, cx);
        set_reword(&mut row, "a real new message");
        assert!(row.has_supplied_reword_message());
        assert!(!row.is_planned_pause());
        assert_eq!(
            row.to_plan_entry(),
            RebasePlanEntry {
                commit: "c1".to_string(),
                action: RebaseAction::Reword(Some("a real new message".to_string())),
            }
        );
    }

    #[gpui::test]
    fn an_edit_row_is_always_a_planned_pause(cx: &mut gpui::TestAppContext) {
        let row = plan_row("c1", "subject", RebaseActionKind::Edit, cx);
        assert!(row.is_planned_pause());
        assert_eq!(
            row.to_plan_entry(),
            RebasePlanEntry {
                commit: "c1".to_string(),
                action: RebaseAction::Edit,
            }
        );
    }

    #[gpui::test]
    fn pick_squash_fixup_drop_are_never_planned_pauses(cx: &mut gpui::TestAppContext) {
        for action in [
            RebaseActionKind::Pick,
            RebaseActionKind::Squash,
            RebaseActionKind::Fixup,
            RebaseActionKind::Drop,
        ] {
            let row = plan_row("c1", "subject", action, cx);
            assert!(
                !row.is_planned_pause(),
                "{action:?} must never be a planned pause"
            );
        }
    }

    // --- derive_result_commit_count / derive_result_blocks ------------------------------------

    #[gpui::test]
    fn n_to_m_counts_drop_as_removed_and_squash_fixup_as_folded(cx: &mut gpui::TestAppContext) {
        let plan = vec![
            plan_row("c1", "one", RebaseActionKind::Pick, cx),
            plan_row("c2", "two", RebaseActionKind::Squash, cx),
            plan_row("c3", "three", RebaseActionKind::Fixup, cx),
            plan_row("c4", "four", RebaseActionKind::Drop, cx),
            plan_row("c5", "five", RebaseActionKind::Pick, cx),
        ];
        assert_eq!(plan.len(), 5);
        assert_eq!(derive_result_commit_count(&plan), 2);
    }

    #[gpui::test]
    fn result_blocks_fold_squash_and_fixup_into_the_previous_block_with_a_real_count(
        cx: &mut gpui::TestAppContext,
    ) {
        let plan = vec![
            plan_row("c1", "base commit", RebaseActionKind::Pick, cx),
            plan_row("c2", "folded one", RebaseActionKind::Squash, cx),
            plan_row("c3", "folded two", RebaseActionKind::Fixup, cx),
        ];
        let blocks = derive_result_blocks(&plan);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].subject, "base commit");
        assert_eq!(blocks[0].folded_count, 2);
        assert_eq!(blocks[0].status, ResultBlockStatus::Normal);
    }

    #[gpui::test]
    fn a_squash_row_with_no_preceding_block_folds_into_nothing_rather_than_fabricating_one(
        cx: &mut gpui::TestAppContext,
    ) {
        let plan = vec![plan_row("c1", "subject", RebaseActionKind::Squash, cx)];
        let blocks = derive_result_blocks(&plan);
        assert!(blocks.is_empty());
    }

    #[gpui::test]
    fn a_drop_row_never_appears_in_the_result_blocks(cx: &mut gpui::TestAppContext) {
        let plan = vec![
            plan_row("c1", "kept", RebaseActionKind::Pick, cx),
            plan_row("c2", "dropped", RebaseActionKind::Drop, cx),
        ];
        let blocks = derive_result_blocks(&plan);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].subject, "kept");
    }

    #[gpui::test]
    fn result_blocks_reflect_the_live_reworded_text_and_status(cx: &mut gpui::TestAppContext) {
        let mut reworded = plan_row("c1", "original", RebaseActionKind::Reword, cx);
        set_reword(&mut reworded, "brand new subject");
        let unreworded = plan_row("c2", "original two", RebaseActionKind::Reword, cx);
        let editing = plan_row("c3", "original three", RebaseActionKind::Edit, cx);

        let blocks = derive_result_blocks(&[reworded, unreworded, editing]);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].subject, "brand new subject");
        assert_eq!(blocks[0].status, ResultBlockStatus::Reworded);
        assert_eq!(blocks[1].subject, "original two");
        assert_eq!(blocks[1].status, ResultBlockStatus::StopsForMessage);
        assert_eq!(blocks[2].subject, "original three");
        assert_eq!(blocks[2].status, ResultBlockStatus::StopsToAmend);
    }

    // --- derive_stop_count ---------------------------------------------------------------------

    #[gpui::test]
    fn stop_count_is_zero_for_an_all_pick_plan(cx: &mut gpui::TestAppContext) {
        let plan = vec![
            plan_row("c1", "one", RebaseActionKind::Pick, cx),
            plan_row("c2", "two", RebaseActionKind::Pick, cx),
        ];
        assert_eq!(derive_stop_count(&plan), 0);
    }

    #[gpui::test]
    fn stop_count_counts_edit_rows_and_message_less_reword_rows_only(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut reworded = plan_row("c1", "original", RebaseActionKind::Reword, cx);
        set_reword(&mut reworded, "a real message");
        let plan = vec![
            plan_row("c2", "two", RebaseActionKind::Edit, cx),
            reworded,
            plan_row("c3", "three", RebaseActionKind::Reword, cx),
            plan_row("c4", "four", RebaseActionKind::Pick, cx),
        ];
        assert_eq!(
            derive_stop_count(&plan),
            2,
            "the edit row and the message-less reword row count; the message-supplied reword \
             and the plain pick do not"
        );
    }

    // --- move_rebase_plan_row ------------------------------------------------------------------

    #[gpui::test]
    fn move_rebase_plan_row_drops_before_the_target_by_default(cx: &mut gpui::TestAppContext) {
        let mut plan = vec![
            plan_row("c1", "one", RebaseActionKind::Pick, cx),
            plan_row("c2", "two", RebaseActionKind::Pick, cx),
            plan_row("c3", "three", RebaseActionKind::Pick, cx),
        ];
        move_rebase_plan_row(&mut plan, "c3", "c1", false);
        assert_eq!(
            plan.iter()
                .map(|row| row.commit.clone())
                .collect::<Vec<_>>(),
            vec!["c3", "c1", "c2"]
        );
    }

    #[gpui::test]
    fn move_rebase_plan_row_respects_insert_after(cx: &mut gpui::TestAppContext) {
        let mut plan = vec![
            plan_row("c1", "one", RebaseActionKind::Pick, cx),
            plan_row("c2", "two", RebaseActionKind::Pick, cx),
            plan_row("c3", "three", RebaseActionKind::Pick, cx),
        ];
        move_rebase_plan_row(&mut plan, "c1", "c2", true);
        assert_eq!(
            plan.iter()
                .map(|row| row.commit.clone())
                .collect::<Vec<_>>(),
            vec!["c2", "c1", "c3"]
        );
    }

    #[gpui::test]
    fn move_rebase_plan_row_dropping_a_row_on_its_own_slot_is_a_no_op(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut plan = vec![
            plan_row("c1", "one", RebaseActionKind::Pick, cx),
            plan_row("c2", "two", RebaseActionKind::Pick, cx),
        ];
        move_rebase_plan_row(&mut plan, "c1", "c1", false);
        assert_eq!(
            plan.iter()
                .map(|row| row.commit.clone())
                .collect::<Vec<_>>(),
            vec!["c1", "c2"]
        );
    }

    #[gpui::test]
    fn move_rebase_plan_row_with_an_unknown_id_is_a_harmless_no_op(cx: &mut gpui::TestAppContext) {
        let mut plan = vec![
            plan_row("c1", "one", RebaseActionKind::Pick, cx),
            plan_row("c2", "two", RebaseActionKind::Pick, cx),
        ];
        move_rebase_plan_row(&mut plan, "unknown", "c1", false);
        assert_eq!(
            plan.iter()
                .map(|row| row.commit.clone())
                .collect::<Vec<_>>(),
            vec!["c1", "c2"]
        );
    }

    // --- outcome_stopped_commit ------------------------------------------------------------------

    #[test]
    fn outcome_stopped_commit_reads_both_real_stop_variants_and_none_for_completed() {
        assert_eq!(
            outcome_stopped_commit(&RebaseOutcome::StoppedForEdit {
                commit: "c1".to_string(),
                reason: None,
            }),
            Some("c1")
        );
        assert_eq!(
            outcome_stopped_commit(&RebaseOutcome::StoppedForConflict {
                commit: "c2".to_string(),
                conflicted_files: Vec::new(),
            }),
            Some("c2")
        );
        assert_eq!(outcome_stopped_commit(&RebaseOutcome::Completed), None);
    }
}
