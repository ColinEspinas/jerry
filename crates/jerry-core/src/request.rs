//! The exhaustive catalogue of what a client can ask: every Command, every Query, and the hook
//! event. One enum per kind so dispatch applies each kind's default policy without inspecting
//! variants, and so a CLI or an MCP tool list can be generated from the same source.

use crate::command::{
    permits, run_command, run_query, schema_of, validate_command, Command, Invocability, Locality,
    Query,
};
use crate::commands::{
    AgentSpec, AmendHeadMessage, MergeAbort, MergeAttempt, MergeBranchIntoCurrent, MergeComplete,
    RebaseAbort, RebaseActionWire, RebaseContinue, RebasePlanEntryWire, RebaseSkip, RebaseStart,
    StageResolved, WorktreeCreate,
};
use crate::ctx::Ctx;
use crate::method::Method;
use crate::queries::{
    AgentsQuery, MergeStatusQuery, RebaseStatusQuery, SessionsQuery, StatusQuery,
};
use crate::report::Report;
use crate::session::{SessionId, SessionKill, SessionResize, SessionSpawn};
use crate::wire::{rpc_code, RpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One agent hook event, forwarded verbatim by `jerry hook <event>`. The agent's identity
/// travels in the call envelope, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    /// The agent CLI's own event name, e.g. `PreToolUse`.
    pub event: String,
    /// The hook's stdin, parsed. Never interpreted here.
    pub payload: Value,
}

/// Every Command a client can send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "params", rename_all = "kebab-case")]
pub enum AppCommand {
    MergeAttempt(MergeAttempt),
    MergeBranchIntoCurrent(MergeBranchIntoCurrent),
    MergeComplete(MergeComplete),
    MergeAbort(MergeAbort),
    StageResolved(StageResolved),
    RebaseStart(RebaseStart),
    RebaseContinue(RebaseContinue),
    RebaseSkip(RebaseSkip),
    RebaseAbort(RebaseAbort),
    AmendHeadMessage(AmendHeadMessage),
    WorktreeCreate(WorktreeCreate),
    SessionSpawn(SessionSpawn),
    SessionResize(SessionResize),
    SessionKill(SessionKill),
}

/// Every Query a client can send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "params", rename_all = "kebab-case")]
pub enum AppQuery {
    Status(StatusQuery),
    MergeStatus(MergeStatusQuery),
    RebaseStatus(RebaseStatusQuery),
    Agents(AgentsQuery),
    Sessions(SessionsQuery),
}

/// The kebab-case names of every `AppCommand` variant, for method lookup and tool listing.
pub const COMMAND_NAMES: &[&str] = &[
    "amend-head-message",
    "merge-abort",
    "merge-attempt",
    "merge-branch-into-current",
    "merge-complete",
    "rebase-abort",
    "rebase-continue",
    "rebase-skip",
    "rebase-start",
    "session-kill",
    "session-resize",
    "session-spawn",
    "stage-resolved",
    "worktree-create",
];

/// The kebab-case names of every `AppQuery` variant.
pub const QUERY_NAMES: &[&str] = &[
    "agents",
    "merge-status",
    "rebase-status",
    "sessions",
    "status",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Hook(HookEvent),
    Command(AppCommand),
    Validate(AppCommand),
    Query(AppQuery),
}

/// Why a request could not run in this process.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocalDispatchError {
    /// A Session-locality request reached a process that has no session table.
    #[error("{0} needs a running host")]
    NeedsHost(Method),
    /// The caller may not ask for this (`Invocability`).
    #[error("{0} is not invocable by this caller")]
    Forbidden(Method),
}

/// One match per property, so adding a variant is a compile error until it is classified.
macro_rules! each_command {
    ($self:expr, |$c:ident| $body:expr) => {
        match $self {
            AppCommand::MergeAttempt($c) => $body,
            AppCommand::MergeBranchIntoCurrent($c) => $body,
            AppCommand::MergeComplete($c) => $body,
            AppCommand::MergeAbort($c) => $body,
            AppCommand::StageResolved($c) => $body,
            AppCommand::RebaseStart($c) => $body,
            AppCommand::RebaseContinue($c) => $body,
            AppCommand::RebaseSkip($c) => $body,
            AppCommand::RebaseAbort($c) => $body,
            AppCommand::AmendHeadMessage($c) => $body,
            AppCommand::WorktreeCreate($c) => $body,
            AppCommand::SessionSpawn($c) => $body,
            AppCommand::SessionResize($c) => $body,
            AppCommand::SessionKill($c) => $body,
        }
    };
}

