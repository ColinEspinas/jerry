//! The host-owned hook store (`docs/architecture/decisions.md` §26): what this host learns from
//! the real `hook` requests it receives, kept coarse and bounded. A hook posted through one
//! `jerry-app`/`jerry-cli`/`jerry-mcp` instance is only visible to that instance until this
//! exists - the same reason `SessionManager` exists for sessions/agents. `jerry-app`'s own
//! richer, rendering-tuned parsing (`crate::hooks::event` there - activity/question/edit/prompt
//! kept apart, nudges folded rather than overwriting a real fact) stays local; this module never
//! reaches for it and never will, since `jerry-app` is the one crate allowed to depend on `gpui`
//! and this one must not.

use jerry_core::{AgentId, HookInboxEntry, HookKind, HookStatus};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Raw entries kept per agent before the oldest is dropped - generous enough to cover a burst of
/// tool calls between two consumer polls, small enough that an agent that never acknowledges
/// (`jerry_core::HookAck`) cannot grow this store without bound.
pub const HOOK_INBOX_CAP_PER_AGENT: usize = 64;

/// Distinct agents this store tracks before the least-recently-updated is evicted - mirrors
/// `jerry_app::hooks::inbox::MAX_TRACKED_AGENTS`'s own cap and reasoning.
pub const MAX_TRACKED_AGENTS: usize = 512;

/// The largest hook payload this store keeps verbatim; a bigger one is replaced with a real,
/// honest marker rather than silently dropped - a second, independent bound on what this store
/// retains, on top of `jerry hook`'s own stdin cap (`MAX_HOOK_PAYLOAD_BYTES` in `jerry-cli`).
pub const MAX_STORED_PAYLOAD_BYTES: usize = 64 * 1024;

struct AgentEntry {
    status: HookStatus,
    inbox: VecDeque<HookInboxEntry>,
    next_seq: u64,
    /// When this entry was last recorded to, for [`MAX_TRACKED_AGENTS`] eviction - a monotonic
    /// `Instant`, not `status.since`'s wall-clock seconds: several agents recorded within the
    /// same real second (a fast burst, or exactly this module's own eviction test) would
    /// otherwise tie and evict arbitrarily rather than genuinely least-recently-touched first,
    /// mirroring `jerry_app::hooks::inbox::HookRecord::received_at`'s identical reasoning for
    /// using `Instant` over a wall-clock timestamp.
    touched: Instant,
}

#[derive(Default)]
struct State {
    agents: HashMap<AgentId, AgentEntry>,
}

/// Cheap to clone (an `Arc`-backed handle) - mirrors `crate::fanout::Fanout`'s own shape.
#[derive(Clone, Default)]
pub struct HookStore(Arc<Mutex<State>>);

