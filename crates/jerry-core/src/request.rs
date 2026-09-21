//! The exhaustive catalogue of what a client can ask: every Command, every Query, and the hook
//! event. One enum per kind so dispatch applies each kind's default policy without inspecting
//! variants, and so a CLI or an MCP tool list can be generated from the same source.

use crate::command::{run_query, Invocability, Locality, Query};
use crate::ctx::{AgentId, Ctx};
use crate::method::Method;
use crate::queries::StatusQuery;
use crate::report::Report;
use crate::wire::{rpc_code, RpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One agent hook event, forwarded verbatim by `jerry hook <event>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    pub agent: AgentId,
    /// The agent CLI's own event name, e.g. `PreToolUse`.
    pub event: String,
    /// The hook's stdin, parsed. Never interpreted here.
    pub payload: Value,
}

/// Every Command a client can send. Empty until the merge pilot (#498) lands the first ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "params", rename_all = "kebab-case")]
pub enum AppCommand {}

/// Every Query a client can send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "params", rename_all = "kebab-case")]
pub enum AppQuery {
    Status(StatusQuery),
}

/// The kebab-case names of every `AppCommand` variant, for method lookup and tool listing.
pub const COMMAND_NAMES: &[&str] = &[];

/// The kebab-case names of every `AppQuery` variant.
pub const QUERY_NAMES: &[&str] = &["status"];

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Hook(HookEvent),
    Command(AppCommand),
    Validate(AppCommand),
    Query(AppQuery),
}

/// A Session-locality request reached a process that has no session table.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} needs a running host")]
pub struct NeedsHost(pub Method);

impl AppCommand {
    fn name(&self) -> &'static str {
        match *self {}
    }

    pub fn invocability(&self) -> Invocability {
        match *self {}
    }

    pub fn locality(&self) -> Locality {
        match *self {}
    }
}

impl AppQuery {
    fn name(&self) -> &'static str {
        match self {
            AppQuery::Status(_) => StatusQuery::NAME,
        }
    }

    pub fn invocability(&self) -> Invocability {
        match self {
            AppQuery::Status(query) => query.invocability(),
        }
    }

    pub fn locality(&self) -> Locality {
        match self {
            AppQuery::Status(query) => query.locality(),
        }
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

    /// The JSON-RPC `params` for this request: the payload without the name, which the method
    /// already carries.
    pub fn params(&self) -> Result<Value, serde_json::Error> {
        match self {
            Request::Hook(event) => serde_json::to_value(event),
            Request::Command(command) | Request::Validate(command) => {
                named_params(serde_json::to_value(command)?)
            }
            Request::Query(query) => named_params(serde_json::to_value(query)?),
        }
    }

    /// Classifies an incoming method and its params. Unknown methods and unknown names are
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

    /// One instance per variant, named for its on-disk fixture. The contract test in
    /// `request_catalogue_tests` compares each against `fixtures/<name>.json`.
    pub fn examples() -> Vec<(&'static str, Request)> {
        vec![
            (
                "request-hook",
                Request::Hook(HookEvent {
                    agent: AgentId::from("agent-7"),
                    event: "PreToolUse".into(),
                    payload: serde_json::json!({ "tool_name": "Edit" }),
                }),
            ),
            (
                "request-query-status",
                Request::Query(AppQuery::Status(StatusQuery::default())),
            ),
        ]
    }
}

/// Runs a Git-locality request in this process. Anything needing the session table, hooks
/// included, is handed back as `NeedsHost` so the caller can forward it or fail with exit 4.
pub fn execute_locally(request: &Request, ctx: &Ctx) -> Result<Report, NeedsHost> {
    if request.locality() == Locality::Session {
        return Err(NeedsHost(request.method()));
    }
    match request {
        Request::Hook(_) => Err(NeedsHost(request.method())),
        Request::Command(command) | Request::Validate(command) => match *command {},
        Request::Query(AppQuery::Status(query)) => Ok(run_query(query, ctx)),
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
    use super::{execute_locally, AppQuery, NeedsHost, Request, COMMAND_NAMES, QUERY_NAMES};
    use crate::command::Locality;
    use crate::ctx::{Caller, Ctx};
    use crate::method::Method;
    use crate::wire::rpc_code;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    /// The wire contract, checked mechanically: every variant has a fixture on disk that matches
    /// its serialization byte for byte after normalization, and decodes back to the same value.
    #[test]
    fn every_request_variant_matches_its_fixture_and_round_trips() {
        for (name, request) in Request::examples() {
            let path = fixture_dir().join(format!("{name}.json"));
            let on_disk: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
            )
            .expect("fixture is JSON");

            let method = request.method().to_string();
            let params = request.params().expect("params serialize");
            let produced = serde_json::json!({ "method": method, "params": params });
            assert_eq!(produced, on_disk, "fixture {name} drifted from the code");

            let back = Request::from_wire(&method, params).expect("decodes");
            assert_eq!(back, request, "{name} does not round-trip");
        }
    }

    #[test]
    fn the_name_lists_cover_exactly_the_catalogue() {
        let mut queries: Vec<&str> = Vec::new();
        let mut commands: Vec<&str> = Vec::new();
        for (_, request) in Request::examples() {
            match request.method() {
                Method::Query(name) => queries.push(Box::leak(name.into_boxed_str())),
                Method::Command(name) | Method::Validate(name) => {
                    commands.push(Box::leak(name.into_boxed_str()))
                }
                Method::Hook | Method::Event(_) => {}
            }
        }
        queries.sort_unstable();
        queries.dedup();
        commands.sort_unstable();
        commands.dedup();
        assert_eq!(queries, QUERY_NAMES);
        assert_eq!(commands, COMMAND_NAMES);
        let fixtures = fs::read_dir(fixture_dir())
            .expect("fixtures dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .map(|e| e.file_name().to_string_lossy().starts_with("request-"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            fixtures,
            Request::examples().len(),
            "a stray or missing request fixture"
        );
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
        assert_eq!(execute_locally(&hook, &ctx), Err(NeedsHost(Method::Hook)));
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
