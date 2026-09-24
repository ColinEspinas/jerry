//! Reacts to the host's `event/session-exited` notification (`docs/architecture/decisions.md`
//! §25): the control-plane signal every subscribed client sees, unlike a pane's own data-plane
//! byte stream (`crate::terminal::pane::SessionAdapter::take_output`), which only the one
//! attached pane observes - and, for a socket-attached (production) session, the *only* place
//! its real exit is ever observed at all, since that adapter synthesizes no exit signal of its
//! own. `AgentsQuery` already stops listing an exited agent on its own (`SessionManager::
//! agent_entries` filters by the real record's own exit status - no client action needed here for
//! that); what this module owns is applying the real exit to the pane itself
//! (`TerminalPane::mark_exited_from_event`) when a match already exists, and `Agents::
//! pending_exits` (the real Linux race where the notification can arrive before `Agents::
//! set_host_session_id` has even run) when it doesn't yet.

use crate::root::AdeApp;
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{Context, Task};
use jerry_core::{ExitStatusWire, Message};
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
    /// `event/session-exited`'s own handler. A session this instance never spawned at all
    /// (another client's session on the same host) matches no pane and is silently ignored -
    /// there is nothing here for it to apply. Otherwise:
    /// - a match already exists: applies the real exit to that pane directly
    ///   (`TerminalPane::mark_exited_from_event`) - for a socket-attached session this is the
    ///   *only* place its real exit is ever observed, since that adapter synthesizes none of its
    ///   own.
    /// - no match yet: the id (with its own status) is recorded
    ///   (`Agents::note_unmatched_session_exit`) so `Agents::set_host_session_id` can still apply
    ///   it once that agent's id is finally known - a process short-lived enough (`sh -c exit`)
    ///   can exit before that call itself has run, particularly on Linux.
    fn handle_session_exited(&mut self, params: Value, cx: &mut Context<Self>) {
        let Some(session_id) = params
            .get("id")
            .and_then(Value::as_str)
            .map(jerry_core::SessionId::from)
        else {
            return;
        };
        let Some(status) = params
            .get("status")
            .and_then(|value| serde_json::from_value::<ExitStatusWire>(value.clone()).ok())
        else {
            return;
        };
        match self.agents.pane_for_host_session(&session_id) {
            Some(pane) => {
                pane.update(cx, |pane, cx| pane.mark_exited_from_event(&status, cx));
            }
            None => self.agents.note_unmatched_session_exit(session_id, status),
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

    /// A real child process that blocks reading one line from its own stdin, then exits `0` -
    /// spelled for this platform's own always-present interpreter. Blocking keeps the process
    /// alive until the test itself releases it (`send_prompt`, below), so the sanity check that
    /// it is really listed cannot lose a race against a child that already exited on its own.
    fn exiting_command(cwd: std::path::PathBuf) -> TerminalSpec {
        #[cfg(windows)]
        {
            TerminalSpec::command(
                "cmd",
                vec!["/c".to_string(), "set /p x=&exit 0".to_string()],
                cwd,
            )
        }
        #[cfg(unix)]
        {
            TerminalSpec::command(
                "sh",
                vec!["-c".to_string(), "read x; exit 0".to_string()],
                cwd,
            )
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

        // Releases the blocked child now that the sanity check above has observed it - on a
        // fast CI runner the child could otherwise already be gone before that check even ran,
        // which would make both assertions here order-dependent (issue #506 review).
        let pane = app
            .update(cx, |app, _cx| {
                app.agents.active().map(|agent| agent.pane.clone())
            })
            .expect("the just-spawned agent must be the active one");
        pane.update(cx, |pane, cx| {
            assert!(
                pane.send_prompt("x", cx),
                "failed to release the blocked child"
            );
        });
        cx.run_until_parked();

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
