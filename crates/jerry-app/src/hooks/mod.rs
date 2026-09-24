//! A real, structural status side-channel for Claude Code agents (GitHub issue #239, phase 2).
//! Since decision Q10 (`docs/architecture/decisions.md` §19), hook facts arrive as `event/hook`
//! notifications from `jerry-host`, pushed by `jerry hook <event>` - not polled off a loopback
//! listener this process used to own.

pub mod cursor_event;
pub mod cursor_hooks_file;
pub mod event;
pub mod flow;
pub mod history;
pub mod inbox;
pub mod settings_file;
pub mod store;

#[cfg(test)]
mod integration_tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{Context, Task};
use jerry_core::Message;

use crate::root::AdeApp;
use crate::work_surface::agents::AgentId;

/// Everything one Jerry launch needs to receive hooks: the generated `--settings` file and the
/// facts learned from it so far. Holds no connection of its own - `event/hook` notifications
/// reach it through whichever repository's own event subscription happens to be forwarding them
/// (`crate::host::ensure_repo_host_connected`, [`spawn_consumer`]), the same per-repository
/// pipeline `worktree_created`/`session_exited` already use.
pub struct HookRuntime {
    files: settings_file::HookFiles,
    inbox: Arc<Mutex<inbox::HookInbox>>,
    edits: Arc<Mutex<inbox::EditLog>>,
    /// Highest [`jerry_core::HookInboxEntry::seq`] already applied, per agent - [`Self::record`]'s
    /// own dedup guard (issue #532's review). The live `event/hook` consumer ([`spawn_consumer`])
    /// subscribes before a connect-time replay (`crate::host::AdeApp::seed_hook_runtime`) takes
    /// its own `HooksQuery` snapshot, so the identical real entry can reach `Self::record` twice;
    /// `HookInbox::record`/`EditLog::record` are not idempotent (a repeated `Stop` would double a
    /// turn count, a repeated edit would double-append), so this is what keeps a shared entry from
    /// being counted twice regardless of which path delivered it first.
    applied_seq: Arc<Mutex<HashMap<jerry_core::AgentId, u64>>>,
}

