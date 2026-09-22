//! Git-locality Queries. Each is a plain value that runs against the shared on-disk repository
//! in whatever process holds it.

use crate::command::{Locality, Query};
use crate::commands::StopReasonWire;
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

/// Every agent the session host is currently supervising - `Locality::Session`, since the
/// answer lives in `jerry-host`'s own agent table (§15), not anything this crate can read. Local
/// dispatch always answers `NeedsHost` for it, exactly like every other Session-locality request;
/// the host itself special-cases it in its dispatcher rather than calling [`Query::run`] below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsQuery {}

/// One agent as the wire sees it: its host-assigned id, which CLI it runs, and its worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsEntry {
    pub id: String,
    pub kind: String,
    pub worktree: PathBuf,
}

impl Query for AgentsQuery {
    type Outcome = Vec<AgentsEntry>;
    const NAME: &'static str = "agents";

    fn locality(&self) -> Locality {
        Locality::Session
    }

    /// Never actually reached: `Locality::Session` means `execute_locally` returns `NeedsHost`
    /// before calling this, and the real host answers from its own agent table instead of this
    /// trait method (see the type's own docs). Honest about that rather than pretending to have
    /// host access it does not.
    fn run(&self, _ctx: &Ctx) -> Result<Vec<AgentsEntry>, Error> {
        Err(Error::new(
            "needs-host",
            "the agent table lives on the session host, not in this process",
        ))
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

/// Is a rebase stopped at the caller's worktree, and where. What the graph pane's rebase mode
/// reconstructs its `Stopped`/`Planning` phase from after dispatching a mutation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseStatusQuery {}

/// `jerry_git::rebase::RebaseStatus`, mirrored: paths and names only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseStatusOutcome {
    pub onto: Option<String>,
    pub current_step: Option<usize>,
    pub total_steps: Option<usize>,
    pub stopped_commit: Option<String>,
    pub conflicted_files: Vec<PathBuf>,
    pub stop_reason: Option<StopReasonWire>,
}

impl From<jerry_git::rebase::RebaseStatus> for RebaseStatusOutcome {
    fn from(status: jerry_git::rebase::RebaseStatus) -> Self {
        RebaseStatusOutcome {
            onto: status.onto,
            current_step: status.current_step,
            total_steps: status.total_steps,
            stopped_commit: status.stopped_commit,
            conflicted_files: status.conflicted_files,
            stop_reason: status.stop_reason.map(Into::into),
        }
    }
}

impl Query for RebaseStatusQuery {
    type Outcome = Option<RebaseStatusOutcome>;
    const NAME: &'static str = "rebase-status";

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn run(&self, ctx: &Ctx) -> Result<Option<RebaseStatusOutcome>, Error> {
        let status = jerry_git::rebase::rebase_status(&ctx.worktree_path)?;
        Ok(status.map(Into::into))
    }
}
