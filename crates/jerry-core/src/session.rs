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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAgentInfo {
    pub kind: String,
    pub agent_id: AgentId,
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

#[cfg(test)]
mod session_id_tests {
    use super::SessionId;

    #[test]
    fn a_session_id_displays_as_its_own_bare_string() {
        assert_eq!(SessionId::from("session-3").to_string(), "session-3");
        assert_eq!(SessionId::from("session-3".to_owned()).0, "session-3");
    }
}
