//! Authorizes and executes one `Call`: who is asking, may they, are they where they claim, then
//! run it. One dispatch thread serves every client in turn, so a slow git operation delays the
//! next caller; that is the accepted shape while every Command is short.
//!
//! The trust boundary is the user, not the process: the socket is reachable only by the user's
//! own uid, and an agent's identity is what its environment says. `Invocability` and cwd
//! confinement keep an agent from acting outside its lane by accident; they are guardrails, not
//! a sandbox against a process that chooses to lie about who it is.

use crate::session::SessionError;
use crate::Inner;
use jerry_core::wire::rpc_code;
use jerry_core::{
    execute_locally, permits, AppCommand, AppQuery, Call, Caller, Ctx, Error, LocalDispatchError,
    Message, Report, Request, RpcError, SessionSpawnOutcome,
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
        // `Locality::Session`: `execute_locally` can never answer this on its own (§15), so the
        // one process with a real session table answers directly from it, exactly like `Hook`
        // above.
        Request::Query(AppQuery::Agents(_)) => to_value(Report::ok(&agents_entries(inner))),
        // `SessionsQuery`/`SessionSpawn`/`SessionResize`/`SessionKill` are the same shape:
        // `Locality::Session`, answered directly from `inner.sessions()` rather than through
        // `execute_locally` (decisions.md §23).
        Request::Query(AppQuery::Sessions(_)) => to_value(Report::ok(&inner.sessions().list())),
        Request::Command(AppCommand::SessionSpawn(command)) => {
            to_value(match spawn_session(inner, &call.cwd, command.clone()) {
                Ok(outcome) => Report::ok(&outcome),
                Err(error) => Report::Error { error },
            })
        }
        Request::Command(AppCommand::SessionResize(command)) => to_value(
            match inner
                .sessions()
                .resize(&command.id, command.rows, command.cols)
            {
                Ok(()) => Report::Ok {
                    outcome: Value::Null,
                },
                Err(error) => Report::Error {
                    error: session_error_to_wire(error),
                },
            },
        ),
        Request::Command(AppCommand::SessionKill(command)) => {
            to_value(match inner.sessions().kill(&command.id) {
                Ok(()) => Report::Ok {
                    outcome: Value::Null,
                },
                Err(error) => Report::Error {
                    error: session_error_to_wire(error),
                },
            })
        }
        _ => {
            let requested_by = caller.clone();
            let ctx = match Ctx::from_cwd(&call.cwd, caller) {
                Ok(ctx) => ctx,
                Err(error) => return to_value(Report::Error { error }),
            };
            match execute_locally(&call.request, &ctx) {
                Ok(report) => {
                    publish_worktree_created(inner, &call.request, &report, &requested_by);
                    to_value(report)
                }
                Err(LocalDispatchError::Forbidden(method)) => Err(RpcError::new(
                    rpc_code::FORBIDDEN,
                    format!("{method} is not invocable by this caller"),
                )),
                Err(LocalDispatchError::NeedsHost(method)) => Err(RpcError::new(
                    rpc_code::NEEDS_HOST,
                    format!("{method} needs a session table, and this host has none yet"),
                )),
            }
        }
    }
}

/// Every agent this host is tracking, as `AgentsQuery`'s own wire shape.
fn agents_entries(inner: &Inner) -> Vec<jerry_core::AgentsEntry> {
    inner
        .agents()
        .list()
        .into_iter()
        .map(|(id, record)| jerry_core::AgentsEntry {
            id: id.to_string(),
            kind: record.kind,
            worktree: record.worktree,
        })
        .collect()
}

/// `SessionSpawn`'s real execution: builds `jerry_pty::SpawnOptions` from the wire command and
/// the caller's own `cwd` (the new session's worktree - never a field on the command itself, so
/// a caller cannot ask to spawn into somewhere its own confinement wouldn't otherwise reach), and
/// hands it to `inner.sessions()`. The returned handle is not part of this outcome - see
/// `SessionManager::handle_for`'s own docs for how an in-process caller attaches to it.
fn spawn_session(
    inner: &Inner,
    cwd: &Path,
    command: jerry_core::SessionSpawn,
) -> Result<SessionSpawnOutcome, Error> {
    let mut options = jerry_pty::SpawnOptions::new(command.program)
        .args(command.args)
        .cwd(cwd.to_path_buf())
        .size(command.rows, command.cols);
    for (key, value) in command.env {
        options = options.env(key, value);
    }
    let (id, _handle) = inner
        .sessions()
        .spawn(cwd.to_path_buf(), None, options)
        .map_err(|error| Error::new("session-spawn-failed", error.to_string()))?;
    Ok(SessionSpawnOutcome { id })
}

fn session_error_to_wire(error: SessionError) -> Error {
    match error {
        SessionError::NotFound(id) => {
            Error::new("session-not-found", format!("no session with id {id}"))
        }
        SessionError::NotOwned(id) => Error::new(
            "session-not-owned",
            format!("session {id} has no process this host owns"),
        ),
        SessionError::Pty(source) => Error::new("session-pty-error", source.to_string()),
    }
}

/// After a successful `command/worktree-create`, the agent spawn is the host's own reaction to
/// the outcome, never the Command's (`docs/architecture/decisions.md` §21): publishes
/// `event/worktree-created` for `jerry-app`'s subscriber to open the worktree and, when `agent`
/// was given, spawn it there. Published with `agent: null` for a plain creation too, so the app
/// opens every worktree it created this way, agent or none. A no-op for anything else, and for a
/// `Validate` (nothing ran) or a refused/failed `Command`.
fn publish_worktree_created(
    inner: &Inner,
    request: &Request,
    report: &Report,
    requested_by: &Caller,
) {
    let Request::Command(AppCommand::WorktreeCreate(command)) = request else {
        return;
    };
    let Report::Ok { outcome } = report else {
        return;
    };
    let Some(path) = outcome.get("path").cloned() else {
        return;
    };
    inner.fanout().broadcast(Message::Notification {
        method: "event/worktree-created".into(),
        params: serde_json::json!({
            "path": path,
            "agent": command.agent,
            "prompt": command.prompt,
            "requested_by": requested_by,
        }),
    });
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
