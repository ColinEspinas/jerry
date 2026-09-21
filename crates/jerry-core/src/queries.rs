//! Git-locality Queries. Each is a plain value that runs against the shared on-disk repository
//! in whatever process holds it.

use crate::command::{Locality, Query};
use crate::ctx::Ctx;
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where am I: the worktree and the repository a caller is acting in. The skeleton the wire is
/// proven against; later issues add the agent and merge state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusQuery {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOutcome {
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub caller: crate::ctx::Caller,
}

impl Query for StatusQuery {
    type Outcome = StatusOutcome;
    const NAME: &'static str = "status";

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn run(&self, ctx: &Ctx) -> Result<StatusOutcome, Error> {
        Ok(StatusOutcome {
            repo_path: ctx.repo_path.clone(),
            worktree_path: ctx.worktree_path.clone(),
            caller: ctx.caller.clone(),
        })
    }
}
