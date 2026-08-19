//! Recording what the hooks taught Jerry about each agent, onto disk (GitHub issue #239 phase 2).

use gpui::{Context, Window};

use crate::root::AdeApp;
use crate::work_surface::agents::ProcessKind;

impl AdeApp {
    /// Every currently open agent's own persisted-status key
    /// (`crate::review::state::baseline_key`) - GitHub issue #227's "this one is still live, don't
    /// show it twice under History" exclusion set for
    /// [`crate::hooks::history::past_agents_for_worktree`].
    pub(crate) fn live_agent_status_keys(&self) -> std::collections::HashSet<String> {
        self.agents
            .iter()
            .filter_map(|agent| {
                let ProcessKind::Agent(kind) = agent.kind else {
                    return None;
                };
                Some(crate::review::state::baseline_key(
                    &agent.cwd,
                    kind,
                    agent.spawned_at_unix,
                ))
            })
            .collect()
    }

    /// GitHub issue #227's resume action: looks `key` up in
    /// [`Self::agent_status_state`], selects its worktree, and spawns a real agent back into it.
    pub(crate) fn resume_past_agent(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(past) = crate::hooks::history::find(&self.agent_status_state, key) else {
            return false;
        };
        if !self
            .worktrees
            .iter()
            .any(|item| item.path == past.worktree && item.error.is_none())
        {
            return false;
        }

        self.select_worktree_by_path(&past.worktree, window, cx);

        let process_kind = ProcessKind::Agent(past.kind);
        // GitHub issue #239 phase 2's injection, exactly as a fresh spawn would get it - a
        // resumed conversation is exactly as real an agent as a new one, and should keep
        // reporting its status through the same hook side-channel.
        let hook_injection = self.hook_injection_for(process_kind);
        let font_size = self.settings.appearance.terminal_font_size;
        let shell_override = self.settings.terminal.shell_override();
        let id = match (past.kind, past.session_id.clone()) {
            // Every kind with a verified resume flag, not Claude alone - `spawn_resume` asks
            // `AgentKind::resume_args` for the right spelling and spawns bare if there is none.
            (kind, Some(session_id)) if kind.resume_args(&session_id).is_some() => {
                self.agents.spawn_resume(
                    kind,
                    past.worktree.clone(),
                    font_size,
                    shell_override,
                    hook_injection.as_ref(),
                    session_id,
                    window,
                    cx,
                )
            }
            _ => self.agents.spawn(
                process_kind,
                past.worktree.clone(),
                font_size,
                shell_override,
                hook_injection.as_ref(),
                window,
                cx,
            ),
        };
        // Mirrors `crate::work_surface::render::AdeApp::new_agent`'s own post-spawn steps: a real
        // review baseline for the newly spawned pane, closing a now-invalid gated review tab, and
        // moving focus onto it.
        self.capture_review_baseline(id, cx);
        self.close_gated_review_tab(window, cx);
        self.focus_newly_spawned_agent(window, cx);
        // A resumed agent is a real new tab in this worktree, and one whose whole point is that it
        // carries a real session id forward - so it belongs in the persisted tab session too, and
        // will itself be resumable again after the next quit. See `crate::work_surface::session`.
        self.record_worktree_session(cx);
        self.prune_confirm_armed = false;
        cx.notify();
        true
    }

    /// The hook injection for an agent about to be spawned, bringing the listener up on first use.
    pub(crate) fn hook_injection_for(
        &mut self,
        kind: crate::work_surface::agents::ProcessKind,
    ) -> Option<crate::hooks::HookInjection> {
        use crate::work_surface::agents::{AgentKind, ProcessKind};
        if !matches!(kind, ProcessKind::Agent(AgentKind::Claude)) {
            return None;
        }
        // Bring-up is attempted exactly once per `AdeApp`. Keyed on a `tried` flag rather than on
        // `hook_runtime.is_none()`, because those differ precisely in the failure case: without
        // it, an instance that cannot start a runtime re-ran the whole attempt - a `bind`, a
        // directory sweep, a `mkdir`, two file writes - on the UI thread on *every* subsequent
        // Claude spawn, and re-logged the same warning each time, for a condition (no usable
        // loopback, an unwritable temp directory) that will not have changed since the last try.
        if !self.hook_runtime_tried {
            self.hook_runtime_tried = true;
            self.hook_runtime = crate::hooks::HookRuntime::start(&std::env::temp_dir());
        }
        self.hook_runtime
            .as_ref()
            .map(|runtime| runtime.injection())
    }

