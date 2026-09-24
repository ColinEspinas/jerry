//! The session table's wire shape (`docs/architecture/decisions.md` §23): one [`SessionId`] per
//! PTY `jerry-host` owns or is tracking, agents and plain terminal tabs alike, plus the
//! Session-locality Commands that spawn, resize and kill one. The table itself, and the real
//! `jerry_pty::PtySession` each entry may own, live in `jerry-host` - this crate only owns what
//! travels on the wire, exactly like every other Command/Query here (§15).

use crate::command::{Command, Denied, Invocability, Locality};
use crate::ctx::{AgentId, Ctx};
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// A PTY session's identity for its whole lifetime, minted by whichever host spawned it. Never
/// constructed by a client - a client only ever echoes one back (`SessionResize`/`SessionKill`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionId(pub String);

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        SessionId(value.to_owned())
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        SessionId(value)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What kind of session this is. `Pty` is the only kind today; an ACP session kind is planned
/// (issue #492's plan) but out of scope here - see decisions.md §23.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionKind {
    Pty,
}

/// Which real agent CLI, if any, a session is running - `None` for a plain interactive shell.
/// Mirrors `jerry_host::AgentRecord`'s existing `kind` convention: a free-form label the
/// registering caller chose (`jerry-app`'s own `AgentKind::label()`), never interpreted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionAgentInfo {
    pub kind: String,
    pub agent_id: AgentId,
    /// Wire method names (e.g. `"command/session-send"`) this agent may call beyond what its own
    /// `Invocability` alone allows - `jerry-host`'s dispatcher consults this for an agent caller
    /// whose `Invocability::Denied` would otherwise refuse the call (orchestrator policy,
    /// `docs/architecture/decisions.md` §28). Empty for an ordinary, non-orchestrator agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
    /// The agent id that asked `WorktreeCreate` to spawn this session, if any - who a `Stop`
    /// hook from this agent is forwarded to as `event/child-stopped`
    /// (`docs/architecture/decisions.md` §28). `None` for a session a human spawned directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<AgentId>,
}

/// [`jerry_pty::ExitStatus`], mirrored onto the wire rather than derived directly on it - this
/// crate does not depend on `jerry-pty` at all (§15: PTY machinery is `jerry-host`'s, so a
/// standalone `jerry-cli` never links it), so the `From` conversion lives in `jerry-host`, the
/// one crate that has both types in scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatusWire {
    pub success: bool,
    pub code: u32,
    pub signal: Option<String>,
}

/// One session as the wire sees it: `SessionsQuery`'s own entry shape, and what
/// `event/session-exited` names by [`SessionId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub kind: SessionKind,
    pub worktree: PathBuf,
    pub agent: Option<SessionAgentInfo>,
    /// Real wall-clock seconds since the Unix epoch, matching
    /// `jerry_app::work_surface::agents::Agent::spawned_at_unix`'s own convention.
    pub started_at: i64,
    /// `None` while running, or for an agent-identity registration with no `PtySession` the
    /// host actually owns (decisions.md §23's `AgentTable`-compatibility entries).
    pub exit: Option<ExitStatusWire>,
    /// The real spawned process's own pid, `None` for an `AgentTable`-compatibility registration
    /// with no process at all. `crate::terminal::socket_adapter::SocketSessionAdapter`'s own
    /// `process_id()` (`docs/architecture/decisions.md` §24) resolves this from a `SessionsQuery`
    /// rather than a control-plane round trip of its own, since a session's pid never changes
    /// once spawned - set once, at spawn time, and left as-is once the session exits.
    pub process_id: Option<u32>,
    /// This session's real pty size - set at spawn time, updated on every real `SessionResize`
    /// dispatched against it (`SessionManager::resize`, never through `Command::execute` - see
    /// `SessionSpawn`'s own docs). `0` for an `AgentTable`-compatibility registration with no pty
    /// to size at all. What a socket-attached (out-of-process) `TerminalPane`'s own resize
    /// dispatch has to verify against, since it has no in-process `PtySession` to read the applied
    /// size back from directly (`docs/architecture/decisions.md` §24's amendment).
    pub rows: u16,
    pub cols: u16,
}

