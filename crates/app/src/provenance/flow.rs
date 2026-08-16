//! The `impl AdeApp` half of per-agent line provenance (GitHub issue #284) - where the pure store
//! meets the running app.

use std::collections::BTreeSet;
use std::path::Path;

use gpui::Context;

use super::change_set::{build_change_set, ChangeSet};
use super::persist_state::LineProvenanceState;
use super::store::RecordOutcome;
use super::AgentKey;
use crate::hooks::event::EditPhase;
use crate::root::AdeApp;
use crate::work_surface::agents::ProcessKind;

impl AdeApp {
    /// Applies every file write the hook layer has reported since the last tick.
    pub(crate) fn apply_agent_edits(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = &self.hook_runtime else {
            return;
        };
        let (edits, dropped) = runtime.drain_edits();
        if dropped > 0 {
            log::warn!(
                "{dropped} agent edit(s) were dropped before Jerry could attribute them - those \
                 lines will read as unattributed"
            );
        }
        if edits.is_empty() {
            return;
        }

        let mut changed = false;
        for edit in edits {
            // The worktree and the durable key are only knowable here: the hook payload carries
            // neither (see `crate::hooks::event::EditedFile`). An edit whose agent has already
            // closed is dropped rather than guessed at - there is no worktree to file it under.
            let Some((worktree, key)) = self.agents.iter().find_map(|agent| {
                if agent.id != edit.agent {
                    return None;
                }
                let ProcessKind::Agent(kind) = agent.kind else {
                    return None;
                };
                Some((
                    agent.cwd.clone(),
                    AgentKey::new(crate::review::state::baseline_key(
                        &agent.cwd,
                        kind,
                        agent.spawned_at_unix,
                    )),
                ))
            }) else {
                continue;
            };

            let path = super::absolute_edit_path(&edit.file);

            match edit.file.phase {
                EditPhase::Before => {
                    self.line_provenance
                        .begin_agent_edit_with(&worktree, &path, edit.before)
                }
                EditPhase::After => {
                    if self
                        .line_provenance
                        .record_agent_edit(&worktree, &path, &key)
                        != RecordOutcome::Unchanged
                    {
                        changed = true;
                        self.line_provenance_owned
                            .insert(crate::review::state::encode_worktree(&worktree));
                    }
                }
            }
        }

        if changed {
            self.rebuild_change_set();
            self.persist_line_provenance(cx);
            cx.notify();
        }
    }

    /// Records a hand edit: the human saved `content` over `relative` in the worktree rooted at
    /// `worktree`, so every line that changed is now [`super::Author::You`]'s.
    pub(crate) fn record_hand_edit(
        &mut self,
        worktree: &Path,
        relative: &Path,
        content: &str,
        cx: &mut Context<Self>,
    ) {
        // Deliberately not gated on `line_provenance_path`: attribution is live in-memory state
        // that `Self::current_change_set` reads whether or not there is anywhere to persist it,
        // and `Self::apply_agent_edits` is ungated for the same reason. `persist_line_provenance`
        // is where the `None` path really stops, which is the one place it costs anything.
        if self
            .line_provenance
            .record_hand_edit_content(worktree, relative, content)
            == RecordOutcome::Unchanged
        {
            return;
        }
        self.line_provenance_owned
            .insert(crate::review::state::encode_worktree(worktree));
        self.rebuild_change_set();
        self.persist_line_provenance(cx);
    }

    /// The current worktree's change set: one row per changed path, each carrying the
    /// de-duplicated author union and the per-author `split`.
    pub(crate) fn current_change_set(&self) -> ChangeSet {
        let Some(diff) = self.current_diff() else {
            return ChangeSet::default();
        };
        build_change_set(diff, self.line_provenance.worktree(&self.diff_root))
    }

    /// The current worktree's **uncommitted** change set - the same join as
    /// [`Self::current_change_set`], over `wt_core::diff::diff_against_head`'s scope rather than
    /// the merge-base one (GitHub issue #285).
    pub(crate) fn current_uncommitted_change_set(&self) -> ChangeSet {
        let Some(diff) = self.uncommitted_diff.loaded() else {
            return ChangeSet::default();
        };
        build_change_set(diff, self.line_provenance.worktree(&self.diff_root))
    }

    /// Refreshes both of [`crate::root::AdeApp`]'s cached change sets - the single chokepoint, so
    /// they and the things they are derived from cannot drift, and so the panel's Runs and
    /// Uncommitted sections are always two views of one join rather than two joins.
    pub(crate) fn rebuild_change_set(&mut self) {
        self.change_set = self.current_change_set();
        self.uncommitted_change_set = self.current_uncommitted_change_set();
    }

    /// Reads back what a previous run recorded, dropping any record whose file no longer matches
    /// it - see [`super::persist_state`] for why a mismatch is discarded rather than reinterpreted.
    pub(crate) fn restore_line_provenance(&mut self) {
        let Some(path) = self.line_provenance_path.clone() else {
            return;
        };
        let state = LineProvenanceState::load_at(&path);
        if state.worktrees.is_empty() {
            return;
        }
        // Every key that was on disk is now this instance's to write back: the store has just
        // taken ownership of those records, so a later save must be able to remove one that has
        // legitimately gone away.
        self.line_provenance_owned
            .extend(state.worktrees.keys().cloned());
        let (restored, discarded) = state.restore_into(&mut self.line_provenance);
        if restored > 0 || discarded > 0 {
            log::info!(
                "line provenance: restored {restored} path(s), discarded {discarded} that no \
                 longer match the files they described"
            );
        }
    }

    /// Writes the store to disk on the background executor - same reasoning as
    /// `crate::hooks::flow::AdeApp::persist_agent_statuses`: `save_merged_at` holds the
    /// process-wide `crate::persisted_state_lock` across two `fsync`s, which has no business on
    /// the render thread.
    fn persist_line_provenance(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.line_provenance_path.clone() else {
            return;
        };
        let state = LineProvenanceState::capture(&self.line_provenance);
        let owned: BTreeSet<String> = self.line_provenance_owned.clone();
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
        self._line_provenance_persist_task = Some(task);
    }
}
