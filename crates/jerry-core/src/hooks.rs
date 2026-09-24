//! The wire shape of the host-owned hook store (`docs/architecture/decisions.md` §26): what
//! `jerry-host` learns about each agent from the real `hook` requests it receives. Deliberately
//! coarser than `jerry-app`'s own rendering-tuned parsing (`crate::hooks::event` there, which
//! keeps activity/question/edit/prompt detail split out) - this crate only owns what a `HooksQuery`
//! answers and what `event/hook` now pushes, both real, both bounded, neither invented.

use crate::command::{Command, Denied, Invocability, Locality, Query};
use crate::ctx::{AgentId, Ctx};
use crate::error::Error;
use serde::{Deserialize, Serialize};

/// The coarse lifecycle fact one hook event carries - four variants, the same split `jerry-app`'s
/// own `crate::hooks::event::HookFact` already made for the identical reason (the rail renders
/// five statuses, and several distinct event names are the same fact about the agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HookKind {
    /// Blocked on the human - a permission prompt or an equivalent notification.
    Waiting,
    /// Mid-turn and doing something.
    Working,
    /// The turn ended cleanly.
    Done,
    /// The turn ended badly, or a tool call failed.
    Error,
}

/// One agent's current hook-derived status, as `jerry-host`'s `HookStore` holds it - the same
/// wire entry `HooksQuery` answers with, mirrored across every subscriber by `event/hook`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookStatus {
    pub agent_id: AgentId,
    pub kind: HookKind,
    /// A short human-facing line - the permission question or failure reason when `kind` is
    /// `Waiting`/`Error`, the tool name when `kind` is `Working`, `None` when nothing was.
    pub message: Option<String>,
    /// Real wall-clock seconds since the Unix epoch this status was last set - matches
    /// `SessionRecord::started_at`'s own convention.
    pub since: i64,
    /// The raw hook event name that produced this status, e.g. `"PreToolUse"`.
    pub last_event: String,
}

/// One raw hook event as `jerry-host` recorded it, bounded per agent
/// (`jerry_host::hooks::HOOK_INBOX_CAP_PER_AGENT`). Never interpreted by this crate - `jerry-app`'s
/// own richer parser (`crate::hooks::event::parse`) is the real consumer of `payload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInboxEntry {
    pub agent_id: AgentId,
    pub event: String,
    /// Real wall-clock seconds since the Unix epoch.
    pub received_at: i64,
    /// Monotonic per agent, minted by the host - what `HookAck::up_to` names.
    pub seq: u64,
    /// The hook's own payload; replaced with a real, honest marker rather than silently dropped
    /// when it exceeds `jerry_host::hooks::MAX_STORED_PAYLOAD_BYTES`.
    pub payload: serde_json::Value,
}

/// Asks the connected Jerry for the current hook status of one agent, or every agent it is
/// tracking - `Locality::Session` (§15), answered directly from `jerry-host`'s own `HookStore`,
/// exactly like `AgentsQuery`/`SessionsQuery`. An agent caller may only ask about its own id;
/// `jerry-host`'s dispatcher refuses any other id as `forbidden`, the same way it already refuses
/// a `Hook` request with no agent identity - a data-dependent rule `Invocability` alone cannot
/// express, so this stays `Allowed` and the check lives in the dispatcher.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HooksQuery {
    pub agent: Option<AgentId>,
}

impl Query for HooksQuery {
    type Outcome = Vec<HookStatus>;
    const NAME: &'static str = "hooks";

    fn locality(&self) -> Locality {
        Locality::Session
    }

    /// Never actually reached - see the type's own docs.
    fn run(&self, _ctx: &Ctx) -> Result<Vec<HookStatus>, Error> {
        Err(Error::new(
            "needs-host",
            "the hook store lives on the session host, not in this process",
        ))
    }
}

/// Acknowledges an agent's raw hook inbox up to a real entry, letting `jerry-host` prune what a
/// caller has already durably applied - the one write a client dispatches against the hook store;
/// every other site reads it (`HooksQuery`) or is fed by `event/hook`'s own push. `Locality::
/// Session`, `Invocability::Denied` to agents: an agent's own hooks fire ambiently, it never
/// consumes its own status. Never actually executed through [`Command::execute`] - `jerry-host`'s
/// dispatcher special-cases it, exactly like `SessionSpawn`/`SessionResize`/`SessionKill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HookAck {
    pub agent_id: AgentId,
    /// The highest [`HookInboxEntry::seq`] this caller has durably applied for `agent_id`; every
    /// earlier entry may be dropped.
    pub up_to: u64,
}

impl Command for HookAck {
    type Outcome = ();
    const NAME: &'static str = "hook-ack";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Session
    }

    /// Never reached - see the type's own docs.
    fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
        Ok(())
    }

    /// Never reached - see the type's own docs.
    fn execute(self, _ctx: &Ctx) -> Result<(), Error> {
        Err(Error::new(
            "needs-host",
            "the hook store lives on the session host, not in this process",
        ))
    }
}

#[cfg(test)]
mod hooks_wire_tests {
    use super::{HookInboxEntry, HookKind, HookStatus};
    use crate::AgentId;

    #[test]
    fn a_hook_kind_serializes_kebab_case() {
        for (kind, expected) in [
            (HookKind::Waiting, "\"waiting\""),
            (HookKind::Working, "\"working\""),
            (HookKind::Done, "\"done\""),
            (HookKind::Error, "\"error\""),
        ] {
            assert_eq!(
                serde_json::to_string(&kind).expect("serializable"),
                expected
            );
        }
    }

    #[test]
    fn a_hook_status_round_trips() {
        let status = HookStatus {
            agent_id: AgentId::from("agent-1"),
            kind: HookKind::Waiting,
            message: Some("Bash needs permission: rm -rf /".into()),
            since: 1_700_000_000,
            last_event: "PermissionRequest".into(),
        };
        let value = serde_json::to_value(&status).expect("serializable");
        let back: HookStatus = serde_json::from_value(value).expect("deserializable");
        assert_eq!(back, status);
    }

    #[test]
    fn a_hook_inbox_entry_round_trips_a_bounded_payload() {
        let entry = HookInboxEntry {
            agent_id: AgentId::from("agent-1"),
            event: "PreToolUse".into(),
            received_at: 1_700_000_000,
            seq: 7,
            payload: serde_json::json!({ "tool_name": "Bash" }),
        };
        let value = serde_json::to_value(&entry).expect("serializable");
        let back: HookInboxEntry = serde_json::from_value(value).expect("deserializable");
        assert_eq!(back, entry);
    }
}