/// Spawns a new PTY session in the caller's worktree, owned by the host from then on
/// (decisions.md §23). `Locality::Session`, `Invocability::Denied` to agents: an agent already
/// has a real pty of its own from whatever spawned *it*, so it has no legitimate use for asking
/// Jerry to open another one on its behalf, at least for now.
///
/// Like every other Session-locality request, this is never actually executed through
/// [`Command::execute`] below - `jerry-host`'s dispatcher special-cases it and calls into its own
/// `SessionManager` directly, exactly as it already does for `AgentsQuery` (§15). The trait impl
/// exists only so this type satisfies `Command` for cataloguing (`AppCommand`, the MCP tool
/// list, the wire fixtures).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSpawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    /// Associates the new session with an agent identity atomically at spawn time, so there is
    /// never a window in which a hook from that agent id arrives before its worktree confinement
    /// is in place - the caller (`jerry-app`'s own `Agents::spawn_resolved`) already minted this
    /// id before spawning, since `JERRY_AGENT_ID` must reach the child's own environment. `None`
    /// for a plain interactive shell, which carries no agent identity at all.
    #[serde(default)]
    pub agent: Option<SessionAgentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpawnOutcome {
    pub id: SessionId,
}

impl Command for SessionSpawn {
    type Outcome = SessionSpawnOutcome;
    const NAME: &'static str = "session-spawn";

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
    fn execute(self, _ctx: &Ctx) -> Result<SessionSpawnOutcome, Error> {
        Err(Error::new(
            "needs-host",
            "the session table lives on the session host, not in this process",
        ))
    }
}

/// Resizes a live session's pty. `Locality::Session`, `Invocability::Denied` to agents (an agent
/// resizes its own real terminal, not one Jerry is holding for someone else). Never actually
/// executed through [`Command::execute`] - see [`SessionSpawn`]'s docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionResize {
    pub id: SessionId,
    pub rows: u16,
    pub cols: u16,
}

impl Command for SessionResize {
    type Outcome = ();
    const NAME: &'static str = "session-resize";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Session
    }

    fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
        Ok(())
    }

    fn execute(self, _ctx: &Ctx) -> Result<(), Error> {
        Err(Error::new(
            "needs-host",
            "the session table lives on the session host, not in this process",
        ))
    }
}

/// Kills a live session's process tree. `Locality::Session`, `Invocability::Denied` to agents.
/// Never actually executed through [`Command::execute`] - see [`SessionSpawn`]'s docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionKill {
    pub id: SessionId,
}

impl Command for SessionKill {
    type Outcome = ();
    const NAME: &'static str = "session-kill";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Session
    }

    fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
        Ok(())
    }

    fn execute(self, _ctx: &Ctx) -> Result<(), Error> {
        Err(Error::new(
            "needs-host",
            "the session table lives on the session host, not in this process",
        ))
    }
}

/// Attaches to a live session's data plane (`docs/architecture/decisions.md` §25's reattach
/// cutover): answers with the path to a per-session socket that, from the moment a client
/// connects, carries raw bytes both ways - the host's own `PtyOutput::Bytes` payloads verbatim in
/// order out, input bytes in - continuing from the [`SessionAttachOutcome::snapshot`] point, plus
/// that snapshot itself. Never `PtyOutput`-framed on the wire: bytes are not a control-plane
/// concern (§23). `Locality::Session`, `Invocability::Denied` to agents (an agent already owns
/// its own real pty). Never actually executed through [`Command::execute`] - see [`SessionSpawn`]'s
/// docs. Exactly one attach at a time per session: a second call while one is live answers
/// `Report::Denied` with code `session-already-attached`, handled directly by `jerry-host`'s
/// dispatcher rather than through this trait impl, exactly like
/// `SessionSpawn`/`SessionResize`/`SessionKill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionAttach {
    pub id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachOutcome {
    pub socket: PathBuf,
    /// A structured rendering of the session's own headless grid at the moment of attach - what
    /// a reattaching pane paints *before* the first byte off `socket` ever arrives (§25). Never a
    /// raw byte replay (the pre-attach buffer §24 introduced is gone): a byte stream replayed
    /// after a resize renders at the *old* size until the child reacts, which is exactly the
    /// artifact both `zed`'s and warp's own reattach implementations independently ran into and
    /// abandoned - see this type's own docs for why a snapshot sidesteps it entirely.
    pub snapshot: SessionSnapshot,
}

