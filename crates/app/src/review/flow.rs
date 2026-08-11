//! The `impl AdeApp` glue behind the agent review surface: capturing a baseline when an agent
//! spawns, loading a review off the UI thread, advancing the baseline on `Mark reviewed`, and
//! releasing a baseline's ref when its agent closes. See `super`'s module docs for scope.
//!
//! ## Every git call here is real blocking I/O
//!
//! `wt_core::review::snapshot_worktree_tree`, `diff_against_tree`, `changed_paths_against_tree`,
//! `anchor_tree` and `delete_ref` all spawn real `git` child processes. Every one of them runs on
//! `cx.background_executor()` and mutates [`AdeApp`] state only afterwards, from inside
//! `this.update` - the exact shape `crate::code_surface::tabs::AdeApp::load_diff` and
//! `crate::graph_view::render::AdeApp::load_graph` already established.

use super::state::{baseline_key, AgentReview, BaselineReason, ReviewBaseline, ReviewLoadState};
use super::*;

impl AdeApp {
    /// Whether agent `id`'s review surface should be shown at all right now - GitHub issue
    /// #225's single-agent gate, in the one place every caller reads it from.
    ///
    /// Two real conditions, both required:
    /// 1. A baseline has actually been captured (the snapshot is a background task, so there is a
    ///    real window right after spawn where there is genuinely nothing to review *against*
    ///    yet - and a review surface with no baseline would have to invent one).
    /// 2. This agent is the only one open in its worktree
    ///    (`Agents::is_sole_agent_in_worktree` - see that method's docs for why).
    ///
    /// Read by the footer's `Review` action, the rail's review-ready status, the tab strip, and
    /// the Review tab's own open path, so none of them can drift from each other.
    pub(crate) fn review_available_for(&self, id: AgentId) -> bool {
        self.agent_reviews.contains_key(&id) && self.agents.is_sole_agent_in_worktree(id)
    }