impl AppCommand {
    fn name(&self) -> &'static str {
        match self {
            AppCommand::MergeAttempt(_) => MergeAttempt::NAME,
            AppCommand::MergeBranchIntoCurrent(_) => MergeBranchIntoCurrent::NAME,
            AppCommand::MergeComplete(_) => MergeComplete::NAME,
            AppCommand::MergeAbort(_) => MergeAbort::NAME,
            AppCommand::StageResolved(_) => StageResolved::NAME,
            AppCommand::RebaseStart(_) => RebaseStart::NAME,
            AppCommand::RebaseContinue(_) => RebaseContinue::NAME,
            AppCommand::RebaseSkip(_) => RebaseSkip::NAME,
            AppCommand::RebaseAbort(_) => RebaseAbort::NAME,
            AppCommand::AmendHeadMessage(_) => AmendHeadMessage::NAME,
            AppCommand::WorktreeCreate(_) => WorktreeCreate::NAME,
            AppCommand::SessionSpawn(_) => SessionSpawn::NAME,
            AppCommand::SessionResize(_) => SessionResize::NAME,
            AppCommand::SessionKill(_) => SessionKill::NAME,
        }
    }

    pub fn invocability(&self) -> Invocability {
        each_command!(self, |c| c.invocability())
    }

    pub fn locality(&self) -> Locality {
        each_command!(self, |c| c.locality())
    }

    /// One line describing what this variant does - the MCP tool description
    /// (`docs/architecture/decisions.md` §22). One match per property, like [`Self::name`]: a new
    /// variant is a compile error here until it is given one.
    pub fn description(&self) -> &'static str {
        match self {
            AppCommand::MergeAttempt(_) => {
                "Merge the caller's worktree branch into the repository's detected base branch."
            }
            AppCommand::MergeBranchIntoCurrent(_) => {
                "Merge a named branch into whatever the caller's worktree has checked out."
            }
            AppCommand::MergeComplete(_) => {
                "Commit the merge in progress at a base worktree, once nothing is unmerged."
            }
            AppCommand::MergeAbort(_) => {
                "Abort the merge in progress at a base worktree, restoring it."
            }
            AppCommand::StageResolved(_) => "Stage one file whose conflict markers are all gone.",
            AppCommand::RebaseStart(_) => {
                "Start a real interactive rebase, driving the given plan to completion or the \
                 first stop."
            }
            AppCommand::RebaseContinue(_) => {
                "Resume a stopped rebase, driving it to completion or the next stop."
            }
            AppCommand::RebaseSkip(_) => {
                "Skip the commit a stopped rebase is at, driving it to completion or the next \
                 stop."
            }
            AppCommand::RebaseAbort(_) => {
                "Abort an in-progress rebase, restoring the pre-rebase state."
            }
            AppCommand::AmendHeadMessage(_) => {
                "Amend the message of the commit a rebase is stopped at."
            }
            AppCommand::WorktreeCreate(_) => {
                "Create a sibling git worktree on a new branch, optionally starting an agent in \
                 it."
            }
            AppCommand::SessionSpawn(_) => "Spawn a new PTY session, owned by this host.",
            AppCommand::SessionResize(_) => "Resize a live session's pty.",
            AppCommand::SessionKill(_) => "Kill a live session's process tree.",
        }
    }

    /// The JSON Schema for this variant's own input struct, for the MCP `tools/list`
    /// `inputSchema` (`docs/architecture/decisions.md` §22).
    pub fn input_schema(&self) -> Value {
        match self {
            AppCommand::MergeAttempt(_) => schema_of::<MergeAttempt>(),
            AppCommand::MergeBranchIntoCurrent(_) => schema_of::<MergeBranchIntoCurrent>(),
            AppCommand::MergeComplete(_) => schema_of::<MergeComplete>(),
            AppCommand::MergeAbort(_) => schema_of::<MergeAbort>(),
            AppCommand::StageResolved(_) => schema_of::<StageResolved>(),
            AppCommand::RebaseStart(_) => schema_of::<RebaseStart>(),
            AppCommand::RebaseContinue(_) => schema_of::<RebaseContinue>(),
            AppCommand::RebaseSkip(_) => schema_of::<RebaseSkip>(),
            AppCommand::RebaseAbort(_) => schema_of::<RebaseAbort>(),
            AppCommand::AmendHeadMessage(_) => schema_of::<AmendHeadMessage>(),
            AppCommand::WorktreeCreate(_) => schema_of::<WorktreeCreate>(),
            AppCommand::SessionSpawn(_) => schema_of::<SessionSpawn>(),
            AppCommand::SessionResize(_) => schema_of::<SessionResize>(),
            AppCommand::SessionKill(_) => schema_of::<SessionKill>(),
        }
    }

    /// The one execution path: validate, then execute, as a `Report`.
    pub fn run(self, ctx: &Ctx) -> Report {
        each_command!(self, |c| run_command(c, ctx))
    }

    pub fn validate(&self, ctx: &Ctx) -> Report {
        each_command!(self, |c| validate_command(c, ctx))
    }
}

