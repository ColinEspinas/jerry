//! Reacts to the host's `event/attention` notification (`docs/architecture/decisions.md` §26):
//! `command/attention-raise`'s own effect, an agent waking the human. Raises the same attention
//! signal an OSC 9/777 desktop notification already does (`TerminalPane::raise_attention_ping`),
//! keyed to the agent's own tab via its real host session id - the rail badge is the one real,
//! already-wired "wake the human" surface this app has (`crate::rail::status`'s own
//! `attention_pinged` read); there is no separate OS-level desktop notification path to raise
//! alongside it (verified: no such mechanism exists anywhere else in this crate).

use crate::root::AdeApp;
use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{Context, Task};
use jerry_core::Message;
use serde_json::Value;

/// Drains `events` for as long as the returned `Task` is held - see
/// `crate::work_surface::worktree_created::spawn_consumer`'s own docs, which this copies exactly
/// but for `event/attention`.
pub(crate) fn spawn_consumer(
    mut events: mpsc::UnboundedReceiver<Message>,
    cx: &mut Context<AdeApp>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Some(message) = events.next().await {
            let jerry_core::Message::Notification { method, params } = message else {
                continue;
            };
            if method != "event/attention" {
                continue;
            }
            let _ = this.update(cx, |app, cx| {
                app.handle_attention(params, cx);
            });
        }
    })
}

impl AdeApp {
    /// `event/attention`'s own handler. A `session_id` this instance has no pane for - another
    /// client's session on the same host, or an agent registered without a real spawned session -
    /// matches nothing and is silently ignored; there is no tab to raise the signal on.
    fn handle_attention(&mut self, params: Value, cx: &mut Context<Self>) {
        let Some(session_id) = params
            .get("session_id")
            .and_then(Value::as_str)
            .map(jerry_core::SessionId::from)
        else {
            return;
        };
        if let Some(pane) = self.agents.pane_for_host_session(&session_id) {
            pane.update(cx, |pane, cx| pane.raise_attention_ping(cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{open_test_app, temp_repo};
    use crate::work_surface::agents::{AgentKind, ProcessKind};
    use gpui::TestAppContext;
    use jerry_core::{AppCommand, AttentionRaise, Call, Request};

    /// A real `command/attention-raise`, dispatched as the spawned agent's own identity (never
    /// `AdeApp::dispatch`, which always dispatches as the human - see that method's own docs;
    /// `AttentionRaise` needs a real agent caller, exactly like a real `jerry attention` would be
    /// run from inside that agent's own pty), reaches this app's subscriber and lights that exact
    /// agent's own tab - not just "some" pending state.
    #[gpui::test]
    async fn a_real_attention_raise_lights_the_calling_agents_own_tab(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Agent(AgentKind::Claude),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let pane = app
            .update(cx, |app, _cx| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == id)
                    .map(|agent| agent.pane.clone())
            })
            .expect("the just-spawned agent has a pane");
        assert!(
            !pane.read_with(cx, |pane, _| pane.has_pending_attention_ping()),
            "no attention has been raised yet"
        );

        let client = app
            .update(cx, |app, _cx| app.host_client_for(repo.path()))
            .expect("the default in-process test host is reachable");
        let report = client
            .request(Call::agent(
                repo.path(),
                jerry_core::AgentId::from(id.to_string()),
                Request::Command(AppCommand::AttentionRaise(AttentionRaise {
                    message: "need your input".into(),
                })),
            ))
            .await
            .expect("dispatched");
        assert!(report.is_ok(), "{report:?}");
        cx.run_until_parked();

        assert!(
            pane.read_with(cx, |pane, _| pane.has_pending_attention_ping()),
            "a real event/attention keyed to this agent's own session must light its tab"
        );
    }
}
