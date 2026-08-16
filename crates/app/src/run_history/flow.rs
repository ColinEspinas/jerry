//! The `impl AdeApp` data half of agent history (GitHub issue #227): what really happens when a
//! run ends, and the two background loads the History surface needs.

use super::*;

use crate::hooks::store::FinishedRun;

/// How many lines of a run's own pane are kept as its transcript - the read side of
/// [`crate::run_history::transcript_store::MAX_TRANSCRIPT_LINES`], asked for at capture time so a
/// pane holding 10 000 lines of scrollback never allocates all of them just to throw most away.
const CAPTURED_TRANSCRIPT_LINES: usize = crate::run_history::transcript_store::MAX_TRANSCRIPT_LINES;

impl AdeApp {
    /// Records that agent `id`'s run really ended, now (GitHub issue #227).
    pub(crate) fn finish_run_record(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let ProcessKind::Agent(kind) = agent.kind else {
            return;
        };
        let worktree = agent.cwd.clone();
        let key = crate::review::state::baseline_key(&worktree, kind, agent.spawned_at_unix);
        // The contract this module's own docs spell out: History shows runs Jerry really knew
        // about, so a run with no record stays absent rather than being invented here.
        if self.agent_status_state.get(&key).is_none() {
            return;
        }
        if self._run_finish_tasks.contains_key(&key) {
            return;
        }

        // Read out of the live pane while it is still alive. `read` rather than `update`: this
        // only observes the grid.
        let transcript = agent
            .pane
            .read(cx)
            .retained_text_lines(CAPTURED_TRANSCRIPT_LINES);
        // The run's own baseline, if it captured one - the tree its diffstat is measured against.
        let baseline = self
            .agent_reviews
            .get(&id)
            .map(|review| (review.baseline.tree_id.clone(), review.baseline.untracked));
        let status = self.agent_status(agent, cx);
        let ended_at_unix = unix_now();
        let transcript_dir = self.run_transcript_dir.clone();

        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let measure_worktree = worktree.clone();
            let measured = match baseline {
                Some((tree_id, untracked)) => {
                    cx.background_executor()
                        .spawn(async move {
                            wt_core::review::diff_against_tree(
                                &measure_worktree,
                                &tree_id,
                                untracked,
                                // The label only ever appears in `wt_core`'s own error text; this
                                // names the run so a failure says which one could not be measured.
                                "run history".to_owned(),
                            )
                            .ok()
                        })
                        .await
                }
                None => None,
            };
            let diffstat = measured.map(|diff| {
                let (insertions, deletions) = crate::rail::state::sum_diff_stat(&diff);
                (diff.files.len() as u32, insertions as u32, deletions as u32)
            });

            if let Some(dir) = transcript_dir {
                let key = key.clone();
                cx.background_executor()
                    .spawn(async move {
                        if let Err(err) =
                            crate::run_history::transcript_store::save(&dir, &key, &transcript)
                        {
                            log::warn!("could not store this run's transcript: {err}");
                        }
                    })
                    .await;
            }

            let _ = this.update(cx, |this, cx| {
                this._run_finish_tasks.remove(&key);
                let changed = this.agent_status_state.finish(
                    &key,
                    ended_at_unix,
                    FinishedRun {
                        status: Some(status),
                        files_changed: diffstat.map(|(files, _, _)| files),
                        insertions: diffstat.map(|(_, insertions, _)| insertions),
                        deletions: diffstat.map(|(_, _, deletions)| deletions),
                    },
                );
                if changed {
                    this.agent_status_owned.insert(key.clone());
                    this.persist_agent_statuses_for_history(cx);
                }
                // This run's drift is now a question with an answer, and its worktree's cached
                // counts predate it. Dropping the entry is what makes the next History render ask
                // again, rather than showing the new run with no band forever.
                this.run_drift.remove(&worktree);
                cx.notify();
            });
        });
        self._run_finish_tasks.insert(task_key, task);
    }

    /// Loads the real drift count for every worktree that has history and has not been answered
    /// yet (GitHub issue #227).
    pub(crate) fn load_run_drift(&mut self, cx: &mut Context<Self>) {
        if self.run_drift_in_flight {
            return;
        }
        let live_keys = self.live_agent_status_keys();
        let mut wanted: Vec<(PathBuf, Vec<(String, i64)>)> = Vec::new();
        for worktree in self.history_worktrees() {
            if self.run_drift.contains_key(&worktree.path) {
                continue;
            }
            let runs: Vec<(String, i64)> = crate::hooks::history::past_agents_for_worktree(
                &self.agent_status_state,
                &worktree.path,
                &live_keys,
            )
            .into_iter()
            .map(|run| {
                (
                    run.key.clone(),
                    crate::run_history::model::run_finished_at(&run),
                )
            })
            .collect();
            if !runs.is_empty() {
                wanted.push((worktree.path.clone(), runs));
            }
        }
        if wanted.is_empty() {
            return;
        }

        self.run_drift_in_flight = true;
        let task = cx.spawn(async move |this, cx| {
            let answers = cx
                .background_executor()
                .spawn(async move {
                    wanted
                        .into_iter()
                        .map(|(path, runs)| {
                            let moments: Vec<i64> = runs.iter().map(|(_, at)| *at).collect();
                            let counts = wt_core::run_drift::commits_since_each(&path, &moments);
                            let counts = match counts {
                                Ok(Some(counts)) => counts,
                                // A checkout with no commits, or a `git` that failed: this
                                // worktree's runs simply have no drift answer. They paint no band
                                // rather than a fabricated `at the tip` - see `AdeApp::run_drift`.
                                Ok(None) => Vec::new(),
                                Err(err) => {
                                    log::warn!("could not read {}'s drift: {err}", path.display());
                                    Vec::new()
                                }
                            };
                            let per_run: HashMap<String, usize> =
                                runs.into_iter().map(|(key, _)| key).zip(counts).collect();
                            (path, per_run)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.run_drift_in_flight = false;
                for (path, counts) in answers {
                    this.run_drift.insert(path, counts);
                }
                cx.notify();
            });
        });
        self._run_drift_task = Some(task);
    }

    /// Reads one run's stored transcript back off disk, once, when its tab is opened.
    pub(crate) fn load_run_transcript(&mut self, run_key: String, cx: &mut Context<Self>) {
        if self.run_transcripts.contains_key(&run_key)
            || self._run_transcript_load_tasks.contains_key(&run_key)
        {
            return;
        }
        let Some(dir) = self.run_transcript_dir.clone() else {
            // No real settings path means no transcript directory at all - which is a real answer
            // ("this run has none"), not a pending one.
            self.run_transcripts.insert(run_key, None);
            return;
        };
        let key = run_key.clone();
        let task = cx.spawn(async move |this, cx| {
            let read_key = key.clone();
            let lines = cx
                .background_executor()
                .spawn(async move { crate::run_history::transcript_store::load(&dir, &read_key) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this._run_transcript_load_tasks.remove(&key);
                this.run_transcripts.insert(key, lines);
                cx.notify();
            });
        });
        self._run_transcript_load_tasks.insert(run_key, task);
    }

    /// Every checkout the window knows about, in the rail's own repo → worktree order - the input
    /// [`crate::run_history::model::build_run_tree`] groups history under.
    pub(crate) fn history_worktrees(&self) -> Vec<crate::run_history::model::HistoryWorktree> {
        self.repos
            .iter()
            .flat_map(|repo| {
                let repo_label = repo.name.clone();
                repo.worktrees
                    .iter()
                    .map(move |item| crate::run_history::model::HistoryWorktree {
                        path: item.path.clone(),
                        repo_label: repo_label.clone(),
                        label: item.label.clone(),
                        branch: item.branch.clone(),
                    })
            })
            .collect()
    }

    /// Every genuinely past run in the window, most recently active first - the flat list
    /// [`crate::run_history::model::build_run_tree`] groups.
    pub(crate) fn past_runs(&self) -> Vec<crate::hooks::history::PastAgent> {
        let live_keys = self.live_agent_status_keys();
        crate::hooks::history::past_agents(&self.agent_status_state)
            .into_iter()
            .filter(|run| !live_keys.contains(&run.key))
            .collect()
    }

    /// Persists the record file after a run's ending was written, and prunes the transcript
    /// directory to match.
    fn persist_agent_statuses_for_history(&mut self, cx: &mut Context<Self>) {
        self.persist_agent_statuses(cx);
        let Some(dir) = self.run_transcript_dir.clone() else {
            return;
        };
        let live: std::collections::BTreeSet<String> =
            self.agent_status_state.agents.keys().cloned().collect();
        cx.background_executor()
            .spawn(async move {
                crate::run_history::transcript_store::prune(&dir, &live);
            })
            .detach();
    }
}