macro_rules! each_query {
    ($self:expr, |$q:ident| $body:expr) => {
        match $self {
            AppQuery::Status($q) => $body,
            AppQuery::MergeStatus($q) => $body,
            AppQuery::RebaseStatus($q) => $body,
            AppQuery::Agents($q) => $body,
            AppQuery::Sessions($q) => $body,
        }
    };
}

impl AppQuery {
    fn name(&self) -> &'static str {
        match self {
            AppQuery::Status(_) => StatusQuery::NAME,
            AppQuery::MergeStatus(_) => MergeStatusQuery::NAME,
            AppQuery::RebaseStatus(_) => RebaseStatusQuery::NAME,
            AppQuery::Agents(_) => AgentsQuery::NAME,
            AppQuery::Sessions(_) => SessionsQuery::NAME,
        }
    }

    pub fn invocability(&self) -> Invocability {
        each_query!(self, |q| q.invocability())
    }

    pub fn locality(&self) -> Locality {
        each_query!(self, |q| q.locality())
    }

    /// One line describing what this variant reports - the MCP tool description
    /// (`docs/architecture/decisions.md` §22).
    pub fn description(&self) -> &'static str {
        match self {
            AppQuery::Status(_) => {
                "Report the worktree, repository, and caller identity the request resolved to."
            }
            AppQuery::MergeStatus(_) => {
                "Report whether a merge is in progress and what remains unmerged."
            }
            AppQuery::RebaseStatus(_) => {
                "Report whether a rebase is stopped in this worktree, and where."
            }
            AppQuery::Agents(_) => "List every agent the connected Jerry is currently supervising.",
            AppQuery::Sessions(_) => {
                "List every PTY session the connected Jerry is currently tracking, agents and \
                 plain terminal tabs alike."
            }
        }
    }

    /// The JSON Schema for this variant's own input struct, for the MCP `tools/list`
    /// `inputSchema` (`docs/architecture/decisions.md` §22).
    pub fn input_schema(&self) -> Value {
        match self {
            AppQuery::Status(_) => schema_of::<StatusQuery>(),
            AppQuery::MergeStatus(_) => schema_of::<MergeStatusQuery>(),
            AppQuery::RebaseStatus(_) => schema_of::<RebaseStatusQuery>(),
            AppQuery::Agents(_) => schema_of::<AgentsQuery>(),
            AppQuery::Sessions(_) => schema_of::<SessionsQuery>(),
        }
    }

    pub fn run(&self, ctx: &Ctx) -> Report {
        each_query!(self, |q| run_query(q, ctx))
    }
}

impl Request {
    pub fn method(&self) -> Method {
        match self {
            Request::Hook(_) => Method::Hook,
            Request::Command(command) => Method::Command(command.name().to_owned()),
            Request::Validate(command) => Method::Validate(command.name().to_owned()),
            Request::Query(query) => Method::Query(query.name().to_owned()),
        }
    }

    /// The payload alone: the name is the method's and the caller is the envelope's.
    pub fn params(&self) -> Result<Value, serde_json::Error> {
        match self {
            Request::Hook(event) => serde_json::to_value(event),
            Request::Command(command) | Request::Validate(command) => {
                named_params(serde_json::to_value(command)?)
            }
            Request::Query(query) => named_params(serde_json::to_value(query)?),
        }
    }

    /// Classifies an incoming method and its payload. Unknown methods and unknown names are
    /// `METHOD_NOT_FOUND`; a known name with the wrong payload is `INVALID_PARAMS`.
    pub fn from_wire(method: &str, params: Value) -> Result<Request, RpcError> {
        let parsed = Method::parse(method).ok_or_else(|| method_not_found(method))?;
        match parsed {
            Method::Hook => serde_json::from_value(params)
                .map(Request::Hook)
                .map_err(|error| invalid_params(method, &error)),
            Method::Command(name) => Ok(Request::Command(decode_named(
                method,
                &name,
                params,
                COMMAND_NAMES,
            )?)),
            Method::Validate(name) => Ok(Request::Validate(decode_named(
                method,
                &name,
                params,
                COMMAND_NAMES,
            )?)),
            Method::Query(name) => Ok(Request::Query(decode_named(
                method,
                &name,
                params,
                QUERY_NAMES,
            )?)),
            Method::Event(_) => Err(method_not_found(method)),
        }
    }