impl HookRuntime {
    /// Writes this launch's `--settings` file naming `jerry_binary`. `None` when hooks are
    /// unsupported on this platform, or the settings file could not be written - both permanent
    /// for the life of this `AdeApp` (see `crate::hooks::flow::AdeApp::hook_injection_for`'s own
    /// one-shot bring-up).
    pub fn start(parent: &Path, jerry_binary: &Path) -> Option<HookRuntime> {
        if !settings_file::is_supported() {
            log::info!(
                "agent hooks are not supported on this platform - agent status will use the \
                 terminal-title and quiescence signals only"
            );
            return None;
        }
        let files = match settings_file::HookFiles::write_in(parent, jerry_binary) {
            Ok(files) => files,
            Err(err) => {
                log::warn!("could not write the agent hook settings ({err}) - falling back to the terminal-title and quiescence signals");
                return None;
            }
        };
        log::info!(
            "agent hook settings ready at {}",
            files.settings_path().display()
        );
        Some(HookRuntime {
            files,
            inbox: Arc::new(Mutex::new(inbox::HookInbox::default())),
            edits: Arc::new(Mutex::new(inbox::EditLog::default())),
            applied_seq: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// An owned handle carrying everything a spawn needs, detached from `self` - `host_socket` is
    /// the spawning repository's own socket (`crate::host::AdeApp::host_socket_for`), resolved
    /// fresh per spawn since different agents in the same launch can belong to different
    /// repositories.
    pub fn injection(&self, host_socket: PathBuf) -> HookInjection {
        HookInjection {
            settings_path: self.files.settings_path().to_path_buf(),
            plugin_dir: self.files.plugin_dir().to_path_buf(),
            host_socket,
        }
    }

    /// Records one raw `event/hook` entry - see [`spawn_consumer`]. A no-op for an entry whose
    /// `seq` was already applied for its agent - see [`Self::applied_seq`]'s own docs.
    fn record(&self, entry: &jerry_core::HookInboxEntry) {
        if !self.mark_applied(entry) {
            return;
        }
        record_hook_notification(&self.inbox, &self.edits, entry);
    }

    /// `true` (and advances the watermark) the first time `entry.seq` is seen for its agent;
    /// `false` for a `seq` already applied or older - [`Self::record`]'s own dedup guard.
    fn mark_applied(&self, entry: &jerry_core::HookInboxEntry) -> bool {
        let Ok(mut applied) = self.applied_seq.lock() else {
            return false;
        };
        match applied.get(&entry.agent_id) {
            Some(&highest) if entry.seq <= highest => false,
            _ => {
                applied.insert(entry.agent_id.clone(), entry.seq);
                true
            }
        }
    }

    /// This agent's current hook fact, for [`crate::rail::status::derive_status`].
    pub fn signal_for(&self, id: AgentId) -> crate::rail::status::HookSignal {
        let Ok(inbox) = self.inbox.lock() else {
            return crate::rail::status::HookSignal::default();
        };
        match inbox.get(id) {
            Some(record) => crate::rail::status::HookSignal {
                fact: Some(record.report.fact),
                age: record.received_at.elapsed(),
            },
            None => crate::rail::status::HookSignal::default(),
        }
    }

    /// This agent's current hook-derived `(activity, question)` text, for its rail row.
    pub fn text_for(&self, id: AgentId) -> (Option<String>, Option<String>) {
        let Ok(inbox) = self.inbox.lock() else {
            return (None, None);
        };
        match inbox.get(id) {
            Some(record) if record.received_at.elapsed() < event::HOOK_SIGNAL_TTL => (
                record.report.activity.clone(),
                record.report.question.clone(),
            ),
            _ => (None, None),
        }
    }

    /// This agent's real Claude Code `session_id` (GitHub issue #227), if its hooks have ever
    /// reported one - deliberately not gated by staleness the way [`Self::text_for`] is: a
    /// conversation stays exactly as resumable as it was the instant its last hook fired.
    pub fn session_id_for(&self, id: AgentId) -> Option<String> {
        let inbox = self.inbox.lock().ok()?;
        inbox.get(id)?.report.session_id.clone()
    }

    /// This agent's real run-level facts - completed turns and the run's title (GitHub issue
    /// #227).
    pub fn run_facts_for(&self, id: AgentId) -> inbox::RunFacts {
        let Ok(inbox) = self.inbox.lock() else {
            return inbox::RunFacts::default();
        };
        match inbox.get(id) {
            Some(record) => inbox::RunFacts {
                turns: record.turns,
                title: record.first_prompt.clone(),
            },
            None => inbox::RunFacts::default(),
        }
    }

    /// Every file write reported since the last call, in arrival order - the signal
    /// `crate::provenance` turns into per-line attribution (GitHub issue #284). Returns
    /// `(edits, dropped)`.
    pub fn drain_edits(&self) -> (Vec<inbox::AgentEdit>, usize) {
        let Ok(mut edits) = self.edits.lock() else {
            return (Vec::new(), 0);
        };
        edits.drain()
    }

    /// Drops an agent's recorded facts, so a future [`AgentId`] can't inherit them.
    pub fn forget(&self, id: AgentId) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.forget(id);
        }
        if let Ok(mut edits) = self.edits.lock() {
            edits.forget(id);
        }
        if let Ok(mut applied) = self.applied_seq.lock() {
            applied.remove(&jerry_core::AgentId::from(id.to_string()));
        }
    }
}

/// Applies one raw entry to [`AdeApp::hook_runtime`] - the one real path both [`spawn_consumer`]
/// (a live `event/hook` notification) and `crate::host::AdeApp::seed_hook_runtime` (a
/// repository-connect replay, decisions.md §26) feed an entry through, so a relaunched instance's
/// rail renders exactly what it would have if it had been running the whole time. A no-op while
/// [`HookRuntime`] has not started yet (hooks unsupported, or its lazy bring-up has not run -
/// `crate::hooks::flow::AdeApp::hook_injection_for`) - the same silent drop an early live event
/// already had.
pub(crate) fn apply_entry(app: &AdeApp, entry: &jerry_core::HookInboxEntry) {
    if let Some(runtime) = &app.hook_runtime {
        runtime.record(entry);
    }
}

/// Drains `events` for as long as the returned `Task` is held - one per repository connection
/// (`crate::host::ensure_repo_host_connected`), all feeding the same [`HookRuntime`] regardless
/// of which repository the notification came from, mirroring `crate::work_surface::
/// worktree_created::spawn_consumer`'s own shape exactly. Acknowledges what it applied
/// (`HookAck`, best-effort, decisions.md §26) so the host's own bounded inbox is pruned by real
/// consumption, not just its cap. An `event/hook` whose params are not a real
/// [`jerry_core::HookInboxEntry`] is silently dropped.
pub(crate) fn spawn_consumer(
    mut events: mpsc::UnboundedReceiver<Message>,
    common_dir: PathBuf,
    cx: &mut Context<AdeApp>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while let Some(message) = events.next().await {
            let Message::Notification { method, params } = message else {
                continue;
            };
            if method != "event/hook" {
                continue;
            }
            let Ok(entry) = serde_json::from_value::<jerry_core::HookInboxEntry>(params) else {
                continue;
            };
            if this
                .update(cx, |app, _cx| apply_entry(app, &entry))
                .is_err()
            {
                continue;
            }
            let ack = this.update(cx, |app, cx| {
                app.dispatch(
                    common_dir.clone(),
                    jerry_core::Request::Command(jerry_core::AppCommand::HookAck(
                        jerry_core::HookAck {
                            agent_id: entry.agent_id.clone(),
                            up_to: entry.seq,
                        },
                    )),
                    cx,
                )
            });
            if let Ok(task) = ack {
                let _ = task.await;
            }
        }
    })
}

