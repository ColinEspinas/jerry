//! Reacts to the host's `event/session-exited` notification (`docs/architecture/decisions.md`
//! §23, §24): the control-plane signal every subscribed client sees, unlike a pane's own
//! data-plane byte stream (`crate::terminal::pane::SessionAdapter::take_output`), which only the
//! one attached pane observes. Forgets the exited process from the host's agent table so
//! `AgentsQuery` stops listing it, even while [`crate::work_surface::agents::Agents::close`]'s
//! "an unclean exit keeps its tab open" policy leaves the pane itself in place, and marks the
//! pane itself exited (`TerminalPane::mark_exited_from_event`) - the real exit signal for a
//! session attached over its data-plane socket
//! (`crate::terminal::socket_adapter::SocketSessionAdapter`), whose byte stream never carries one
//! of its own. Owns no state of its own - see `crate::work_surface::worktree_created`, the
//! identical pattern this mirrors.

use crate::root::AdeApp;
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{Context, Task};
use jerry_core::Message;
use jerry_pty::ExitStatus;
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
    /// `event/session-exited`'s own handler: resolves which open agent (if any) the host's
    /// session id belongs to, marks that agent's pane exited (`TerminalPane::
    /// mark_exited_from_event` - a no-op if a `SessionHandle`-backed pane already recorded its
    /// own exit from `PtyOutput::Exited`), and forgets it from the host's agent table. When no
    /// agent matches yet, the status is recorded instead of dropped
    /// (`Agents::note_unmatched_session_exit`): a process short-lived enough (`sh -c exit`) can
    /// exit before `Agents::set_host_session_id` itself has run, particularly on Linux, and that
    /// method applies the exit - agent-table and pane alike - the moment it does. Still a genuine
    /// no-op for a session this instance never spawns at all (another client's session on the
    /// same host).
    fn handle_session_exited(&mut self, params: Value, cx: &mut Context<Self>) {
        let Some(session_id) = params
            .get("id")
            .and_then(Value::as_str)
            .map(jerry_core::SessionId::from)
        else {
            return;
        };
        let Some(id) = self.agents.agent_for_host_session(&session_id) else {
            self.agents.note_unmatched_session_exit(
                session_id,
                params.get("status").cloned().unwrap_or(Value::Null),
            );
            return;
        };
        if let Some(pane) = self.agents.pane_for(id) {
            let status = exit_status_from_wire(params.get("status"));
            pane.update(cx, |pane, cx| pane.mark_exited_from_event(status, cx));
        }
        self.agents.forget_host_agent(id);
    }
}

/// The reverse of `jerry_host::session::exit_status_wire`: reconstructs a real
/// `jerry_pty::ExitStatus` from the wire shape `event/session-exited` carries. Lossy when a
/// signal is present - `portable_pty::ExitStatus::with_signal` has no way to also carry the
/// original exit code, so only whether the process exited cleanly (`success()`, checked
/// elsewhere) round-trips exactly; a missing/malformed `status` becomes a plain, honest failure
/// (`with_exit_code(1)`) rather than a fabricated success. `pub(crate)`: also how
/// `crate::work_surface::agents::Agents::set_host_session_id` marks a pane exited when it applies
/// an exit `Self::handle_session_exited` had to record as pending (see that method's own docs).
pub(crate) fn exit_status_from_wire(status: Option<&Value>) -> ExitStatus {
    let Some(status) = status else {
        return ExitStatus::with_exit_code(1);
    };
    if let Some(signal) = status.get("signal").and_then(Value::as_str) {
        return ExitStatus::with_signal(signal);
    }
    let code = status
        .get("code")
        .and_then(Value::as_u64)
        .map(|code| code as u32)
        .unwrap_or(1);
    ExitStatus::with_exit_code(code)
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
        // Checked before any `run_until_parked` at all, deliberately: `Agents::spawn_inner`'s own
        // `spawn_resolved` registers this agent-table entry synchronously, inside the same
        // `update_in` call above, before the `SessionSpawn` dispatch task that starts the real,
        // genuinely-exiting process even runs - so nothing async has had a chance to race this
        // read yet. A post-drain read here raced the real exit on Linux: the child could exit and
        // its `event/session-exited` be processed - forgetting the agent - inside the very same
        // `run_until_parked` this sanity check would have shared with the wait loop below, making
        // an already-empty table look like the registration itself never happened.
        let registered = app.read_with(cx, |app, _| app.agents.host_agent_ids_for_test());
        assert_eq!(
            registered.len(),
            1,
            "sanity check: the real spawn must have registered one agent-table entry - got \
             {registered:?}"
        );

        let forgotten = test_support::wait_until(Duration::from_secs(30), || {
            cx.run_until_parked();
            app.read_with(cx, |app, _| app.agents.host_agent_ids_for_test().is_empty())
        });
        let remaining = app.read_with(cx, |app, _| app.agents.host_agent_ids_for_test());
        assert!(
            forgotten,
            "the host agent table must stop listing this agent once its real exit's \
             event/session-exited notification reaches this instance's own subscriber - still \
             has {remaining:?}"
        );
    }
}