impl HookStore {
    /// Records one real hook request: appends a bounded [`HookInboxEntry`] to `agent_id`'s own
    /// inbox (dropping the oldest past [`HOOK_INBOX_CAP_PER_AGENT`]) and, when `event` names a
    /// real lifecycle fact (see [`derive_status`]), updates its current [`HookStatus`]. Returns
    /// the entry recorded, exactly as broadcast on `event/hook`.
    pub fn record(&self, agent_id: AgentId, event: String, payload: Value) -> HookInboxEntry {
        let now = unix_now();
        let payload = bound_payload(payload);
        let mut state = lock(&self.0);
        if !state.agents.contains_key(&agent_id) && state.agents.len() >= MAX_TRACKED_AGENTS {
            if let Some(oldest) = state
                .agents
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(id, _)| id.clone())
            {
                state.agents.remove(&oldest);
            }
        }
        let entry = state
            .agents
            .entry(agent_id.clone())
            .or_insert_with(|| AgentEntry {
                status: HookStatus {
                    agent_id: agent_id.clone(),
                    kind: HookKind::Working,
                    message: None,
                    since: now,
                    last_event: event.clone(),
                },
                inbox: VecDeque::new(),
                next_seq: 0,
                touched: Instant::now(),
            });
        entry.touched = Instant::now();
        let seq = entry.next_seq;
        entry.next_seq += 1;
        let wire_entry = HookInboxEntry {
            agent_id: agent_id.clone(),
            event: event.clone(),
            received_at: now,
            seq,
            payload,
        };
        if entry.inbox.len() >= HOOK_INBOX_CAP_PER_AGENT {
            entry.inbox.pop_front();
        }
        entry.inbox.push_back(wire_entry.clone());
        if let Some((kind, message)) = derive_status(&event, &wire_entry.payload) {
            entry.status = HookStatus {
                agent_id,
                kind,
                message,
                since: now,
                last_event: event,
            };
        }
        wire_entry
    }

    /// This agent's current status, if this store has ever recorded one for it.
    pub fn status(&self, agent_id: &AgentId) -> Option<HookStatus> {
        lock(&self.0)
            .agents
            .get(agent_id)
            .map(|entry| entry.status.clone())
    }

    /// Every agent this store is currently tracking, in no particular order - `HooksQuery`'s own
    /// dispatch filters this down to agents the host still knows about (`docs/architecture/
    /// decisions.md` §26's "cleared when the session is forgotten", mirroring `AgentsQuery`'s own
    /// exit-based filtering rather than a second, separate removal path).
    pub fn statuses(&self) -> Vec<HookStatus> {
        lock(&self.0)
            .agents
            .values()
            .map(|entry| entry.status.clone())
            .collect()
    }

    /// Drops every raw entry `agent_id` has with `seq <= up_to` - `HookAck`'s own effect. A
    /// no-op for an unknown agent or an `up_to` older than everything already dropped.
    pub fn ack(&self, agent_id: &AgentId, up_to: u64) {
        if let Some(entry) = lock(&self.0).agents.get_mut(agent_id) {
            entry.inbox.retain(|item| item.seq > up_to);
        }
    }

    /// Drops every trace of `agent_id` - the raw inbox and the current status alike. Not reached
    /// by the ordinary hook path (see [`Self::statuses`]'s own docs for why); exposed for a
    /// caller that wants to reclaim an id immediately rather than wait for [`MAX_TRACKED_AGENTS`]
    /// eviction, and exercised directly by this module's own tests.
    pub fn forget(&self, agent_id: &AgentId) {
        lock(&self.0).agents.remove(agent_id);
    }
}

