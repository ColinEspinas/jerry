//! The `impl AdeApp` glue behind the agent review surface: capturing a baseline when an agent
//! spawns, loading a review off the UI thread, advancing the baseline on `Mark reviewed`, and
//! releasing a baseline's ref when its agent closes. See `super`'s module docs for scope.

use super::state::{baseline_key, AgentReview, BaselineReason, ReviewBaseline, ReviewLoadState};
use super::*;

/// The durable identity a captured baseline is filed under, bundled into one value so
/// [`AdeApp::record_review_baseline`] stays under clippy's argument limit - the same reason
/// `crate::work_surface::render::TabChromeArgs` and
/// `crate::code_surface::file_view::HoverRenderContext` exist.
struct BaselineIdentity {
    id: AgentId,
    /// `super::state::baseline_key`'s output for the three fields below.
    key: String,
    worktree: PathBuf,
    kind: AgentKind,
    spawned_at_unix: i64,
}

impl AdeApp {
    /// Whether agent `id`'s review surface should be shown at all right now - GitHub issue
    /// #225's single-agent gate, in the one place every caller reads it from.
    pub(crate) fn review_available_for(&self, id: AgentId) -> bool {
        self.agent_reviews.contains_key(&id) && self.agents.is_sole_agent_in_worktree(id)
    }

