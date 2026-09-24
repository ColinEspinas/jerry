//! The envelope around a `Request` on the wire: where the caller is and, for an agent, who it
//! is. Every method carries the same shape, so a host classifies and confines uniformly.

use crate::ctx::AgentId;
use crate::method::Method;
use crate::request::Request;
use crate::wire::{rpc_code, RpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The directory the caller acts from; the host derives the worktree and repository from it.
    pub cwd: PathBuf,
    /// Present only when the caller's environment carries `JERRY_AGENT_ID`.
    pub agent: Option<AgentId>,
    pub request: Request,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentId>,
    #[serde(default)]
    params: Value,
}

impl Call {
    pub fn human(cwd: impl Into<PathBuf>, request: Request) -> Call {
        Call {
            cwd: cwd.into(),
            agent: None,
            request,
        }
    }

    pub fn agent(cwd: impl Into<PathBuf>, agent: AgentId, request: Request) -> Call {
        Call {
            cwd: cwd.into(),
            agent: Some(agent),
            request,
        }
    }

    pub fn method(&self) -> Method {
        self.request.method()
    }

    pub fn params(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(Envelope {
            cwd: self.cwd.clone(),
            agent: self.agent.clone(),
            params: self.request.params()?,
        })
    }

    pub fn from_wire(method: &str, params: Value) -> Result<Call, RpcError> {
        let envelope: Envelope = serde_json::from_value(params).map_err(|error| {
            RpcError::new(
                rpc_code::INVALID_PARAMS,
                format!("invalid envelope for {method}: {error}"),
            )
            .with_data(serde_json::json!({ "method": method }))
        })?;
        Ok(Call {
            cwd: envelope.cwd,
            agent: envelope.agent,
            request: Request::from_wire(method, envelope.params)?,
        })
    }

    /// One call per request variant, named for its fixture under `fixtures/`.
    pub fn examples() -> Vec<(&'static str, Call)> {
        Request::examples()
            .into_iter()
            .map(|(name, request)| match request {
                Request::Hook(_) => (
                    name,
                    Call::agent("/repo/wt-agent-7", AgentId::from("agent-7"), request),
                ),
                other => (name, Call::human("/repo", other)),
            })
            .collect()
    }
}

#[cfg(test)]
mod call_contract_tests {
    use super::Call;
    use crate::wire::rpc_code;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    /// The wire contract, checked mechanically: every variant has a fixture on disk equal to
    /// its serialization, and decodes back to the same call.
    #[test]
    fn every_call_matches_its_fixture_and_round_trips() {
        for (name, call) in Call::examples() {
            let path = fixture_dir().join(format!("{name}.json"));
            let on_disk: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
            )
            .expect("fixture is JSON");

            let method = call.method().to_string();
            let params = call.params().expect("params serialize");
            let produced = serde_json::json!({ "method": method, "params": params });
            assert_eq!(produced, on_disk, "fixture {name} drifted from the code");

            let back = Call::from_wire(&method, params).expect("decodes");
            assert_eq!(back, call, "{name} does not round-trip");
        }
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
            Call::examples().len(),
            "a stray or missing request fixture"
        );
    }

    #[test]
    fn a_missing_cwd_is_an_invalid_envelope() {
        let err = Call::from_wire("query/status", serde_json::json!({ "params": {} }))
            .expect_err("cwd is required");
        assert_eq!(err.code, rpc_code::INVALID_PARAMS);
    }

    #[test]
    fn the_agent_field_is_omitted_for_a_human() {
        let (_, call) = Call::examples()
            .into_iter()
            .find(|(name, _)| *name == "request-query-status")
            .expect("status example");
        let params = call.params().expect("serialize");
        assert!(params.get("agent").is_none(), "{params}");
    }
}
