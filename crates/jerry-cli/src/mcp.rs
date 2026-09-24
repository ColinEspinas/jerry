//! `jerry mcp`: an MCP server over the `Request` enum (`docs/architecture/decisions.md` §22).
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout - stdout is reserved for the protocol, every
//! diagnostic goes to stderr. One request at a time, blocking: no new async runtime for this.
//! `tools/list` and `tools/call` are generated from `jerry_core::all_tools`, never hand-written.

use crate::exit;
use crate::Session;
use jerry_core::wire::{read_line_frame, rpc_code, write_line_frame, FrameError};
use jerry_core::{
    permits, tool_name, AppQuery, Caller, Message, Method, Report, Request, RequestId, RpcError,
};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufReader, Read, Write};

/// The legacy `initialize`-handshake protocol version this server speaks - see why, over the
/// current `2026-07-28` revision, in `docs/architecture/decisions.md` §22.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// The kebab-case code an MCP tool result's `structuredContent` carries when the caller's
/// `Invocability` forbids the tool - the same classification the socket answers as
/// `rpc_code::FORBIDDEN`, spelled the way every other refusal in this codebase already is
/// (`Denied::code`, `Report::Denied::code`).
const FORBIDDEN_CODE: &str = "forbidden";

/// Runs the server loop until stdin closes (a clean EOF exits 0) or stdin/stdout itself fails
/// (the failure exit code - there is no request left to answer, so nothing to keep looping for).
pub(crate) fn run(
    session: &mut Session,
    caller: &Caller,
    stdin: Box<dyn Read + Send>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let _ = writeln!(err, "jerry mcp: ready");
    let mut reader = BufReader::new(stdin);
    loop {
        let message = match read_line_frame(&mut reader) {
            Ok(message) => message,
            Err(FrameError::Closed) => return exit::DONE,
            // The client's own mistake, not this server's: answer it and keep reading, exactly
            // as a length-prefixed connection would keep serving other calls.
            Err(error @ FrameError::Json(_)) => {
                let response = Message::Response {
                    id: RequestId::Null,
                    result: Err(RpcError::new(rpc_code::PARSE_ERROR, error.to_string())),
                };
                if !write_response(out, err, &response) {
                    return exit::FAILED;
                }
                continue;
            }
            // Unlike a malformed line, this is stdin itself misbehaving (a truncated read, or a
            // single line past `MAX_FRAME_BYTES` with no terminator) - answering and continuing
            // would just repeat the same failure against whatever is left of the stream.
            Err(error) => {
                let _ = writeln!(err, "jerry mcp: stdin read failed: {error}");
                return exit::FAILED;
            }
        };
        match message {
            Message::Request { id, method, params } => {
                let result = handle(session, caller, &method, params, err);
                if !write_response(out, err, &Message::Response { id, result }) {
                    return exit::FAILED;
                }
            }
            // `notifications/initialized` needs no reply; an MCP client never sends `jerry mcp` a
            // `Response`, so any that arrives is equally inert.
            Message::Notification { .. } | Message::Response { .. } => {}
        }
    }
}

/// Writes one response frame; `false` means the write itself failed, so the caller has nothing
/// left to answer with and should stop rather than fail the same write again on every request.
fn write_response(out: &mut dyn Write, err: &mut dyn Write, message: &Message) -> bool {
    match write_line_frame(out, message) {
        Ok(()) => true,
        Err(error) => {
            let _ = writeln!(
                err,
                "jerry mcp: could not write a response to stdout: {error}"
            );
            false
        }
    }
}

fn handle(
    session: &mut Session,
    caller: &Caller,
    method: &str,
    params: Value,
    err: &mut dyn Write,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize_result()),
        "ping" => Ok(serde_json::json!({})),
        "tools/list" => Ok(tools_list_result(session, caller, err)),
        "tools/call" => call_tool(session, caller, params, err),
        other => Err(RpcError::new(
            rpc_code::METHOD_NOT_FOUND,
            format!("no such method: {other}"),
        )),
    }
}

