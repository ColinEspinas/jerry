//! The contract shared by every Jerry client and the session host: what a `Command` and a
//! `Query` are, how a request and its `Report` travel on the wire, how a host is found, and how
//! a client talks to one. Git-locality implementations live here too, so a standalone CLI can
//! run them with nothing but this crate and `jerry-git`.
//!
//! Owns no threads, no listener, no sessions: those are `jerry-host`'s. Zero `gpui`.

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod call;
pub mod client;
pub mod command;
pub mod commands;
pub mod ctx;
pub mod error;
pub mod jerry_binary;
pub mod method;
pub mod queries;
pub mod registry;
pub mod report;
pub mod request;
pub mod wire;

pub use call::Call;
pub use command::{
    permits, run_command, run_query, validate_command, Command, Denied, Invocability, Locality,
    Query,
};
pub use commands::{
    AmendHeadMessage, ConflictKind, ConflictedPathReport, MergeAbort, MergeAttempt,
    MergeAttemptOutcome, MergeBranchIntoCurrent, MergeComplete, MergeCompleteOutcome, RebaseAbort,
    RebaseActionWire, RebaseContinue, RebaseOutcomeReport, RebasePlanEntryWire, RebaseSkip,
    RebaseStart, StageResolved, StopReasonWire,
};
pub use ctx::{AgentId, Caller, Ctx};
pub use error::Error;
pub use method::Method;
pub use queries::{
    MergeStatusOutcome, MergeStatusQuery, RebaseStatusOutcome, RebaseStatusQuery, StatusOutcome,
    StatusQuery,
};
pub use report::Report;
pub use request::{execute_locally, AppCommand, AppQuery, HookEvent, LocalDispatchError, Request};
pub use wire::{Message, RequestId, RpcError, PROTOCOL_VERSION};