    /// Folds every live agent's current real state into the persisted record, and writes the file
    /// if anything changed.
    pub(crate) fn record_agent_statuses(&mut self, cx: &mut Context<Self>) {
        if self.agent_status_path.is_none() {
            return;
        }

        let now = unix_now();
        let mut changed = false;
        let mut touched: Vec<String> = Vec::new();

        // Only agents that have actually reported a hook are recorded, and that restriction is
        // the whole point rather than an optimisation. `crate::hooks::store`'s module docs say it
        // outright: a status derived from pty silence is a guess, and a guess written to disk is
        // still a guess an hour later - it would give GitHub issue #227 a history of things Jerry
        // never really knew. A Codex agent, a shell, and a Claude agent whose hooks have not
        // fired have nothing real to record, so nothing is recorded for them.
        //
        // It also matters for cost. This runs on the status poll, and a save is two `fsync`s held
        // under `crate::persisted_state_lock`'s *process-wide* mutex - the same one `repos.toml`,
        // `file-tree-state.toml` and `tab-order.toml`'s writers contend for. Recording every
        // agent meant writing on every ordinary quiescence transition
        // (`NoProcess` -> `Run` -> `Idle`), which measurably slowed the whole app down and, in
        // the test suite, destabilised unrelated timing-sensitive tests.
        let Some(runtime) = &self.hook_runtime else {
            return;
        };
        let recordable: Vec<(
            crate::work_surface::agents::AgentId,
            String,
            std::path::PathBuf,
            &'static str,
            i64,
        )> = self
            .agents
            .iter()
            .filter_map(|agent| {
                let crate::work_surface::agents::ProcessKind::Agent(kind) = agent.kind else {
                    return None;
                };
                // The gate: a real, *unexpired* hook fact, or this agent is not recorded at all.
                // `fresh()` rather than `.fact`, and the difference is the whole point - see
                // `crate::rail::status::HookSignal::fresh`'s own docs.
                runtime.signal_for(agent.id).fresh()?;
                let key =
                    crate::review::state::baseline_key(&agent.cwd, kind, agent.spawned_at_unix);
                Some((
                    agent.id,
                    key,
                    agent.cwd.clone(),
                    agent.kind.label(),
                    agent.spawned_at_unix,
                ))
            })
            .collect();
        if recordable.is_empty() {
            return;
        }

        struct RecordedEntry {
            key: String,
            cwd: std::path::PathBuf,
            kind_label: &'static str,
            spawned_at_unix: i64,
            status: crate::rail::status::Status,
            activity: Option<String>,
            question: Option<String>,
            session_id: Option<String>,
            run_facts: crate::hooks::server::RunFacts,
        }

        let entries: Vec<RecordedEntry> = recordable
            .into_iter()
            .filter_map(|(id, key, cwd, kind_label, spawned_at_unix)| {
                let agent = self.agents.iter().find(|agent| agent.id == id)?;
                let status = self.agent_status(agent, cx);
                let (activity, question) = match &self.hook_runtime {
                    Some(runtime) => runtime.text_for(id),
                    None => (None, None),
                };
                // GitHub issue #227: the real Claude Code session id this agent's hooks have
                // reported, if any - deliberately read without the `fresh()`/TTL gate
                // `text_for` applies (see `HookRuntime::session_id_for`'s own docs), since a
                // conversation stays exactly as resumable after its hooks go quiet as it was
                // the instant its last one fired.
                let session_id = self
                    .hook_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.session_id_for(id))
                    // A hookless kind carries its own id on the pane instead, minted before its
                    // spawn - see `crate::work_surface::agents::Agent::session_id`.
                    .or_else(|| self.agents.session_id_for(id).map(str::to_owned));
                // GitHub issue #227: the run's own title and completed-turn count, from the same
                // real hook stream and read ungated for the same reason as the session id - both
                // describe the run, not the present (`HookListener::run_facts_for`).
                let run_facts = self
                    .hook_runtime
                    .as_ref()
                    .map(|runtime| runtime.run_facts_for(id))
                    .unwrap_or_default();
                Some(RecordedEntry {
                    key,
                    cwd,
                    kind_label,
                    spawned_at_unix,
                    status,
                    activity,
                    question,
                    session_id,
                    run_facts,
                })
            })
            .collect();

        for entry in entries {
            if self.agent_status_state.set(
                entry.key.clone(),
                crate::hooks::store::LiveRun {
                    worktree: &entry.cwd,
                    kind: entry.kind_label,
                    spawned_at_unix: entry.spawned_at_unix,
                    status: entry.status,
                    activity: entry.activity,
                    question: entry.question,
                    session_id: entry.session_id,
                    title: entry.run_facts.title,
                    turns: entry.run_facts.turns,
                },
                now,
            ) {
                changed = true;
            }
            touched.push(entry.key);
        }

        // Ownership is cumulative across the session: an agent the user has since closed stays
        // owned, so its final recorded status keeps being written through on later saves instead
        // of being dropped the moment its pane goes away. That closed agent is precisely what
        // issue #227 most wants to show.
        for key in touched {
            self.agent_status_owned.insert(key);
        }

        if changed {
            self.persist_agent_statuses(cx);
            // A live agent's `session_id` is learned *here*, from its own hooks, some time after
            // its tab was created and recorded - so the tab session written when the tab opened
            // still says "no resumable session" for it. Re-recording on a real change is what
            // turns that into a genuinely resumable entry before the user ever quits; without it,
            // relaunching would honestly refuse to restore an agent that was, in fact, resumable
            // the whole time (`crate::work_surface::session::AdeApp::restore_worktree_session`).
            //
            // Cheap despite riding the status poll: the recorder compares against what is already
            // recorded and returns without touching the disk when nothing changed, which is every
            // tick after the first one that learns an id.
            self.record_worktree_session(cx);
        }
    }

    /// Writes [`crate::root::AdeApp::agent_status_state`] to disk on the background executor.
    pub(crate) fn persist_agent_statuses(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.agent_status_path.clone() else {
            return;
        };
        let state = self.agent_status_state.clone();
        let owned = self.agent_status_owned.clone();
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
        self._agent_status_persist_task = Some(task);
    }
}

/// Seconds since the Unix epoch, mirroring `crate::work_surface::agents::unix_now`.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
