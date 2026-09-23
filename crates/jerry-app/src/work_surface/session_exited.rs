//! Reacts to the host's `event/session-exited` notification (`docs/architecture/decisions.md`
//! §23): the control-plane signal every subscribed client sees, unlike a pane's own data-plane
//! byte stream (`crate::terminal::pane::SessionAdapter::take_output`), which only the one
//! attached pane observes. Forgets the exited process from the host's agent table so
//! `AgentsQuery` stops listing it, even while [`crate::work_surface::agents::Agents::close`]'s
//! "an unclean exit keeps its tab open" policy leaves the pane itself in place. Owns no state of
//! its own - see `crate::work_surface::worktree_created`, the identical pattern this mirrors.

use crate::root::AdeApp;
use futures::StreamExt;
use gpui::{Context, Task};
use jerry_host::LocalClient;
use serde_json::Value;

/// Subscribes to `client`'s notifications for as long as the returned `Task` is held - see
/// `crate::work_surface::worktree_created::spawn_consumer`'s own docs, which this copies exactly
/// but for `event/session-exited`.
pub(crate) fn spawn_consumer(client: LocalClient, cx: &mut Context<AdeApp>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut events = client.subscribe();
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
    /// `event/session-exited`'s own handler: resolves which open agent (if any) the host's
    /// session id belongs to and forgets it from the host's agent table. A no-op for a session
    /// this instance never attached to a pane (another client's session, or one already closed).
    fn handle_session_exited(&mut self, params: Value, _cx: &mut Context<Self>) {
        let Some(session_id) = params
            .get("id")
            .and_then(Value::as_str)
            .map(jerry_core::SessionId::from)
        else {
            return;
        };
        if let Some(id) = self.agents.agent_for_host_session(&session_id) {
            self.agents.forget_host_agent(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::terminal::pane::TerminalSpec;
    use crate::test_support::{open_test_app, temp_repo};
    use crate::work_surface::agents::{AgentKind, ProcessKind};
    use gpui::TestAppContext;
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
    /// workspace's own nextest mitigation deliberately keeps `claude` off `PATH` anyway), waits
    /// for the host to observe its real exit and publish `event/session-exited`, and checks the
    /// host's own agent table - the same table `AgentsQuery` answers from - to prove this
    /// instance's own subscriber reacted to the notification rather than merely to its pane's own
    /// data-plane stream. This test's agent tab is left open the whole time (an unclean exit
    /// keeps its tab - `Agents::close`'s own policy), so the table entry going away could not be
    /// explained by the tab having been closed instead.
    #[gpui::test]
    fn a_real_agents_exit_is_forgotten_from_the_agent_table_once_its_event_arrives(
        cx: &mut TestAppContext,
    ) {
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
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.host_agent_ids_for_test().len()),
            1,
            "sanity check: the real spawn must have registered one agent-table entry"
        );

        let forgotten = test_support::wait_until(Duration::from_secs(30), || {
            cx.run_until_parked();
            app.read_with(cx, |app, _| app.agents.host_agent_ids_for_test().is_empty())
        });
        assert!(
            forgotten,
            "the host agent table must stop listing this agent once its real exit's \
             event/session-exited notification reaches this instance's own subscriber"
        );
    }
}
