//! The MCP tool catalogue over the `Request` enum (`docs/architecture/decisions.md` §22): one
//! tool per `Command`/`Query` variant, named and described from the same source the wire method
//! and the CLI already use. Building the actual MCP server (stdio transport, JSON-RPC framing)
//! is `jerry-cli`'s job - this module only owns what a tool *is*.

use crate::command::Invocability;
use crate::method::Method;
use crate::request::Request;
use serde_json::Value;

/// One `tools/list` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: &'static str,
    pub input_schema: Value,
    pub invocability: Invocability,
}

/// The MCP tool name for a wire method: `/` becomes `_`, the one substitution
/// [`method_for_tool_name`] reverses. MCP tool names must match `^[a-zA-Z0-9_-]{1,64}$`; a wire
/// method's kind (`command`/`query`) and its kebab-case name (`crate::method::Method::parse`'s
/// own `is_kebab` check) never contain `_`, so the first `_` in a tool name is always the
/// boundary this put there.
pub fn tool_name(method: &Method) -> String {
    method.to_string().replace('/', "_")
}

/// The inverse of [`tool_name`]. `None` for anything that isn't a real `command/`/`query/`
/// method - in particular, a tool name with no `_` at all, or naming `hook`/an `event/`.
pub fn method_for_tool_name(tool: &str) -> Option<Method> {
    let (kind, name) = tool.split_once('_')?;
    Method::parse(&format!("{kind}/{name}"))
}

/// Every tool an MCP client can list - one per `AppCommand`/`AppQuery` variant, built from
/// [`Request::examples`] so the tool catalogue can never diverge from the wire catalogue: a new
/// variant appears in both from the same source, and `request_catalogue_tests` already proves
/// `Request::examples` covers every variant.
pub fn all_tools() -> Vec<ToolSpec> {
    Request::examples()
        .into_iter()
        .filter_map(|(_, request)| tool_for(request))
        .collect()
}

fn tool_for(request: Request) -> Option<ToolSpec> {
    let method = request.method();
    match request {
        Request::Command(command) => Some(ToolSpec {
            name: tool_name(&method),
            description: command.description(),
            input_schema: command.input_schema(),
            invocability: command.invocability(),
        }),
        Request::Query(query) => Some(ToolSpec {
            name: tool_name(&method),
            description: query.description(),
            input_schema: query.input_schema(),
            invocability: query.invocability(),
        }),
        // Not tools: `Validate` is a CLI-only dry-run variant of a Command, and `Hook` is not a
        // caller-invocable action at all.
        Request::Hook(_) | Request::Validate(_) => None,
    }
}

#[cfg(test)]
mod mcp_tool_catalogue_tests {
    use super::{all_tools, method_for_tool_name, tool_name};
    use crate::method::Method;
    use crate::request::{COMMAND_NAMES, QUERY_NAMES};
    use std::collections::HashSet;

    /// MCP's own name grammar (Anthropic's Model Context Protocol specification).
    fn is_valid_mcp_tool_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    #[test]
    fn tool_name_is_bijective_with_method_for_every_command_and_query() {
        for name in COMMAND_NAMES {
            let method = Method::Command((*name).to_owned());
            let tool = tool_name(&method);
            assert!(is_valid_mcp_tool_name(&tool), "{tool:?}");
            assert_eq!(method_for_tool_name(&tool), Some(method));
        }
        for name in QUERY_NAMES {
            let method = Method::Query((*name).to_owned());
            let tool = tool_name(&method);
            assert!(is_valid_mcp_tool_name(&tool), "{tool:?}");
            assert_eq!(method_for_tool_name(&tool), Some(method));
        }
    }

    #[test]
    fn an_unknown_or_malformed_tool_name_maps_to_no_method() {
        for name in ["", "nounderscore", "command_", "_status", "bogus_status"] {
            assert_eq!(method_for_tool_name(name), None, "{name:?}");
        }
    }

    #[test]
    fn every_request_variant_is_exactly_one_tool_with_a_real_description_and_schema() {
        let tools = all_tools();
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            tools.len(),
            COMMAND_NAMES.len() + QUERY_NAMES.len(),
            "one tool per Command/Query variant, no more, no fewer"
        );
        assert_eq!(names.len(), tools.len(), "tool names must be unique");
        for tool in &tools {
            assert!(is_valid_mcp_tool_name(&tool.name), "{tool:?}");
            assert!(!tool.description.is_empty(), "{tool:?}");
            assert!(
                tool.input_schema.is_object(),
                "a generated JSON Schema is always an object: {tool:?}"
            );
        }
    }
}