    pub fn locality(&self) -> Locality {
        match self {
            Request::Hook(_) => Locality::Session,
            Request::Command(command) | Request::Validate(command) => command.locality(),
            Request::Query(query) => query.locality(),
        }
    }

    pub fn invocability(&self) -> Invocability {
        match self {
            Request::Hook(_) => Invocability::Allowed,
            Request::Command(command) | Request::Validate(command) => command.invocability(),
            Request::Query(query) => query.invocability(),
        }
    }

    /// One instance per variant, named for its fixture; `Call::examples` wraps these.
    pub fn examples() -> Vec<(&'static str, Request)> {
        vec![
            (
                "request-hook",
                Request::Hook(HookEvent {
                    event: "PreToolUse".into(),
                    payload: serde_json::json!({ "tool_name": "Edit" }),
                }),
            ),
            (
                "request-query-status",
                Request::Query(AppQuery::Status(StatusQuery::default())),
            ),
            (
                "request-query-merge-status",
                Request::Query(AppQuery::MergeStatus(MergeStatusQuery::default())),
            ),
            (
                "request-command-merge-attempt",
                Request::Command(AppCommand::MergeAttempt(MergeAttempt::default())),
            ),
            (
                "request-validate-merge-attempt",
                Request::Validate(AppCommand::MergeAttempt(MergeAttempt::default())),
            ),
            (
                "request-command-merge-branch-into-current",
                Request::Command(AppCommand::MergeBranchIntoCurrent(MergeBranchIntoCurrent {
                    source_branch: "feature/login".into(),
                })),
            ),
            (
                "request-command-merge-complete",
                Request::Command(AppCommand::MergeComplete(MergeComplete {
                    base_worktree_path: "/repo".into(),
                })),
            ),
            (
                "request-command-merge-abort",
                Request::Command(AppCommand::MergeAbort(MergeAbort {
                    base_worktree_path: "/repo".into(),
                })),
            ),
            (
                "request-command-stage-resolved",
                Request::Command(AppCommand::StageResolved(StageResolved {
                    worktree_path: "/repo".into(),
                    path: "src/a.rs".into(),
                })),
            ),
            (
                "request-query-rebase-status",
                Request::Query(AppQuery::RebaseStatus(RebaseStatusQuery::default())),
            ),
            (
                "request-command-rebase-start",
                Request::Command(AppCommand::RebaseStart(RebaseStart {
                    onto: "0123456789abcdef0123456789abcdef01234567".into(),
                    plan: vec![RebasePlanEntryWire {
                        commit: "89abcdef0123456789abcdef0123456789abcdef".into(),
                        action: RebaseActionWire::Pick,
                    }],
                })),
            ),
            (
                "request-command-rebase-continue",
                Request::Command(AppCommand::RebaseContinue(RebaseContinue::default())),
            ),
            (
                "request-command-rebase-skip",
                Request::Command(AppCommand::RebaseSkip(RebaseSkip::default())),
            ),
            (
                "request-command-rebase-abort",
                Request::Command(AppCommand::RebaseAbort(RebaseAbort::default())),
            ),
            (
                "request-command-amend-head-message",
                Request::Command(AppCommand::AmendHeadMessage(AmendHeadMessage {
                    message: "a real new message".into(),
                })),
            ),
            (
                "request-command-worktree-create",
                Request::Command(AppCommand::WorktreeCreate(WorktreeCreate {
                    branch: "feature/login".into(),
                    from: None,
                    agent: Some(AgentSpec::Claude),
                    prompt: Some("fix the login bug".into()),
                })),
            ),
            (
                "request-query-agents",
                Request::Query(AppQuery::Agents(AgentsQuery::default())),
            ),
            (
                "request-command-session-spawn",
                Request::Command(AppCommand::SessionSpawn(SessionSpawn {
                    program: "bash".into(),
                    args: Vec::new(),
                    env: Vec::new(),
                    rows: 24,
                    cols: 80,
                })),
            ),
            (
                "request-command-session-resize",
                Request::Command(AppCommand::SessionResize(SessionResize {
                    id: SessionId::from("session-1"),
                    rows: 40,
                    cols: 120,
                })),
            ),
            (
                "request-command-session-kill",
                Request::Command(AppCommand::SessionKill(SessionKill {
                    id: SessionId::from("session-1"),
                })),
            ),
            (
                "request-query-sessions",
                Request::Query(AppQuery::Sessions(SessionsQuery::default())),
            ),
        ]
    }
}

