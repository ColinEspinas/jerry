//! Recording what the hooks taught Jerry about each agent, onto disk (GitHub issue #239 phase 2).
//!
//! The `AdeApp`-side half of [`crate::hooks::store`]: it walks the live agents once per status
//! poll, folds each one's real derived status and hook text into
//! [`crate::root::AdeApp::agent_status_state`], and persists only when something genuinely
//! changed.
//!
//! Nothing renders any of this. It exists so GitHub issue #227 ("Agent history and
//! resume/recover") has real, structured, dated data to build on rather than having to invent a
//! capture mechanism first - see [`crate::hooks::store`]'s module docs.

use gpui::Context;

use crate::root::AdeApp;

impl AdeApp {
    /// The hook injection for an agent about to be spawned, bringing the listener up on first use.
    ///
    /// `None` for anything that isn't a Claude agent, and `None` if the runtime can't start - both
    /// of which simply mean "this agent reports no hooks", which is the pre-phase-2 behaviour.
    ///
    /// **Lazy rather than started at app startup**, which is a deliberate refinement of the
    /// original design. It is still exactly one listener per `AdeApp`, shared by every Claude
    /// agent that instance ever spawns - never one per agent. But a window that only ever holds
    /// shells, or a Codex agent, now never opens a socket or writes a file at all. That matters
    /// in two real places: a user who doesn't use Claude Code shouldn't have Jerry holding a
    /// loopback port open for the whole session, and the test suite (which builds a great many
    /// `AdeApp`s and spawns almost no Claude agents) no longer pays a listener thread and a temp
    /// directory per app - real added contention that was measurably destabilising timing
    /// sensitive tests elsewhere in the suite.
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
    ///
    /// Called from the rail's existing status poll (`crate::rail::render::AdeApp::start_status_polling`)
    /// rather than from a timer of its own, matching how the review measurement already rides
    /// that loop. Deliberately *not* called from `build_agent_rows`: that runs on every render,
    /// and this ends in an `fsync`.
    ///
    /// Only agents that have really reported a hook are recorded - never a shell, never a Codex
    /// agent, and never a Claude agent still running on the quiescence heuristic. See the comment
    /// on the gate itself for why that is the feature rather than a shortcut.
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

        let entries: Vec<(String, std::path::PathBuf, &'static str, i64, _, _, _)> = recordable
            .into_iter()
            .filter_map(|(id, key, cwd, kind_label, spawned_at_unix)| {
                let agent = self.agents.iter().find(|agent| agent.id == id)?;
                let status = self.agent_status(agent, cx);
                let (activity, question) = match &self.hook_runtime {
                    Some(runtime) => runtime.text_for(id),
                    None => (None, None),
                };
                Some((
                    key,
                    cwd,
                    kind_label,
                    spawned_at_unix,
                    status,
                    activity,
                    question,
                ))
            })
            .collect();

        for (key, cwd, kind_label, spawned_at_unix, status, activity, question) in entries {
            if self.agent_status_state.set(
                key.clone(),
                &cwd,
                kind_label,
                spawned_at_unix,
                status,
                activity,
                question,
                now,
            ) {
                changed = true;
            }
            touched.push(key);
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
        }
    }

    /// Writes [`crate::root::AdeApp::agent_status_state`] to disk on the background executor.
    ///
    /// Off the UI thread deliberately, exactly like
    /// `crate::review::flow::AdeApp::persist_review_baselines`: `save_merged_at` holds the
    /// process-wide `crate::persisted_state_lock` mutex across two `fsync`s, and other persisted
    /// files' writers contend for that same lock. Running it inline would put a disk flush on the
    /// render thread.
    fn persist_agent_statuses(&mut self, cx: &mut Context<Self>) {
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