    /// Captures agent `id`'s review baseline: a real `wt_core::review::snapshot_worktree_tree` of
    /// its worktree, anchored under its own `refs/jerry/review/*` ref, recorded in memory and
    /// persisted.
    ///
    /// Called from `crate::work_surface::render::AdeApp::new_agent` - the caller of
    /// `Agents::spawn`, not `Agents::spawn` itself, mirroring how `load_diff` is triggered by a
    /// caller rather than baked into a lower-level type. `Agents` has no business knowing about
    /// git snapshots.
    ///
    /// ## The accepted race
    ///
    /// The agent's process starts immediately; this snapshot lands a real moment later (it's a
    /// background task spawning `git`). Anything the agent writes inside that window is captured
    /// *into* the baseline and therefore won't appear as an unreviewed change. That is accepted
    /// for phase 1 rather than papered over - which is exactly why
    /// [`ReviewBaseline::taken_at_unix`] records when the snapshot really happened, so the tab
    /// header can say "09:31" honestly instead of implying it is the process's own start instant.
    ///
    /// Captured for **every** agent, including agents in worktrees that already have others open:
    /// the gate is a display-time decision ([`Self::review_available_for`]), so a worktree that
    /// later drops back to one agent finds a real baseline already waiting, taken at the right
    /// moment rather than retroactively invented.
    pub(crate) fn capture_review_baseline(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree = agent.cwd.clone();
        let kind = agent.kind;
        let spawned_at_unix = agent.spawned_at_unix;
        let key = baseline_key(&worktree, kind, spawned_at_unix);
        let ref_name = wt_core::review::baseline_ref_name(&key);

        let task = cx.spawn(async move |this, cx| {
            let snapshot_ref_name = ref_name.clone();
            let result = cx
                .background_executor()
                .spawn({
                    let worktree = worktree.clone();
                    async move {
                        let tree_id = wt_core::review::snapshot_worktree_tree(&worktree)?;
                        // Anchor before reporting success: an unanchored tree is collectable, so
                        // a baseline that skipped this could silently stop resolving later.
                        wt_core::review::anchor_tree(&worktree, &snapshot_ref_name, &tree_id)?;
                        Ok::<String, wt_core::Error>(tree_id)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(tree_id) => {
                    let baseline = ReviewBaseline {
                        tree_id,
                        ref_name,
                        taken_at_unix: unix_now(),
                        reason: BaselineReason::Spawn,
                    };
                    this.record_review_baseline(
                        id,
                        key,
                        &worktree,
                        kind,
                        spawned_at_unix,
                        baseline,
                    );
                    cx.notify();
                }
                Err(err) => {
                    // No baseline means no review surface for this agent at all
                    // ([`Self::review_available_for`]) - honest, and strictly better than
                    // inventing a base point. Everything else about the agent still works.
                    log::warn!(
                        "failed to capture a review baseline for {}: {err}",
                        worktree.display()
                    );
                }
            });
        });
        self._review_baseline_task = Some(task);
    }

    /// Stores a freshly captured baseline in memory and queues its persistence. Shared by
    /// [`Self::capture_review_baseline`] and [`Self::mark_reviewed`] so the in-memory and on-disk
    /// halves can never be updated by one and forgotten by the other.
    fn record_review_baseline(
        &mut self,
        id: AgentId,
        key: String,
        worktree: &Path,
        kind: AgentKind,
        spawned_at_unix: i64,
        baseline: ReviewBaseline,
    ) {
        self.review_baseline_state.set(
            key.clone(),
            worktree,
            kind.label(),
            spawned_at_unix,
            &baseline,
        );
        self.review_baselines_owned.insert(key);
        match self.agent_reviews.get_mut(&id) {
            Some(review) => review.advance_to(baseline),
            None => {
                self.agent_reviews.insert(id, AgentReview::new(baseline));
            }
        }
        self.persist_review_baselines();
    }

    /// Queues a background-executor save of [`Self::review_baseline_state`] - the exact shape
    /// `crate::work_surface::render::AdeApp::persist_tab_order` uses, including the merge against
    /// [`Self::review_baselines_owned`] so a second window can't erase baselines it never saw. A
    /// genuine no-op with a `None` path (every GPUI test that hasn't opted into a real one).
    fn persist_review_baselines(&mut self) {
        let Some(path) = self.review_baseline_path.clone() else {
            return;
        };
        let state = self.review_baseline_state.clone();
        let owned = self.review_baselines_owned.clone();
        // Deliberately `std::thread::spawn`-free and `cx`-free: this is called from inside an
        // existing `this.update` closure, where no `Context` is available to spawn a task from
        // without re-entering. A blocking write of a small TOML file is bounded work, and the
        // sibling persisted-state files' own save paths are the same size; the cost of getting a
        // background task's lifetime wrong here would be a dropped (silently unwritten) save.
        if let Err(err) = state.save_merged_at(&path, &owned) {
            log::warn!("failed to save {}: {err}", path.display());
        }
    }

    /// Loads (or reloads) agent `id`'s review diff against its own baseline, off the UI thread.
    /// Mirrors `crate::code_surface::tabs::AdeApp::load_diff`'s shape exactly.
    ///
    /// A no-op if `id` has no baseline yet - there is genuinely nothing to diff against, and
    /// falling back to any *other* base point would produce a number this surface would then
    /// present as "this agent's changes", which it would not be.
    pub(crate) fn load_agent_review(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(review) = self.agent_reviews.get_mut(&id) else {
            return;
        };
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree = agent.cwd.clone();
        let tree_id = review.baseline.tree_id.clone();
        // The `WorktreeDiff::base_branch` slot carries a *label*, and a review has no base branch
        // at all - so it carries the honest "since ..." phrase instead of a branch name that
        // would be a lie. See `wt_core::review::diff_against_tree`'s own docs.
        let label = review.baseline.reason.since_phrase().to_string();
        review.load = ReviewLoadState::Loading;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { wt_core::review::diff_against_tree(&worktree, &tree_id, label) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(review) = this.agent_reviews.get_mut(&id) else {
                    // The agent closed while this was loading - its review is already gone, and
                    // re-inserting one here would resurrect state for an agent that no longer
                    // exists.
                    return;
                };
                review.load = match result {
                    Ok(diff) => {
                        // The full load is itself a real measurement of the same set the status
                        // poll measures cheaply, so it refreshes `unreviewed_paths` too rather
                        // than leaving a possibly-staler poll answer next to a fresher diff. A
                        // failed load deliberately leaves the previous measurement alone (see
                        // `Self::apply_review_measurements` for the same rule).
                        review.unreviewed_paths =
                            Some(diff.files.iter().map(|file| file.path.clone()).collect());
                        ReviewLoadState::Loaded(diff)
                    }
                    Err(err) => ReviewLoadState::Error(err.to_string()),
                };
                // The previously open file may not be in the reloaded review any more.
                if let Some(open) = review.open_file.clone() {
                    let still_changed = review
                        .diff()
                        .is_some_and(|diff| diff.files.iter().any(|file| file.path == open));
                    if !still_changed {
                        review.open_file = None;
                    }
                }
                this.refresh_review_highlight_cache();
                cx.notify();
            });
        });
        self._review_load_task = Some(task);
    }

    /// The Review tab's `Mark reviewed` action: re-snapshots the worktree *right now*, advances
    /// this agent's baseline onto it, and reloads.
    ///
    /// After this, the review is empty (nothing has changed since a snapshot taken a moment
    /// ago), and that is the correct, good outcome, not an error (see
    /// `super::state::review_empty_message`). The reload is what makes the surface actually show
    /// it, rather than leaving the pre-mark file list on screen next to a baseline it no longer
    /// describes.
    ///
    /// Re-anchors the same ref onto the new tree (`anchor_tree` moves an existing ref), so a
    /// baseline never accumulates one ref per mark.
    pub(crate) fn mark_reviewed(&mut self, id: AgentId, cx: &mut Context<Self>) {
        if !self.review_available_for(id) || self.review_mark_in_flight.is_some() {
            return;
        }
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree = agent.cwd.clone();
        let kind = agent.kind;
        let spawned_at_unix = agent.spawned_at_unix;
        let key = baseline_key(&worktree, kind, spawned_at_unix);
        let ref_name = wt_core::review::baseline_ref_name(&key);
        self.review_mark_in_flight = Some(id);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let snapshot_ref_name = ref_name.clone();
            let result = cx
                .background_executor()
                .spawn({
                    let worktree = worktree.clone();
                    async move {
                        let tree_id = wt_core::review::snapshot_worktree_tree(&worktree)?;
                        wt_core::review::anchor_tree(&worktree, &snapshot_ref_name, &tree_id)?;
                        Ok::<String, wt_core::Error>(tree_id)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.review_mark_in_flight = None;
                match result {
                    Ok(tree_id) => {
                        let baseline = ReviewBaseline {
                            tree_id,
                            ref_name,
                            taken_at_unix: unix_now(),
                            reason: BaselineReason::MarkedReviewed,
                        };
                        this.record_review_baseline(
                            id,
                            key,
                            &worktree,
                            kind,
                            spawned_at_unix,
                            baseline,
                        );
                        this.refresh_review_highlight_cache();
                        this.load_agent_review(id, cx);
                    }
                    Err(err) => {
                        // The old baseline is deliberately left exactly where it was: a failed
                        // snapshot must never silently advance what the user has "reviewed".
                        if let Some(review) = this.agent_reviews.get_mut(&id) {
                            review.load =
                                ReviewLoadState::Error(format!("could not mark reviewed: {err}"));
                        }
                    }
                }
                cx.notify();
            });
        });
        self._review_mark_task = Some(task);
    }

    /// Drops agent `id`'s in-memory review and deletes its baseline ref, releasing the snapshot's
    /// objects to a future `git gc`. Called from `Self::close_agent`.
    ///
    /// The **persisted metadata entry is deliberately left in place** - see
    /// `super::baseline_state`'s module docs. The ref goes because a closed agent's snapshot has
    /// no live consumer and shouldn't pin objects forever; the record of what was captured, when,
    /// and why stays, because that is exactly the groundwork GitHub issue #227 ("Agent history and
    /// resume/recover") will need and this app should not be actively destroying.
    pub(crate) fn release_review_baseline(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(review) = self.agent_reviews.remove(&id) else {
            return;
        };
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            // Already removed from `Agents` - we no longer know which worktree to run
            // `update-ref -d` in. The ref stays; it costs one small file and one unreachable
            // tree. Callers run this *before* `Agents::close` precisely so this is the rare path.
            return;
        };
        let worktree = agent.cwd.clone();
        let ref_name = review.baseline.ref_name.clone();
        let task = cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::review::delete_ref(&worktree, &ref_name) })
                .await;
            if let Err(err) = result {
                log::warn!("failed to release a review baseline ref: {err}");
            }
        });
        self._review_release_task = Some(task);
    }

    /// The rail's real per-agent `N files` count for a [`crate::rail::status::Status::Review`]
    /// row - how many files this agent has changed since *its own* baseline.
    ///
    /// A pure read of an already-loaded review, never a fresh git call: this is consumed by
    /// `build_agent_rows`, which runs on every rail render. The loading itself happens in
    /// [`Self::load_agent_review`].
    ///
    /// `None` (rather than a fabricated `0`) whenever the number would be a guess: no baseline
    /// yet, nothing loaded yet, a failed load, or a multi-agent worktree the gate holds back.
    pub(crate) fn agent_review_file_count(&self, id: AgentId) -> Option<usize> {
        if !self.review_available_for(id) {
            return None;
        }
        self.agent_reviews.get(&id)?.unreviewed_file_count()
    }

    /// Every agent that currently has a baseline, as `(id, worktree, tree id)` - what the rail's
    /// status-poll tick needs to measure each agent's unreviewed set on the background executor.
    ///
    /// Deliberately **not** filtered by the single-agent gate: measuring is cheap and the answer
    /// is genuinely correct per-agent-baseline regardless of how many agents share a worktree.
    /// Only *presenting* it is gated (`Self::review_available_for`), so a worktree dropping back
    /// to one agent has a fresh measurement already in hand rather than waiting a tick for one.
    pub(crate) fn review_measure_targets(&self) -> Vec<(AgentId, PathBuf, String)> {
        self.agents
            .iter()
            .filter_map(|agent| {
                let review = self.agent_reviews.get(&agent.id)?;
                Some((agent.id, agent.cwd.clone(), review.baseline.tree_id.clone()))
            })
            .collect()
    }

    /// Writes back one status-poll tick's real measurements. An agent whose measurement failed
    /// (or that closed mid-tick) is skipped, leaving its previous answer in place rather than
    /// being reset to a fabricated empty set off the back of a git call that errored.
    ///
    /// Also drops a stale entry whose baseline moved while the tick was in flight (a concurrent
    /// `Mark reviewed`): that measurement describes the *old* baseline, and applying it would
    /// briefly show already-reviewed files as unreviewed.
    pub(crate) fn apply_review_measurements(
        &mut self,
        measurements: Vec<(AgentId, String, Vec<PathBuf>)>,
    ) {
        for (id, measured_tree_id, paths) in measurements {
            if let Some(review) = self.agent_reviews.get_mut(&id) {
                if review.baseline.tree_id == measured_tree_id {
                    review.unreviewed_paths = Some(paths);
                }
            }
        }
    }

    /// `true` when agent `id` has a real, loaded, non-empty review against its own baseline and
    /// the single-agent gate allows showing it - the replacement for the old
    /// "is the *worktree's* git diff non-empty" input to `crate::rail::status::derive_status`.
    ///
    /// This is the correctness fix at the heart of GitHub issue #225: an agent that changed
    /// nothing, in a worktree whose branch had already diverged from `main`, used to be reported
    /// `Review ready` off the back of the *branch's* diff.
    pub(crate) fn agent_has_unreviewed_changes(&self, id: AgentId) -> bool {
        self.review_available_for(id)
            && self
                .agent_reviews
                .get(&id)
                .is_some_and(|review| review.has_unreviewed_changes())
    }
}

/// Real wall-clock seconds since the Unix epoch, for a baseline's own `taken_at_unix`. Mirrors
/// `crate::work_surface::agents`' and `crate::graph_view::render`'s identical helpers.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