/// One cell of a [`SessionSnapshot`] - the wire twin of `jerry_term::grid::GridCell`, minus
/// `selected` (meaningless for a snapshot with no live UI selection behind it). `jerry-core` does
/// not depend on `jerry-term`/`alacritty_terminal` at all (§4: no threads, no rendering-adjacent
/// dependencies in the wire-contract crate) - `jerry-host` converts field-by-field from its own
/// `jerry_term::grid::GridCell` when answering a real attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCell {
    pub c: char,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub width: SnapshotCellWidth,
}

/// The wire twin of `jerry_term::grid::CellWidth` - see [`SnapshotCell`]'s own docs for why this
/// crate keeps its own copy rather than depending on `jerry-term` for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotCellWidth {
    Narrow,
    Wide,
    Spacer,
}

/// A structured, palette-resolved rendering of a session's own headless grid, carried by
/// [`SessionAttachOutcome`] (`docs/architecture/decisions.md` §25). `cells` is exactly
/// `rows * cols` cells (`cells[r][c]`, row-major); `scrollback` is real retained history above
/// the visible screen, oldest first, bounded by `jerry-host`'s own named cap - never the whole
/// history a long-lived session may have accumulated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub rows: u16,
    pub cols: u16,
    /// The cursor's own `(row, col)` within `cells` - `None` when the child process has hidden it.
    pub cursor: Option<(u16, u16)>,
    pub cells: Vec<Vec<SnapshotCell>>,
    pub scrollback: Vec<Vec<SnapshotCell>>,
}

impl Command for SessionAttach {
    type Outcome = SessionAttachOutcome;
    const NAME: &'static str = "session-attach";

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
    fn execute(self, _ctx: &Ctx) -> Result<SessionAttachOutcome, Error> {
        Err(Error::new(
            "needs-host",
            "the session table lives on the session host, not in this process",
        ))
    }
}

/// Wakes the human working on this repository (`docs/architecture/decisions.md` §28): the host
/// records it against the calling agent's own live session and publishes `event/attention
/// { session_id, agent_id, message }`, so the app can raise the same attention signal it already
/// shows for an OSC 9/777 ping, keyed to that agent's own tab. `Locality::Session` (meaningless
/// without a live session to record it against), `Invocability::Allowed` - this exists for an
/// agent to call. Never actually executed through [`Command::execute`] below -
/// `jerry-host`'s dispatcher special-cases it, since only it can resolve the caller's own agent
/// identity to a live session id (see [`SessionSpawn`]'s own docs for why this shape recurs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttentionRaise {
    pub message: String,
}

impl Command for AttentionRaise {
    type Outcome = ();
    const NAME: &'static str = "attention-raise";

    fn invocability(&self) -> Invocability {
        Invocability::Allowed
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
            "the session table lives on the session host, not in this process",
        ))
    }
}

fn default_submit() -> bool {
    true
}

/// Writes to another live session's stdin over the control plane (`docs/architecture/decisions.md`
/// §28): `text`, plus a trailing Enter when `submit` (the default - `--no-submit` on the CLI
/// leaves the text sitting unentered, e.g. to let a human finish it by hand). `Locality::Session`,
/// `Invocability::Denied` by default - opened per agent through `SessionAgentInfo::grants`
/// (`"command/session-send"`), never a blanket `Allowed`, since this is one live session directly
/// driving another's input. Never actually executed through [`Command::execute`] below -
/// `jerry-host`'s dispatcher special-cases it: refusing a caller sending to its own session and
/// checking the target is a real, live session both need the session table this trait impl has no
/// access to (see [`SessionSpawn`]'s own docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSend {
    pub to: SessionId,
    pub text: String,
    #[serde(default = "default_submit")]
    pub submit: bool,
}

impl Command for SessionSend {
    type Outcome = ();
    const NAME: &'static str = "session-send";

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
            "the session table lives on the session host, not in this process",
        ))
    }
}

/// Whether a child's *next* `Stop` hook should be blocked (kept going) or let through -
/// [`SessionStopPolicy`]'s own decision, and the `hook` reply's own outcome for a `Stop` event
/// once one is registered (`docs/architecture/decisions.md` §28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StopDecision {
    Stop,
    Continue,
}

