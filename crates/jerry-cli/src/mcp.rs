//! `jerry mcp`: an MCP server over the `Request` enum (`docs/architecture/decisions.md` §22).
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout - stdout is reserved for the protocol, every
//! diagnostic goes to stderr. One request at a time, blocking: no new async runtime for this.
//! `tools/list` and `tools/call` are generated from `jerry_core::all_tools`, never hand-written.

use crate::exit;
use crate::Session;
use jerry_core::wire::{read_line_frame, rpc_code, write_line_frame, FrameError};
use jerry_core::{permits, Caller, Message, Method, Report, Request, RequestId, RpcError};
use serde_json::Value;
use std::io::{BufReader, Read, Write};

/// The latest protocol revision that still uses the classic `initialize` handshake - the MCP
/// specification calls this "legacy" as of its 2026-07-28 revision, which replaced it for new
/// clients with a stateless, per-request `_meta.protocolVersion` plus a mandatory
/// `server/discover` call (verified at
/// <https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning> and
/// <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>). Real MCP clients,
/// Claude Code included, still overwhelmingly speak the handshake this module implements.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// The kebab-case code an MCP tool result's `structuredContent` carries when the caller's
/// `Invocability` forbids the tool - the same classification the socket answers as
/// `rpc_code::FORBIDDEN`, spelled the way every other refusal in this codebase already is
/// (`Denied::code`, `Report::Denied::code`).
const FORBIDDEN_CODE: &str = "forbidden";

/// Runs the server loop until stdin closes (a clean EOF exits 0).
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
            // A malformed line is the client's mistake, not this server's: answer it and keep
            // reading, exactly as a length-prefixed connection would keep serving other calls.
            Err(parse_error) => {
                let _ = write_line_frame(
                    out,
                    &Message::Response {
                        id: RequestId::Null,
                        result: Err(RpcError::new(
                            rpc_code::PARSE_ERROR,
                            parse_error.to_string(),
                        )),
                    },
                );
                continue;
            }
        };
        match message {
            Message::Request { id, method, params } => {
                let result = handle(session, caller, &method, params, err);
                let _ = write_line_frame(out, &Message::Response { id, result });
            }
            // `notifications/initialized` needs no reply; an MCP client never sends `jerry mcp` a
            // `Response`, so any that arrives is equally inert.
            Message::Notification { .. } | Message::Response { .. } => {}
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
        "tools/list" => Ok(tools_list_result(caller)),
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
/// own `permits` rule.
fn tools_list_result(caller: &Caller) -> Value {
    let tools: Vec<Value> = jerry_core::all_tools()
        .into_iter()
        .filter(|tool| permits(caller, tool.invocability))
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