/// Runs a Git-locality request in this process. Anything needing the session table, hooks
/// included, is handed back as `NeedsHost` so the caller can forward it or fail with exit 4.
pub fn execute_locally(request: &Request, ctx: &Ctx) -> Result<Report, LocalDispatchError> {
    if !permits(&ctx.caller, request.invocability()) {
        return Err(LocalDispatchError::Forbidden(request.method()));
    }
    if request.locality() == Locality::Session {
        return Err(LocalDispatchError::NeedsHost(request.method()));
    }
    match request {
        Request::Hook(_) => Err(LocalDispatchError::NeedsHost(request.method())),
        Request::Command(command) => Ok(command.clone().run(ctx)),
        Request::Validate(command) => Ok(command.validate(ctx)),
        Request::Query(query) => Ok(query.run(ctx)),
    }
}

fn named_params(value: Value) -> Result<Value, serde_json::Error> {
    match value {
        Value::Object(mut fields) => Ok(fields.remove("params").unwrap_or(Value::Null)),
        other => Ok(other),
    }
}

fn decode_named<T: for<'de> Deserialize<'de>>(
    method: &str,
    name: &str,
    params: Value,
    known: &[&str],
) -> Result<T, RpcError> {
    if !known.contains(&name) {
        return Err(method_not_found(method));
    }
    let tagged = serde_json::json!({ "name": name, "params": params });
    serde_json::from_value(tagged).map_err(|error| invalid_params(method, &error))
}

fn method_not_found(method: &str) -> RpcError {
    RpcError::new(
        rpc_code::METHOD_NOT_FOUND,
        format!("no such method: {method}"),
    )
    .with_data(serde_json::json!({ "method": method }))
}

fn invalid_params(method: &str, error: &serde_json::Error) -> RpcError {
    RpcError::new(
        rpc_code::INVALID_PARAMS,
        format!("invalid params for {method}: {error}"),
    )
    .with_data(serde_json::json!({ "method": method }))
}

#[cfg(test)]
mod request_catalogue_tests {
    use super::{
        execute_locally, AppQuery, LocalDispatchError, Request, COMMAND_NAMES, QUERY_NAMES,
    };
    use crate::command::Locality;
    use crate::ctx::{Caller, Ctx};
    use crate::method::Method;
    use crate::wire::rpc_code;
    use std::path::PathBuf;

    #[test]
    fn the_name_lists_cover_exactly_the_catalogue() {
        let mut queries: Vec<String> = Vec::new();
        let mut commands: Vec<String> = Vec::new();
        for (_, request) in Request::examples() {
            match request.method() {
                Method::Query(name) => queries.push(name),
                Method::Command(name) | Method::Validate(name) => commands.push(name),
                Method::Hook | Method::Event(_) => {}
            }
        }
        queries.sort_unstable();
        queries.dedup();
        commands.sort_unstable();
        commands.dedup();
        assert_eq!(queries, QUERY_NAMES);
        assert_eq!(commands, COMMAND_NAMES);
    }

    #[test]
    fn unknown_methods_and_bad_params_get_distinct_rpc_codes() {
        let unknown = Request::from_wire("query/nope", serde_json::Value::Null).unwrap_err();
        assert_eq!(unknown.code, rpc_code::METHOD_NOT_FOUND);
        let event = Request::from_wire("event/anything", serde_json::Value::Null).unwrap_err();
        assert_eq!(event.code, rpc_code::METHOD_NOT_FOUND);
        let bad = Request::from_wire("hook", serde_json::json!({ "event": 1 })).unwrap_err();
        assert_eq!(bad.code, rpc_code::INVALID_PARAMS);
    }

    #[test]
    fn a_hook_is_session_bound_and_never_runs_locally() {
        let (_, hook) = Request::examples().remove(0);
        assert_eq!(hook.locality(), Locality::Session);
        let ctx = Ctx {
            repo_path: PathBuf::from("/r/.git"),
            worktree_path: PathBuf::from("/r"),
            caller: Caller::Human,
        };
        assert_eq!(
            execute_locally(&hook, &ctx),
            Err(LocalDispatchError::NeedsHost(Method::Hook))
        );
    }

    #[test]
    fn a_git_query_runs_locally_against_a_real_repository() {
        let repo = test_support::seed_empty_repo();
        let ctx = Ctx::from_cwd(repo.path(), Caller::Human).expect("a repo");
        let request = Request::Query(AppQuery::Status(Default::default()));
        let report = execute_locally(&request, &ctx).expect("git locality");
        assert!(report.is_ok(), "{report:?}");
    }
}