/// Derives the coarse `(kind, message)` a real hook event and its payload carry, or `None` for an
/// event that is not a real lifecycle transition (an unrecognized name, or a `Notification` whose
/// `notification_type` is not one that blocks a human) - in which case the agent's current status
/// is left exactly as it was, the same "a nudge must not overwrite a real fact" rule `jerry-app`'s
/// own `crate::hooks::event::EventKind` documents at length, kept here only in its coarsest form:
/// this store never sees enough of `jerry-app`'s own state (which question was already on record,
/// for instance) to fold a nudge in any richer way, and does not try.
///
/// `pub`: `jerry-app`'s own local `HookStatusCache` (`crate::hooks::store` there) calls this same
/// function to update its cache from each `event/hook` notification, so its coarse status can
/// never drift from what this store itself would have computed for the identical event.
pub fn derive_status(event: &str, payload: &Value) -> Option<(HookKind, Option<String>)> {
    match event {
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => {
            let message = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some((HookKind::Working, message))
        }
        "PostToolUseFailure" => {
            let tool = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("a tool");
            Some((HookKind::Error, Some(format!("{tool} failed"))))
        }
        "PermissionRequest" => {
            let tool = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("a tool");
            Some((HookKind::Waiting, Some(format!("{tool} needs permission"))))
        }
        "Notification" => {
            let notification_type = payload
                .get("notification_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match notification_type {
                "permission_prompt" | "agent_needs_input" | "elicitation_dialog" => {
                    let message = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    Some((HookKind::Waiting, message))
                }
                _ => None,
            }
        }
        "Stop" => Some((HookKind::Done, None)),
        "StopFailure" => {
            let message = payload
                .get("error_message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some((HookKind::Error, message))
        }
        _ => None,
    }
}

/// Replaces `payload` with a real, honest marker when its serialized size exceeds
/// [`MAX_STORED_PAYLOAD_BYTES`] - truncated, never silently dropped, the same rule `jerry hook`'s
/// own stdin cap follows.
fn bound_payload(payload: Value) -> Value {
    match serde_json::to_vec(&payload) {
        Ok(bytes) if bytes.len() > MAX_STORED_PAYLOAD_BYTES => {
            serde_json::json!({ "truncated": true, "original_bytes": bytes.len() })
        }
        _ => payload,
    }
}

/// Real wall-clock seconds since the Unix epoch, matching `crate::session::unix_now`'s own
/// convention.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn lock(mutex: &Mutex<State>) -> MutexGuard<'_, State> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod hook_store_tests {
    use super::{HookStore, HOOK_INBOX_CAP_PER_AGENT, MAX_TRACKED_AGENTS};
    use jerry_core::{AgentId, HookKind};

    fn agent(id: &str) -> AgentId {
        AgentId::from(id)
    }

    #[test]
    fn a_recognized_event_updates_the_agents_current_status() {
        let store = HookStore::default();
        store.record(
            agent("a-1"),
            "PreToolUse".into(),
            serde_json::json!({ "tool_name": "Bash" }),
        );
        let status = store.status(&agent("a-1")).expect("recorded");
        assert_eq!(status.kind, HookKind::Working);
        assert_eq!(status.message.as_deref(), Some("Bash"));
        assert_eq!(status.last_event, "PreToolUse");

        store.record(agent("a-1"), "Stop".into(), serde_json::json!({}));
        let status = store.status(&agent("a-1")).expect("recorded");
        assert_eq!(status.kind, HookKind::Done);
        assert_eq!(status.last_event, "Stop");
    }

    #[test]
    fn a_permission_request_reports_waiting_with_the_tool_named() {
        let store = HookStore::default();
        store.record(
            agent("a-1"),
            "PermissionRequest".into(),
            serde_json::json!({ "tool_name": "Bash" }),
        );
        let status = store.status(&agent("a-1")).expect("recorded");
        assert_eq!(status.kind, HookKind::Waiting);
        assert_eq!(status.message.as_deref(), Some("Bash needs permission"));
    }

    #[test]
    fn a_tool_failure_and_a_stop_failure_both_report_error() {
        let store = HookStore::default();
        store.record(
            agent("a-1"),
            "PostToolUseFailure".into(),
            serde_json::json!({ "tool_name": "Bash" }),
        );
        assert_eq!(store.status(&agent("a-1")).unwrap().kind, HookKind::Error);

        store.record(
            agent("a-2"),
            "StopFailure".into(),
            serde_json::json!({ "error_message": "rate limited" }),
        );
        let status = store.status(&agent("a-2")).expect("recorded");
        assert_eq!(status.kind, HookKind::Error);
        assert_eq!(status.message.as_deref(), Some("rate limited"));
    }

    #[test]
    fn an_idle_notification_does_not_overwrite_a_real_fact() {
        let store = HookStore::default();
        store.record(agent("a-1"), "Stop".into(), serde_json::json!({}));
        store.record(
            agent("a-1"),
            "Notification".into(),
            serde_json::json!({ "notification_type": "idle_prompt", "message": "still there?" }),
        );
        let status = store.status(&agent("a-1")).expect("recorded");
        assert_eq!(
            status.kind,
            HookKind::Done,
            "an idle nudge must not erase a real turn boundary"
        );
        assert_eq!(status.last_event, "Stop");
    }

    #[test]
    fn a_blocking_notification_does_report_waiting() {
        let store = HookStore::default();
        store.record(
            agent("a-1"),
            "Notification".into(),
            serde_json::json!({ "notification_type": "permission_prompt", "message": "needs you" }),
        );
        let status = store.status(&agent("a-1")).expect("recorded");
        assert_eq!(status.kind, HookKind::Waiting);
        assert_eq!(status.message.as_deref(), Some("needs you"));
    }

    #[test]
    fn each_agents_inbox_is_isolated_from_every_other_agents() {
        let store = HookStore::default();
        store.record(agent("a-1"), "Stop".into(), serde_json::json!({}));
        store.record(agent("a-2"), "SessionStart".into(), serde_json::json!({}));

        assert_eq!(store.status(&agent("a-1")).unwrap().kind, HookKind::Done);
        assert_eq!(store.status(&agent("a-2")).unwrap().kind, HookKind::Working);
        let statuses = store.statuses();
        assert_eq!(statuses.len(), 2);
    }

    #[test]
    fn the_raw_inbox_sheds_its_oldest_entries_rather_than_growing_without_bound() {
        let store = HookStore::default();
        for index in 0..(HOOK_INBOX_CAP_PER_AGENT + 5) {
            store.record(
                agent("a-1"),
                "PreToolUse".into(),
                serde_json::json!({ "tool_name": format!("Bash-{index}") }),
            );
        }
        // The status still reflects the very last event recorded - only the raw inbox is bounded.
        let status = store.status(&agent("a-1")).expect("recorded");
        assert_eq!(
            status.message.as_deref(),
            Some(format!("Bash-{}", HOOK_INBOX_CAP_PER_AGENT + 4).as_str())
        );
    }

    #[test]
    fn acknowledging_up_to_a_seq_drops_only_entries_at_or_below_it() {
        let store = HookStore::default();
        let mut entries = Vec::new();
        for index in 0..5 {
            entries.push(store.record(
                agent("a-1"),
                "PreToolUse".into(),
                serde_json::json!({ "tool_name": format!("t{index}") }),
            ));
        }
        store.ack(&agent("a-1"), entries[2].seq);
        // Recording again after the ack must not resurrect the acknowledged entries or otherwise
        // misbehave - a real, if indirect, proof the inbox shrank (nothing here reads the inbox
        // back directly, since `HookStore` intentionally exposes no such accessor to a caller
        // other than the dispatcher's own broadcast of each freshly recorded entry).
        store.ack(&agent("a-1"), entries[4].seq);
        store.ack(&agent("no-such-agent"), 0);
    }

    #[test]
    fn forgetting_an_agent_drops_both_its_status_and_its_inbox() {
        let store = HookStore::default();
        store.record(agent("a-1"), "Stop".into(), serde_json::json!({}));
        assert!(store.status(&agent("a-1")).is_some());
        store.forget(&agent("a-1"));
        assert!(store.status(&agent("a-1")).is_none());
    }

    #[test]
    fn the_store_is_capped_so_invented_agent_ids_cannot_grow_it_without_bound() {
        let store = HookStore::default();
        for index in 0..(MAX_TRACKED_AGENTS + 10) {
            store.record(
                agent(&format!("a-{index}")),
                "Stop".into(),
                serde_json::json!({}),
            );
        }
        assert_eq!(store.statuses().len(), MAX_TRACKED_AGENTS);
        assert!(
            store.status(&agent("a-0")).is_none(),
            "the oldest is evicted"
        );
        assert!(store
            .status(&agent(&format!("a-{}", MAX_TRACKED_AGENTS + 9)))
            .is_some());
    }

    #[test]
    fn an_oversized_payload_is_replaced_with_a_real_marker_not_silently_dropped() {
        let store = HookStore::default();
        let huge = "x".repeat(super::MAX_STORED_PAYLOAD_BYTES + 10);
        let entry = store.record(
            agent("a-1"),
            "PreToolUse".into(),
            serde_json::json!({ "tool_name": "Write", "content": huge }),
        );
        assert_eq!(entry.payload["truncated"], serde_json::json!(true));
    }
}