/// Pre-registers this orchestrator's own decision for the *next* `Stop` hook `child` fires
/// (`docs/architecture/decisions.md` §28) - consumed exactly once, so a policy left registered
/// after the `Stop` it was meant for does not also apply to the one after that.
/// `Locality::Session`, `Invocability::Denied` by default - opened per agent through
/// `SessionAgentInfo::grants` (`"command/session-stop-policy"`), the same as [`SessionSend`].
/// Never actually executed through [`Command::execute`] below - `jerry-host`'s dispatcher
/// special-cases it, storing the policy directly rather than through the session table this trait
/// impl has no access to (see [`SessionSpawn`]'s own docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionStopPolicy {
    /// The agent id whose next `Stop` this decision applies to - not a [`SessionId`], since the
    /// forwarding this answers (`event/child-stopped`) is keyed by agent identity, the same way
    /// `SessionAgentInfo::parent` records it.
    pub child: AgentId,
    pub decision: StopDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Command for SessionStopPolicy {
    type Outcome = ();
    const NAME: &'static str = "session-stop-policy";

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
            "the session table lives on the session host, not in this process",
        ))
    }
}

#[cfg(test)]
mod session_id_tests {
    use super::SessionId;

    #[test]
    fn a_session_id_displays_as_its_own_bare_string() {
        assert_eq!(SessionId::from("session-3").to_string(), "session-3");
        assert_eq!(SessionId::from("session-3".to_owned()).0, "session-3");
    }
}

#[cfg(test)]
mod orchestrator_policy_tests {
    use super::{
        AttentionRaise, SessionAgentInfo, SessionId, SessionSend, SessionStopPolicy, StopDecision,
    };
    use crate::command::{Command, Invocability, Locality};
    use crate::ctx::AgentId;

    #[test]
    fn attention_raise_is_session_locality_and_allowed_to_agents() {
        let command = AttentionRaise {
            message: "hi".into(),
        };
        assert_eq!(command.invocability(), Invocability::Allowed);
        assert_eq!(command.locality(), Locality::Session);
    }

    #[test]
    fn session_send_and_session_stop_policy_are_denied_by_default() {
        let send = SessionSend {
            to: SessionId::from("session-1"),
            text: "y".into(),
            submit: true,
        };
        assert_eq!(send.invocability(), Invocability::Denied);
        assert_eq!(send.locality(), Locality::Session);

        let policy = SessionStopPolicy {
            child: AgentId::from("agent-1"),
            decision: StopDecision::Stop,
            reason: None,
        };
        assert_eq!(policy.invocability(), Invocability::Denied);
        assert_eq!(policy.locality(), Locality::Session);
    }

    /// `submit` defaults to `true` when omitted - a hand-typed `{"to": ..., "text": ...}` (no
    /// `submit` key at all) still sends the trailing Enter, matching the CLI's own default.
    #[test]
    fn session_send_submit_defaults_to_true() {
        let decoded: SessionSend =
            serde_json::from_value(serde_json::json!({ "to": "session-1", "text": "y" }))
                .expect("decodes with submit omitted");
        assert!(decoded.submit);
    }

    /// Empty `grants`/absent `parent` are omitted from the wire entirely - an ordinary agent's
    /// `SessionAgentInfo` serializes exactly as it did before this issue, and a payload with
    /// neither key still decodes.
    #[test]
    fn an_ordinary_agents_grants_and_parent_are_invisible_on_the_wire() {
        let ordinary = SessionAgentInfo {
            kind: "Claude".into(),
            agent_id: AgentId::from("agent-1"),
            grants: Vec::new(),
            parent: None,
        };
        let value = serde_json::to_value(&ordinary).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({ "kind": "Claude", "agent_id": "agent-1" })
        );
        let decoded: SessionAgentInfo =
            serde_json::from_value(serde_json::json!({ "kind": "Claude", "agent_id": "agent-1" }))
                .expect("decodes with neither key present");
        assert_eq!(decoded, ordinary);
    }

    #[test]
    fn an_orchestrators_grants_and_parent_round_trip() {
        let orchestrator = SessionAgentInfo {
            kind: "Claude".into(),
            agent_id: AgentId::from("agent-2"),
            grants: vec!["command/session-send".into()],
            parent: Some(AgentId::from("agent-1")),
        };
        let value = serde_json::to_value(&orchestrator).expect("serializes");
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "Claude",
                "agent_id": "agent-2",
                "grants": ["command/session-send"],
                "parent": "agent-1",
            })
        );
        let decoded: SessionAgentInfo = serde_json::from_value(value).expect("decodes");
        assert_eq!(decoded, orchestrator);
    }
}