/// Parses one `event/hook` entry's payload and records it exactly as the old HTTP listener's
/// `read_and_record` did: an edit is appended before the inbox merge, and a `Before` snapshot is
/// taken from disk at record time, not deferred to whenever a reader drains it.
fn record_hook_notification(
    inbox: &Mutex<inbox::HookInbox>,
    edits: &Mutex<inbox::EditLog>,
    entry: &jerry_core::HookInboxEntry,
) {
    let Ok(agent_id) = entry.agent_id.to_string().parse::<AgentId>() else {
        // Not a recognizable `JERRY_AGENT_ID` - there is no rail row this could belong to.
        return;
    };
    let Ok(payload_bytes) = serde_json::to_vec(&entry.payload) else {
        return;
    };
    let Some(report) = event::parse(&entry.event, &payload_bytes) else {
        return;
    };

    if let Some(file) = report.edit.clone() {
        let before = match file.phase {
            event::EditPhase::Before => crate::provenance::store::snapshot_for_edit(
                &crate::provenance::absolute_edit_path(&file),
            ),
            event::EditPhase::After => None,
        };
        if let Ok(mut edits) = edits.lock() {
            edits.record(agent_id, file, before);
        }
    }
    if let Ok(mut inbox) = inbox.lock() {
        inbox.record(agent_id, report);
    }
}

/// An owned, spawn-time snapshot of a [`HookRuntime`] - see [`HookRuntime::injection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInjection {
    /// Kept as a real [`PathBuf`], not a `String`.
    settings_path: PathBuf,
    /// The skill plugin directory (`docs/architecture/decisions.md` §21), passed as
    /// `claude --plugin-dir <path>` alongside `--settings` - `settings_file::HookFiles::fill`
    /// writes it next to the settings file in the same per-launch directory.
    plugin_dir: PathBuf,
    host_socket: PathBuf,
}

impl HookInjection {
    /// The extra CLI arguments and environment a freshly spawned `claude` needs so its hooks
    /// reach this Jerry and it can discover the `jerry` skill - `(args, env)`, ready for
    /// `crate::terminal::pane::TerminalSpec`.
    pub fn spawn_extras(&self, id: AgentId) -> Option<crate::work_surface::agents::SpawnExtras> {
        let Some(settings_path) = self.settings_path.to_str() else {
            log::warn!(
                "the generated hook settings path ({}) is not valid UTF-8, so it cannot be passed \
                 as a command-line argument - this agent will fall back to the terminal-title and \
                 quiescence signals",
                self.settings_path.display()
            );
            return None;
        };
        let mut args = vec!["--settings".to_owned(), settings_path.to_owned()];
        match self.plugin_dir.to_str() {
            Some(plugin_dir) => {
                args.push("--plugin-dir".to_owned());
                args.push(plugin_dir.to_owned());
            }
            None => log::warn!(
                "the generated skill plugin directory ({}) is not valid UTF-8, so it cannot be \
                 passed as a command-line argument - this agent will not have the jerry skill",
                self.plugin_dir.display()
            ),
        }
        Some((args, self.env(id)))
    }

    /// The environment-only counterpart of [`Self::spawn_extras`], for an agent CLI with no
    /// `--settings <path>`-equivalent flag (`cursor-agent`, GitHub issue #479) - just the
    /// `AGENT_ENV`/`SOCKET_ENV` pair, with no extra CLI arguments to prepend.
    pub fn env_only(&self, id: AgentId) -> Vec<(String, String)> {
        self.env(id)
    }

    fn env(&self, id: AgentId) -> Vec<(String, String)> {
        vec![
            (settings_file::AGENT_ENV.to_owned(), id.to_string()),
            (
                settings_file::SOCKET_ENV.to_owned(),
                self.host_socket.to_string_lossy().into_owned(),
            ),
        ]
    }
}

#[cfg(test)]
impl HookInjection {
    /// A `HookInjection` a test can build with no real `HookRuntime` behind it - exactly what a
    /// real one would hand out, without the settings file, the socket listener, or the
    /// notification-consuming task a full [`HookRuntime::start`] needs. For tests whose subject
    /// is what a spawn *does* with an injection, not how one comes to exist.
    pub(crate) fn for_test(settings_path: PathBuf, host_socket: PathBuf) -> HookInjection {
        let plugin_dir = settings_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        HookInjection {
            settings_path,
            plugin_dir,
            host_socket,
        }
    }
}