fn initialize_result() -> Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "jerry", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// Only the tools this caller's `Invocability` allows - an agent sees `Allowed` variants only, a
/// human caller (no `JERRY_AGENT_ID` in its environment) sees everything, mirroring the socket's
/// own `permits` rule - plus, per `docs/architecture/decisions.md` §26, whatever an orchestrator's
/// own grants open beyond that, exactly as they open the underlying method on the socket
/// (`jerry_host::dispatch::granted`): `tools/list` reflects the same authority `tools/call` would
/// actually honor, rather than listing a tool a grant makes callable but hiding it anyway.
fn tools_list_result(session: &mut Session, caller: &Caller, err: &mut dyn Write) -> Value {
    let granted_tool_names = granted_tool_names(session, caller, err);
    let tools: Vec<Value> = jerry_core::all_tools()
        .into_iter()
        .filter(|tool| {
            permits(caller, tool.invocability) || granted_tool_names.contains(&tool.name)
        })
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            })
        })
        .collect();
    serde_json::json!({ "tools": tools })
}

/// The tool names this exact caller's own `SessionAgentInfo::grants` opens - empty for a human
/// (already permitted everything by `permits`) or an agent with no live session the host can
/// resolve grants from (standalone, or a registration with none). Asks a real `SessionsQuery`
/// rather than trusting anything the caller claims about itself - the same host-authoritative
/// check `tools/call` already makes at call time.
fn granted_tool_names(
    session: &mut Session,
    caller: &Caller,
    err: &mut dyn Write,
) -> HashSet<String> {
    let Caller::Agent { id } = caller else {
        return HashSet::new();
    };
    let Ok(Report::Ok { outcome }) =
        session.call(Request::Query(AppQuery::Sessions(Default::default())), err)
    else {
        return HashSet::new();
    };
    let Some(entries) = outcome.as_array() else {
        return HashSet::new();
    };
    entries
        .iter()
        .find(|entry| entry["agent"]["agent_id"].as_str() == Some(id.0.as_str()))
        .and_then(|entry| entry["agent"]["grants"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(Method::parse)
        .map(|method| tool_name(&method))
        .collect()
}

/// `name`/`arguments` are the client's mistake when malformed - a real JSON-RPC error, like any
/// other invalid request. Once a real `Request` exists, everything past that point is this tool
/// *call's* own outcome, so it becomes an `Ok` tool result, `isError` set or not, never a
/// JSON-RPC error - decision 3, `docs/architecture/decisions.md` §22.
fn call_tool(
    session: &mut Session,
    caller: &Caller,
    params: Value,
    err: &mut dyn Write,
) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(rpc_code::INVALID_PARAMS, "tools/call needs a \"name\""))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let method = jerry_core::method_for_tool_name(name).ok_or_else(|| {
        RpcError::new(rpc_code::METHOD_NOT_FOUND, format!("no such tool: {name}"))
    })?;
    let request = Request::from_wire(&method.to_string(), arguments)?;
    if !permits(caller, request.invocability()) {
        return Ok(forbidden_tool_result(&method));
    }
    match session.call(request, err) {
        Ok(report) => Ok(report_tool_result(&report)),
        Err(code) => Ok(tool_error(format!(
            "the request could not be completed (exit {code}); see the server's own stderr log"
        ))),
    }
}

fn forbidden_tool_result(method: &Method) -> Value {
    let reason = format!("{method} is not invocable by this caller");
    serde_json::json!({
        "content": [{ "type": "text", "text": reason.clone() }],
        "isError": true,
        "structuredContent": { "code": FORBIDDEN_CODE, "reason": reason },
    })
}

/// A `Report` becomes the tool result's text (compact JSON) plus its raw `structuredContent`;
/// `isError` is set for anything but `Report::Ok` - a `Denied` command call is exactly this path.
fn report_tool_result(report: &Report) -> Value {
    let structured = serde_json::to_value(report).unwrap_or(Value::Null);
    let text = serde_json::to_string(report).unwrap_or_else(|_| "{}".to_owned());
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !report.is_ok(),
        "structuredContent": structured,
    })
}

fn tool_error(message: String) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

#[cfg(test)]
mod mcp_server_tests {
    use jerry_core::registry::{Os, Registry};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use test_support::seed_empty_repo;

