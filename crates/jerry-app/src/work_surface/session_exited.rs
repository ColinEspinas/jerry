//! Reacts to the host's `event/session-exited` notification (`docs/architecture/decisions.md`
//! §23): the control-plane signal every subscribed client sees, unlike a pane's own data-plane
//! byte stream (`crate::terminal::pane::SessionAdapter::take_output`), which only the one
//! attached pane observes. `AgentsQuery` already stops listing an exited agent on its own
//! (`SessionManager::agent_entries` filters by the real record's own exit status - no client
//! action needed); what this module still owns is `Agents::pending_exits`, the real Linux race
//! where the notification can arrive before `Agents::set_host_session_id` has even run. Owns no
//! other state of its own - see `crate::work_surface::worktree_created`, the identical pattern
//! this mirrors.

use crate::root::AdeApp;
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{Context, Task};
use jerry_core::Message;
use serde_json::Value;

/// Drains `events` for as long as the returned `Task` is held - see
/// `crate::work_surface::worktree_created::spawn_consumer`'s own docs, which this copies exactly
/// but for `event/session-exited`.
pub(crate) fn spawn_consumer(
    mut events: mpsc::UnboundedReceiver<Message>,
    cx: &mut Context<AdeApp>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Some(message) = events.next().await {
            let jerry_core::Message::Notification { method, params } = message else {
                continue;
            };
            if method != "event/session-exited" {
                continue;
            }
            let _ = this.update(cx, |app, cx| {
                app.handle_session_exited(params, cx);
            });
        }
    })
}

impl AdeApp {
    /// `event/session-exited`'s own handler: when no open agent's `host_session_id` matches yet,
    /// the id is recorded (`Agents::note_unmatched_session_exit`) so `Agents::
    /// set_host_session_id` can still clear it once that agent's id is finally known - a process
    /// short-lived enough (`sh -c exit`) can exit before that call itself has run, particularly on
    /// Linux. Once a match already exists there is nothing left to do here: `AgentsQuery` already
    /// stopped listing the exited agent on its own (this method's own module docs), and a session
    /// this instance never spawned at all (another client's session on the same host) never
    /// matches to begin with.
    fn handle_session_exited(&mut self, params: Value, _cx: &mut Context<Self>) {
        let Some(session_id) = params
            .get("id")
            .and_then(Value::as_str)
            .map(jerry_core::SessionId::from)
        else {
            return;
        };
        if self.agents.agent_for_host_session(&session_id).is_none() {
            self.agents.note_unmatched_session_exit(session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::terminal::pane::TerminalSpec;
    use crate::test_support::{open_test_app, temp_repo};
    use crate::work_surface::agents::{AgentKind, ProcessKind};
    use gpui::TestAppContext;
    use jerry_core::{AppQuery, Report, Request};
    use std::time::Duration;

    /// A real child process that does nothing but exit `0`, spelled for this platform's own
    /// always-present interpreter - the same idiom
    /// `crate::terminal::pane::process_exit_event_tests::exiting_pane` uses.
    fn exiting_command(cwd: std::path::PathBuf) -> TerminalSpec {
        #[cfg(windows)]
        {
            TerminalSpec::command("cmd", vec!["/c".to_string(), "exit 0".to_string()], cwd)
        }
        #[cfg(unix)]
        {
            TerminalSpec::command("sh", vec!["-c".to_string(), "exit 0".to_string()], cwd)
        }
    }

    /// Spawns a real, genuinely-exiting process tagged as an agent (via
    /// `Agents::spawn_with_explicit_command_for_test`, since a real `claude`/`codex`/
    /// `cursor-agent` binary would never exit on its own within a test's budget, and this
    /// workspace's own nextest mitigation deliberately keeps `claude` off `PATH` anyway), and
    /// waits for a real `AgentsQuery` dispatch to stop listing it once the host observes the real
    /// exit - proving `SessionManager::agent_entries`'s own exit filter, not a shortcut into any
    /// in-process table. This test's agent tab is left open the whole time (an unclean exit keeps
    /// its tab - `Agents::close`'s own policy), so the listing going away could not be explained
    /// by the tab having been closed instead.
    #[gpui::test]
    async fn a_real_agents_exit_stops_agentsquery_listing_it(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.agents.spawn_with_explicit_command_for_test(
                ProcessKind::Agent(AgentKind::Claude),
                repo.path().to_path_buf(),
                12.0,
                exiting_command(repo.path().to_path_buf()),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let listed = app
            .update(cx, |app, cx| {
                app.dispatch(
                    repo.path().to_path_buf(),
                    Request::Query(AppQuery::Agents(Default::default())),
                    cx,
                )
            })
            .await
            .expect("agents query");
        let Report::Ok { outcome } = listed else {
            panic!("expected ok, got {listed:?}")
        };
        assert_eq!(
            outcome.as_array().expect("array").len(),
            1,
            "sanity check: the real spawn must be listed once its SessionSpawn dispatch \
             resolves - got {outcome:?}"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut forgotten = false;
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            let listed = app
                .update(cx, |app, cx| {
                    app.dispatch(
                        repo.path().to_path_buf(),
                        Request::Query(AppQuery::Agents(Default::default())),
                        cx,
                    )
                })
                .await;
            if matches!(listed, Ok(Report::Ok { outcome }) if outcome.as_array().is_some_and(Vec::is_empty))
            {
                forgotten = true;
                break;
            }
        }
        assert!(
            forgotten,
            "AgentsQuery must stop listing this agent once its real exit is observed"
        );
    }
}
