//! Authorizes and executes one `Call`: who is asking, may they, are they where they claim, then
//! run it. Runs on the dispatch thread, so blocking git work is fine here.

use crate::Inner;
use jerry_core::wire::rpc_code;
use jerry_core::{
    execute_locally, permits, Call, Caller, Ctx, LocalDispatchError, Message, Report, Request,
    RpcError,
};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(crate) fn handle(inner: &Inner, call: Call) -> Result<Value, RpcError> {
    let caller = classify(inner, &call)?;
    if !permits(&caller, call.request.invocability()) {
        return Err(RpcError::new(
            rpc_code::FORBIDDEN,
            format!("{} is not invocable by an agent", call.method()),
        ));
    }
    if let Caller::Agent { id } = &caller {
        confine(inner, id, &call.cwd)?;
    }
    match &call.request {
        Request::Hook(event) => {
            let Caller::Agent { id } = &caller else {
                return Err(RpcError::new(
                    rpc_code::FORBIDDEN,
                    "a hook event needs an agent identity",
                ));
            };
            inner.fanout().broadcast(Message::Notification {
                method: "event/hook".into(),
                params: serde_json::json!({
                    "agent": id,
                    "cwd": call.cwd,
                    "event": event.event,
                    "payload": event.payload,
                }),
            });
            to_value(Report::Ok {
                outcome: Value::Null,
            })
        }
        _ => {
            let ctx = match Ctx::from_cwd(&call.cwd, caller) {
                Ok(ctx) => ctx,
                Err(error) => return to_value(Report::Error { error }),
            };
            match execute_locally(&call.request, &ctx) {
                Ok(report) => to_value(report),
                Err(LocalDispatchError::Forbidden(method)) => Err(RpcError::new(
                    rpc_code::FORBIDDEN,
                    format!("{method} is not invocable by this caller"),
                )),
                Err(LocalDispatchError::NeedsHost(method)) => Err(RpcError::new(
                    rpc_code::INTERNAL_ERROR,
                    format!("{method} needs the session table, which this host gains in #505"),
                )),
            }
        }
    }
}

/// Env-injected identity makes an `Agent`, and only an identity this host handed out counts.
fn classify(inner: &Inner, call: &Call) -> Result<Caller, RpcError> {
    match &call.agent {
        None => Ok(Caller::Human),
        Some(id) => {
            if inner.agents().worktree_of(id).is_none() {
                return Err(RpcError::new(
                    rpc_code::FORBIDDEN,
                    format!("agent {id} is not one this host spawned"),
                ));
            }
            Ok(Caller::Agent { id: id.clone() })
        }
    }
}

/// An agent may only act inside the worktree it was spawned in.
fn confine(inner: &Inner, id: &jerry_core::AgentId, cwd: &Path) -> Result<(), RpcError> {
    let Some(worktree) = inner.agents().worktree_of(id) else {
        return Err(RpcError::new(
            rpc_code::FORBIDDEN,
            format!("agent {id} is not one this host spawned"),
        ));
    };
    let cwd = fs::canonicalize(cwd).map_err(|error| {
        RpcError::new(
            rpc_code::INVALID_PARAMS,
            format!("cwd {} is not accessible: {error}", cwd.display()),
        )
    })?;
    let worktree = fs::canonicalize(&worktree).map_err(|error| {
        RpcError::new(
            rpc_code::INTERNAL_ERROR,
            format!(
                "agent {id}'s worktree {} is not accessible: {error}",
                worktree.display()
            ),
        )
    })?;
    if !cwd.starts_with(&worktree) {
        return Err(RpcError::new(
            rpc_code::CONFINED,
            format!(
                "agent {id} may only act inside {}, not {}",
                worktree.display(),
                cwd.display()
            ),
        )
        .with_data(serde_json::json!({ "worktree": worktree, "cwd": cwd })));
    }
    Ok(())
}

fn to_value(report: Report) -> Result<Value, RpcError> {
    serde_json::to_value(report).map_err(|error| {
        RpcError::new(
            rpc_code::INTERNAL_ERROR,
            format!("the report does not serialize: {error}"),
        )
    })
}