    fn env(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_os_str().to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn runtime_env_key() -> &'static str {
        match Os::host() {
            Os::Windows => "LOCALAPPDATA",
            Os::MacOs => "TMPDIR",
            Os::Unix => "XDG_RUNTIME_DIR",
        }
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("jerry")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    /// Runs `jerry mcp` end to end against a pipe: `requests`, one JSON-RPC message per line, as
    /// stdin, and every newline-delimited response on stdout parsed back into a `Value`.
    fn run_mcp(
        env: &dyn Fn(&str) -> Option<OsString>,
        cwd: &Path,
        requests: &[serde_json::Value],
    ) -> (u8, Vec<serde_json::Value>, String) {
        let mut input = String::new();
        for request in requests {
            input.push_str(&request.to_string());
            input.push('\n');
        }
        let stdin: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(input.into_bytes()));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::run(args(&["mcp"]), env, cwd, stdin, &mut out, &mut err);
        let out = String::from_utf8(out).expect("utf8 stdout");
        let responses = out
            .lines()
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?} not JSON: {e}"))
            })
            .collect();
        (
            code,
            responses,
            String::from_utf8(err).expect("utf8 stderr"),
        )
    }

    fn request(id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn eof_with_no_input_ends_the_loop_at_exit_zero_with_no_output() {
        let repo = seed_empty_repo();
        let (code, responses, _err) = run_mcp(&env(&[]), repo.path(), &[]);
        assert_eq!(code, 0);
        assert!(responses.is_empty());
    }

    #[test]
    fn initialize_answers_with_a_real_protocol_version_and_server_info() {
        let repo = seed_empty_repo();
        let init = request(
            1,
            "initialize",
            serde_json::json!({ "protocolVersion": "2025-11-25", "capabilities": {} }),
        );
        let (code, responses, err) = run_mcp(&env(&[]), repo.path(), &[init]);
        assert_eq!(code, 0, "{err}");
        let result = &responses[0]["result"];
        assert_eq!(result["serverInfo"]["name"], serde_json::json!("jerry"));
        assert!(result["protocolVersion"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn ping_answers_immediately_with_no_transport_needed() {
        let repo = seed_empty_repo();
        let (code, responses, err) = run_mcp(
            &env(&[]),
            repo.path(),
            &[request(1, "ping", serde_json::json!({}))],
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(responses[0]["result"], serde_json::json!({}));
    }

    #[test]
    fn an_unknown_method_is_a_real_json_rpc_error() {
        let repo = seed_empty_repo();
        let (code, responses, err) = run_mcp(
            &env(&[]),
            repo.path(),
            &[request(1, "not/a/real/method", serde_json::Value::Null)],
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(
            responses[0]["error"]["code"],
            serde_json::json!(jerry_core::wire::rpc_code::METHOD_NOT_FOUND)
        );
    }

    #[test]
    fn a_malformed_line_gets_a_parse_error_and_the_loop_keeps_serving_the_next_request() {
        let repo = seed_empty_repo();
        let mut input = "not json at all\n".to_owned();
        input.push_str(&request(2, "ping", serde_json::json!({})).to_string());
        input.push('\n');
        let stdin: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(input.into_bytes()));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::run(
            args(&["mcp"]),
            &env(&[]),
            repo.path(),
            stdin,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let lines: Vec<serde_json::Value> = String::from_utf8(out)
            .expect("utf8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(
            lines.len(),
            2,
            "the loop keeps going after the malformed line"
        );
        assert_eq!(lines[0]["id"], serde_json::Value::Null);
        assert!(lines[0]["error"]["code"].is_i64(), "{:?}", lines[0]);
        assert_eq!(lines[1]["result"], serde_json::json!({}));
    }

    /// Always fails the write - stands in for a broken stdout (a closed pipe, a client that
    /// exited).
    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_broken_stdout_ends_the_loop_with_the_failure_exit_code() {
        let repo = seed_empty_repo();
        let input = request(1, "ping", serde_json::json!({})).to_string() + "\n";
        let stdin: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(input.into_bytes()));
        let mut out = FailingWriter;
        let mut err = Vec::new();
        let code = crate::run(
            args(&["mcp"]),
            &env(&[]),
            repo.path(),
            stdin,
            &mut out,
            &mut err,
        );
        assert_eq!(code, crate::exit::FAILED);
    }

    #[test]
    fn a_line_past_the_frame_cap_ends_the_loop_instead_of_spinning_on_the_rest_of_the_stream() {
        let repo = seed_empty_repo();
        // One line, never terminated, past `jerry_core::wire::MAX_FRAME_BYTES` - proves the loop
        // stops on the first oversized read rather than re-reading the same unterminated tail.
        let oversized = vec![b'a'; 16 * 1024 * 1024 + 10];
        let stdin: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(oversized));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::run(
            args(&["mcp"]),
            &env(&[]),
            repo.path(),
            stdin,
            &mut out,
            &mut err,
        );
        assert_eq!(
            code,
            crate::exit::FAILED,
            "stderr: {}",
            String::from_utf8_lossy(&err)
        );
        assert!(out.is_empty(), "an oversized line has no response to send");
    }

    #[test]
    fn tools_list_grants_an_agent_only_allowed_tools_and_a_human_everything() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let list = request(1, "tools/list", serde_json::json!({}));

        let (code, responses, err) = run_mcp(
            &env(&[(runtime_env_key(), runtime.path())]),
            repo.path(),
            std::slice::from_ref(&list),
        );
        assert_eq!(code, 0, "{err}");
        let human_tools = responses[0]["result"]["tools"].as_array().expect("tools");
        assert!(human_tools
            .iter()
            .any(|t| t["name"] == serde_json::json!("command_merge-attempt")));
        assert!(human_tools
            .iter()
            .any(|t| t["name"] == serde_json::json!("command_worktree-create")));

        let agent_id = PathBuf::from("agent-1");
        let agent_env = env(&[
            (runtime_env_key(), runtime.path()),
            (crate::AGENT_ENV, agent_id.as_path()),
        ]);
        let (code, responses, err) = run_mcp(&agent_env, repo.path(), &[list]);
        assert_eq!(code, 0, "{err}");
        let agent_tools = responses[0]["result"]["tools"].as_array().expect("tools");
        // Merge is `Denied` to agents (decisions.md §18); worktree-create is `Allowed` (§21).
        assert!(!agent_tools
            .iter()
            .any(|t| t["name"] == serde_json::json!("command_merge-attempt")));
        assert!(agent_tools
            .iter()
            .any(|t| t["name"] == serde_json::json!("command_worktree-create")));
        assert!(agent_tools.len() < human_tools.len());
    }

    /// `docs/architecture/decisions.md` §26: an orchestrator's own `SessionAgentInfo::grants`
    /// widens its `tools/list` beyond `Invocability` alone - `command_session-send` is `Denied`
    /// by default (an ordinary agent never sees it, proven by the test above), but a real,
    /// granted orchestrator session does.
    #[test]
    fn tools_list_reflects_a_real_grant_beyond_invocability() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let base_env = env(&[(runtime_env_key(), runtime.path())]);
        let registry_dir =
            jerry_core::registry::runtime_dir_for(Os::host(), &base_env).expect("runtime dir");
        let registry = Registry::open(registry_dir).expect("registry");
        let instance = registry.allocate().expect("instance");
        let host = jerry_host::Host::start().expect("host");
        host.listen(&instance.socket).expect("listen");
        let common = jerry_core::Ctx::from_cwd(repo.path(), jerry_core::Caller::Human)
            .expect("a repo")
            .repo_path;
        registry.publish(&instance, &[common]).expect("publish");

        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let orchestrator_spawn = jerry_core::Request::Command(
            jerry_core::AppCommand::SessionSpawn(jerry_core::SessionSpawn {
                program: if cfg!(windows) { "cmd" } else { "sh" }.into(),
                args: if cfg!(windows) {
                    vec!["/c".into(), sleep.into()]
                } else {
                    vec!["-c".into(), sleep.into()]
                },
                env: Vec::new(),
                rows: 24,
                cols: 80,
                agent: Some(jerry_core::SessionAgentInfo {
                    kind: "Claude".into(),
                    agent_id: jerry_core::AgentId::from("agent-orchestrator"),
                    grants: vec!["command/session-send".into()],
                    parent: None,
                }),
            }),
        );
        let spawned = futures::executor::block_on(
            host.client()
                .request(jerry_core::Call::human(repo.path(), orchestrator_spawn)),
        )
        .expect("orchestrator spawn dispatched");
        assert!(spawned.is_ok(), "{spawned:?}");

        let orchestrator_agent = PathBuf::from("agent-orchestrator");
        let orchestrator_env = env(&[
            (runtime_env_key(), runtime.path()),
            (crate::AGENT_ENV, &orchestrator_agent),
        ]);
        let list = request(1, "tools/list", serde_json::json!({}));
        let (code, responses, err) = run_mcp(&orchestrator_env, repo.path(), &[list]);
        assert_eq!(code, 0, "{err}");
        let tools = responses[0]["result"]["tools"].as_array().expect("tools");
        assert!(
            tools
                .iter()
                .any(|t| t["name"] == serde_json::json!("command_session-send")),
            "a granted orchestrator must see the tool its grant opens: {tools:?}"
        );

        host.shutdown_and_join();
        registry.remove(&instance).expect("remove");
    }

    #[test]
    fn tools_call_of_a_query_dispatches_through_the_in_process_test_host() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);
        let registry_dir =
            jerry_core::registry::runtime_dir_for(Os::host(), &env).expect("runtime dir");
        let registry = Registry::open(registry_dir).expect("registry");
        let instance = registry.allocate().expect("instance");
        let host = jerry_host::Host::start().expect("host");
        host.listen(&instance.socket).expect("listen");
        let common = jerry_core::Ctx::from_cwd(repo.path(), jerry_core::Caller::Human)
            .expect("a repo")
            .repo_path;
        registry.publish(&instance, &[common]).expect("publish");

        let call = request(
            1,
            "tools/call",
            serde_json::json!({ "name": "query_agents", "arguments": {} }),
        );
        let (code, responses, err) = run_mcp(&env, repo.path(), &[call]);
        assert_eq!(code, 0, "{err}");
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], serde_json::json!(false));
        assert_eq!(
            result["structuredContent"],
            serde_json::json!({ "status": "ok", "outcome": [] }),
            "the real host answered `AgentsQuery` through the socket, not a stub"
        );

        host.shutdown_and_join();
        registry.remove(&instance).expect("remove");
    }

    #[test]
    fn a_denied_command_becomes_an_is_error_tool_result_carrying_the_report() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);
        let call = request(
            1,
            "tools/call",
            serde_json::json!({
                "name": "command_merge-abort",
                "arguments": { "base_worktree_path": repo.path().to_str().expect("utf8") },
            }),
        );
        let (code, responses, err) = run_mcp(&env, repo.path(), &[call]);
        assert_eq!(code, 0, "{err}");
        assert!(responses[0].get("error").is_none(), "{:?}", responses[0]);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], serde_json::json!(true));
        assert_eq!(
            result["structuredContent"]["code"],
            serde_json::json!("merge-not-in-progress")
        );
    }

    #[test]
    fn an_agent_calling_a_denied_tool_gets_a_forbidden_result_never_a_json_rpc_error() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let agent_id = PathBuf::from("agent-1");
        let env = env(&[
            (runtime_env_key(), runtime.path()),
            (crate::AGENT_ENV, agent_id.as_path()),
        ]);
        let call = request(
            1,
            "tools/call",
            serde_json::json!({ "name": "command_merge-attempt", "arguments": {} }),
        );
        let (code, responses, err) = run_mcp(&env, repo.path(), &[call]);
        assert_eq!(code, 0, "{err}");
        assert!(responses[0].get("error").is_none(), "{:?}", responses[0]);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], serde_json::json!(true));
        assert_eq!(
            result["structuredContent"]["code"],
            serde_json::json!("forbidden")
        );
    }

    #[test]
    fn tools_call_of_an_unknown_tool_name_is_a_json_rpc_error() {
        let repo = seed_empty_repo();
        let call = request(
            1,
            "tools/call",
            serde_json::json!({ "name": "not-a-real-tool", "arguments": {} }),
        );
        let (code, responses, err) = run_mcp(&env(&[]), repo.path(), &[call]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(
            responses[0]["error"]["code"],
            serde_json::json!(jerry_core::wire::rpc_code::METHOD_NOT_FOUND)
        );
    }
}
