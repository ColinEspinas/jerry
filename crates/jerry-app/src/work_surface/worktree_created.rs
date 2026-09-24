//! Reacts to the host's `event/worktree-created` notification
//! (`docs/architecture/decisions.md` §21): opens the new worktree using the same code path a
//! rail click takes, and - when the event carried one - spawns the named agent there via the
//! existing `Agents::spawn`/`spawn_with_prompt`. Owns no state of its own; the consuming `Task`
//! is `crate::host::RepoHost`'s, cancelled when that repository's connection drops.

use crate::root::AdeApp;
use crate::work_surface::agents::{AgentKind, ProcessKind};
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{AppContext as _, Context, Task};
use jerry_core::Message;
use serde_json::Value;
use std::path::PathBuf;

/// Drains `events` for as long as the returned `Task` is held - a channel-woken loop, never a
/// timer (§16). The subscription itself already happened, synchronously or over a real socket,
/// when `events` was created (`crate::host::ensure_repo_host_connected`), so this task can start
/// arbitrarily later than that with nothing lost in between. `this.update_in` reaches this launch's one real
/// window through GPUI's own entity-to-window lookup, exactly as
/// `AdeApp::spawn_with_minted_chat_id` already does from an identical async-continuation shape.
pub(crate) fn spawn_consumer(
    mut events: mpsc::UnboundedReceiver<Message>,
    cx: &mut Context<AdeApp>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Some(message) = events.next().await {
            let jerry_core::Message::Notification { method, params } = message else {
                continue;
            };
            if method != "event/worktree-created" {
                continue;
            }
            let _ = this.update_in(cx, |app, window, cx| {
                app.handle_worktree_created(params, window, cx);
            });
        }
    })
}

impl AdeApp {
    /// `event/worktree-created`'s own handler: resolves which open repository the new worktree
    /// belongs to, refreshes that repository's worktree list, opens the worktree through
    /// [`Self::select_worktree_by_path`] - the same path a rail click takes - and, when the
    /// event carried an agent, spawns it there.
    fn handle_worktree_created(
        &mut self,
        params: Value,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = params
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
        else {
            return;
        };
        let agent: Option<AgentKind> = params
            .get("agent")
            .cloned()
            .and_then(|value| serde_json::from_value::<Option<jerry_core::AgentSpec>>(value).ok())
            .flatten()
            .map(AgentKind::from);
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_owned);

        let repo_roots: Vec<PathBuf> = self.repos.iter().map(|repo| repo.path.clone()).collect();
        let task = cx.spawn_in(window, async move |this, cx| {
            let lookup_path = path.clone();
            let common = cx
                .background_spawn(async move { jerry_git::git_common_dir(&lookup_path) })
                .await;
            let Ok(common) = common else {
                log::warn!(
                    "jerry-app: could not resolve the repository for the new worktree at {}",
                    path.display()
                );
                return;
            };
            let Some(repo_root) = repo_roots
                .into_iter()
                .find(|root| root.join(".git") == common)
            else {
                log::warn!(
                    "jerry-app: {} is not inside a repository this instance has open",
                    path.display()
                );
                return;
            };
            let fetch_root = repo_root.clone();
            let listed = cx
                .background_spawn(async move { jerry_git::list_worktrees_porcelain(&fetch_root) })
                .await;
            let Ok(entries) = listed else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let items = crate::rail::worktrees::build_worktree_items(entries);
                if let Some(repo) = this.repos.iter_mut().find(|repo| repo.path == repo_root) {
                    repo.worktrees = items.clone();
                    repo.worktrees_loaded = true;
                }
                if this.focused_repo_path() == repo_root {
                    this.worktrees = items;
                }
                this.select_worktree_by_path(&path, window, cx);
                if let Some(agent_kind) = agent {
                    this.spawn_created_worktree_agent(
                        agent_kind,
                        path.clone(),
                        prompt.clone(),
                        window,
                        cx,
                    );
                }
                cx.notify();
            });
        });
        self._new_agent_pane_task.push(task);
    }

    /// The agent-spawn half of [`Self::handle_worktree_created`]: spawns `agent_kind` into
    /// `cwd`, exactly as a human's "New agent here" click would (`Agents::spawn`), or with
    /// `prompt` as its leading argument (`Agents::spawn_with_prompt`) when one was given.
    fn spawn_created_worktree_agent(
        &mut self,
        agent_kind: AgentKind,
        cwd: PathBuf,
        prompt: Option<String>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let font_size = self.settings.appearance.terminal_font_size;
        let shell_override = self.settings.terminal.shell_override().map(str::to_owned);
        let hook_injection = self.hook_injection_for(ProcessKind::Agent(agent_kind), &cwd);
        let id = match prompt {
            Some(prompt) => self.agents.spawn_with_prompt(
                agent_kind,
                cwd,
                font_size,
                shell_override.as_deref(),
                hook_injection.as_ref(),
                prompt,
                window,
                cx,
            ),
            None => self.agents.spawn(
                ProcessKind::Agent(agent_kind),
                cwd,
                font_size,
                shell_override.as_deref(),
                hook_injection.as_ref(),
                window,
                cx,
            ),
        };
        self.after_agent_spawn(id, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{open_test_app, temp_repo};
    use crate::work_surface::agents::{AgentKind, ProcessKind};
    use gpui::TestAppContext;
    use jerry_core::{AgentSpec, AppCommand, Report, Request, WorktreeCreate};
    use std::path::PathBuf;

    #[gpui::test]
    async fn a_real_worktree_create_with_an_agent_opens_it_and_spawns_that_agent(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.to_path_buf());

        let report = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.to_path_buf(),
                    Request::Command(AppCommand::WorktreeCreate(WorktreeCreate {
                        branch: "feature-created-agent".into(),
                        from: None,
                        agent: Some(AgentSpec::Claude),
                        prompt: Some("fix the bug".into()),
                    })),
                    cx,
                )
            })
            .await
            .expect("dispatched");
        let Report::Ok { outcome } = report else {
            panic!("expected ok, got {report:?}")
        };
        let created = PathBuf::from(outcome["path"].as_str().expect("path"));

        // The dispatch's own `event/worktree-created` reaches this app's subscriber on the same
        // deterministic test executor - no real time passes, so parking it out is enough for the
        // whole reaction (refresh, select, spawn) to have run.
        cx.run_until_parked();

        app.read_with(cx, |app, _cx| {
            assert!(
                app.worktrees.iter().any(|item| item.path == created),
                "the new worktree must appear in the refreshed list: {:?}",
                app.worktrees
                    .iter()
                    .map(|item| &item.path)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                app.current_worktree_path(),
                Some(created.clone()),
                "opening it means selecting it, exactly as a rail click would"
            );
            let spawned = app
                .agents
                .iter()
                .find(|agent| agent.cwd == created)
                .expect("an agent was spawned into the new worktree");
            assert_eq!(spawned.kind, ProcessKind::Agent(AgentKind::Claude));
        });

        let _ = std::fs::remove_dir_all(created.parent().expect("parent"));
    }
}