    /// Captures agent `id`'s review baseline: a real `wt_core::review::snapshot_worktree_tree` of
    /// its worktree, anchored under its own `refs/jerry/review/*` ref, recorded in memory and
    /// persisted.
    pub(crate) fn capture_review_baseline(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let ProcessKind::Agent(kind) = agent.kind else {
            return;
        };
        let worktree = agent.cwd.clone();
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
                        let snapshot = wt_core::review::snapshot_worktree_tree(&worktree)?;
                        // Anchor before reporting success: an unanchored tree is collectable, so
                        // a baseline that skipped this could silently stop resolving later.
                        wt_core::review::anchor_tree(
                            &worktree,
                            &snapshot_ref_name,
                            &snapshot.tree_id,
                        )?;
                        Ok::<wt_core::review::WorktreeSnapshot, wt_core::Error>(snapshot)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // This capture is finished either way; drop its own slot so the map only ever
                // holds genuinely in-flight work.
                this._review_baseline_tasks.remove(&id);
                match result {
                    Ok(snapshot) => {
                        let baseline = ReviewBaseline {
                            tree_id: snapshot.tree_id,
                            ref_name,
                            taken_at_unix: unix_now(),
                            reason: BaselineReason::Spawn,
                            untracked: snapshot.untracked,
                        };
                        this.record_review_baseline(
                            BaselineIdentity {
                                id,
                                key,
                                worktree: worktree.clone(),
                                kind,
                                spawned_at_unix,
                            },
                            baseline,
                            cx,
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
                }
            });
        });
        self._review_baseline_tasks.insert(id, task);
    }

    /// Stores a freshly captured baseline in memory and queues its persistence. Shared by
    /// [`Self::capture_review_baseline`] and [`Self::mark_reviewed`] so the in-memory and on-disk
    /// halves can never be updated by one and forgotten by the other.
    fn record_review_baseline(
        &mut self,
        identity: BaselineIdentity,
        baseline: ReviewBaseline,
        cx: &mut Context<Self>,
    ) {
        let BaselineIdentity {
            id,
            key,
            worktree,
            kind,
            spawned_at_unix,
        } = identity;
        self.review_baseline_state.set(
            key.clone(),
            &worktree,
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
        self.persist_review_baselines(cx);
    }

    /// Queues a background-executor save of [`Self::review_baseline_state`] - the exact shape
    /// `crate::work_surface::render::AdeApp::persist_tab_order` uses, including the merge against
    /// [`Self::review_baselines_owned`] so a second window can't erase baselines it never saw. A
    /// genuine no-op with a `None` path (every GPUI test that hasn't opted into a real one).
    fn persist_review_baselines(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.review_baseline_path.clone() else {
            return;
        };
        let state = self.review_baseline_state.clone();
        let owned = self.review_baselines_owned.clone();
        let task = cx.spawn(async move |_this, cx| {
            let save_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { state.save_merged_at(&save_path, &owned) })
                .await;
            if let Err(err) = result {
                log::warn!("failed to save {}: {err}", path.display());
            }
        });
        self._review_persist_task = Some(task);
    }

    /// Loads (or reloads) agent `id`'s review diff against its own baseline, off the UI thread.
    /// Mirrors `crate::code_surface::tabs::AdeApp::load_diff`'s shape exactly.
    pub(crate) fn load_agent_review(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(review) = self.agent_reviews.get_mut(&id) else {
            return;
        };
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree = agent.cwd.clone();
        let tree_id = review.baseline.tree_id.clone();
        // Must match the coverage the snapshot was taken with, or a tracked-only baseline would
        // report every pre-existing untracked file as a brand-new addition.
        let untracked = review.baseline.untracked;
        // The `WorktreeDiff::base_branch` slot carries a *label*, and a review has no base branch
        // at all - so it carries the honest "since ..." phrase instead of a branch name that
        // would be a lie. See `wt_core::review::diff_against_tree`'s own docs.
        let label = review.baseline.reason.since_phrase().to_string();
        review.load = ReviewLoadState::Loading;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    wt_core::review::diff_against_tree(&worktree, &tree_id, untracked, label)
                })
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
    pub(crate) fn mark_reviewed(&mut self, id: AgentId, cx: &mut Context<Self>) {
        if !self.review_available_for(id) || self.review_mark_in_flight.is_some() {
            return;
        }
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        // Unreachable in practice - `review_available_for`'s already-checked
        // `agent_reviews.contains_key` implies a baseline exists, which `capture_review_baseline`
        // only ever creates for a real agent session - but a destructure here rather than
        // trusting that invariant keeps this function correct even if that changes.
        let ProcessKind::Agent(kind) = agent.kind else {
            return;
        };
        let worktree = agent.cwd.clone();
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
                        let snapshot = wt_core::review::snapshot_worktree_tree(&worktree)?;
                        wt_core::review::anchor_tree(
                            &worktree,
                            &snapshot_ref_name,
                            &snapshot.tree_id,
                        )?;
                        Ok::<wt_core::review::WorktreeSnapshot, wt_core::Error>(snapshot)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.review_mark_in_flight = None;
                match result {
                    Ok(snapshot) => {
                        let baseline = ReviewBaseline {
                            tree_id: snapshot.tree_id,
                            ref_name,
                            taken_at_unix: unix_now(),
                            reason: BaselineReason::MarkedReviewed,
                            untracked: snapshot.untracked,
                        };
                        this.record_review_baseline(
                            BaselineIdentity {
                                id,
                                key,
                                worktree: worktree.clone(),
                                kind,
                                spawned_at_unix,
                            },
                            baseline,
                            cx,
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
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::review::delete_ref(&worktree, &ref_name) })
                .await;
            if let Err(err) = result {
                log::warn!("failed to release a review baseline ref: {err}");
            }
            let _ = this.update(cx, |this, _cx| {
                this._review_release_tasks.remove(&id);
            });
        });
        self._review_release_tasks.insert(id, task);
    }

    /// The rail's real per-agent `N files` count for a [`crate::rail::status::Status::Review`]
    /// row - how many files this agent has changed since *its own* baseline.
    pub(crate) fn agent_review_file_count(&self, id: AgentId) -> Option<usize> {
        if !self.review_available_for(id) {
            return None;
        }
        self.agent_reviews.get(&id)?.unreviewed_file_count()
    }

    /// Every agent that currently has a baseline, as `(id, worktree, tree id)` - what the rail's
    /// status-poll tick needs to measure each agent's unreviewed set on the background executor.
    pub(crate) fn review_measure_targets(
        &self,
    ) -> Vec<(AgentId, PathBuf, String, wt_core::review::UntrackedCoverage)> {
        self.agents
            .iter()
            .filter_map(|agent| {
                let review = self.agent_reviews.get(&agent.id)?;
                Some((
                    agent.id,
                    agent.cwd.clone(),
                    review.baseline.tree_id.clone(),
                    // Same coverage the baseline was captured with - see `load_agent_review`.
                    review.baseline.untracked,
                ))
            })
            .collect()
    }

    /// Writes back one status-poll tick's real measurements. An agent whose measurement failed
    /// (or that closed mid-tick) is skipped, leaving its previous answer in place rather than
    /// being reset to a fabricated empty set off the back of a git call that errored.
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
