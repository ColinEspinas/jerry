//! Git-locality Queries. Each is a plain value that runs against the shared on-disk repository
//! in whatever process holds it.

use crate::command::{Locality, Query};
use crate::ctx::Ctx;
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where am I: the worktree, the repository and the caller a request acts as, as the host
/// resolved them from the call envelope.
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

/// Is a merge in progress for this repository, and what is still unmerged in it. What
/// `jerry merge --continue` and `--abort` read before acting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeStatusQuery {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeStatusOutcome {
    /// The base worktree holding `MERGE_HEAD`, if any.
    pub in_progress_at: Option<PathBuf>,
    /// Paths git still holds as unmerged there; empty when nothing is in progress.
    pub unmerged: Vec<PathBuf>,
}

impl Query for MergeStatusQuery {
    type Outcome = MergeStatusOutcome;
    const NAME: &'static str = "merge-status";

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn run(&self, ctx: &Ctx) -> Result<MergeStatusOutcome, Error> {
        let in_progress_at = jerry_git::merge::find_in_progress_merge(&ctx.repo_path)?;
        let unmerged = match &in_progress_at {
            Some(base) => jerry_git::merge::unmerged_files(base)?,
            None => Vec::new(),
        };
        Ok(MergeStatusOutcome {
            in_progress_at,
            unmerged,
        })
    }
}
