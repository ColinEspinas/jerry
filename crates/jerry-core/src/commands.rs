//! Git-locality Commands: each wraps one `jerry-git` mutation as a typed value with a
//! serializable outcome of paths and names, never file contents. Every one is `Denied` to
//! agents: git already gives an agent a merge, so exposing these adds surface, not capability.

use crate::command::{Command, Denied, Invocability, Locality};
use crate::ctx::Ctx;
use crate::error::{git_error_code, Error};
use jerry_git::merge::{self, ConflictedPath, MergeOutcome, MergeStart, UnmergeableReason};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Merge the caller's worktree branch into the repository's detected base branch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeAttempt {}

/// Merge `source_branch` into whatever the caller's worktree has checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeBranchIntoCurrent {
    pub source_branch: String,
}

/// Commit the merge in progress at `base_worktree_path`, once nothing is unmerged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeComplete {
    pub base_worktree_path: PathBuf,
}

/// Abort the merge in progress at `base_worktree_path`, restoring the base worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeAbort {
    pub base_worktree_path: PathBuf,
}

/// Stage one file whose conflict markers are all gone. The write itself is a plain disk edit;
/// only staging touches the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageResolved {
    pub worktree_path: PathBuf,
    /// Relative to `worktree_path`.
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum MergeAttemptOutcome {
    AlreadyUpToDate {
        base_branch: String,
    },
    /// Merged without conflicts and staged; `MergeComplete` commits it.
    Clean {
        base_branch: String,
        base_worktree_path: PathBuf,
        files: Vec<PathBuf>,
    },
    Conflicted {
        base_branch: String,
        base_worktree_path: PathBuf,
        clean_files: Vec<PathBuf>,
        conflicted: Vec<ConflictedPathReport>,
    },
}

/// One conflicted path as the wire sees it: where it is and what kind of work it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictedPathReport {
    pub path: PathBuf,
    pub kind: ConflictKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ConflictKind {
    /// Parseable markers; `remaining_hunks` of them are still unresolved.
    Text { remaining_hunks: usize },
    /// One side deleted the file and the other modified it; needs an explicit decision.
    ModifyDelete,
    /// No markers to edit; needs an explicit decision.
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeCompleteOutcome {
    pub base_worktree_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAbortOutcome {
    pub base_worktree_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageResolvedOutcome {
    pub path: PathBuf,
}

impl Command for MergeAttempt {
    type Outcome = MergeAttemptOutcome;
    const NAME: &'static str = "merge-attempt";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        merge::merge_preflight(&ctx.repo_path, &ctx.worktree_path)
            .map(|_| ())
            .map_err(denied_by_git)
    }

    fn execute(self, ctx: &Ctx) -> Result<MergeAttemptOutcome, Error> {
        let (start, outcome) = merge::attempt_merge(&ctx.repo_path, &ctx.worktree_path)?;
        report_outcome(start, outcome)
    }
}

impl Command for MergeBranchIntoCurrent {
    type Outcome = MergeAttemptOutcome;
    const NAME: &'static str = "merge-branch-into-current";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        merge::merge_into_current_preflight(&ctx.worktree_path, &self.source_branch)
            .map(|_| ())
            .map_err(denied_by_git)
    }

    fn execute(self, ctx: &Ctx) -> Result<MergeAttemptOutcome, Error> {
        let (start, outcome) =
            merge::attempt_merge_into_current(&ctx.worktree_path, &self.source_branch)?;
        report_outcome(start, outcome)
    }
}

impl Command for MergeComplete {
    type Outcome = MergeCompleteOutcome;
    const NAME: &'static str = "merge-complete";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
        require_merge_in_progress(&self.base_worktree_path)?;
        let unmerged = merge::unmerged_files(&self.base_worktree_path).map_err(denied_by_git)?;
        if unmerged.is_empty() {
            return Ok(());
        }
        Err(Denied::new(
            "merge-files-still-conflicted",
            format!(
                "{} still unmerged: {}",
                unmerged.len(),
                join_paths(&unmerged)
            ),
        ))
    }

    fn execute(self, _ctx: &Ctx) -> Result<MergeCompleteOutcome, Error> {
        merge::complete_merge(&self.base_worktree_path)?;
        Ok(MergeCompleteOutcome {
            base_worktree_path: self.base_worktree_path,
        })
    }
}

impl Command for MergeAbort {
    type Outcome = MergeAbortOutcome;
    const NAME: &'static str = "merge-abort";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
        require_merge_in_progress(&self.base_worktree_path)
    }

    fn execute(self, _ctx: &Ctx) -> Result<MergeAbortOutcome, Error> {
        merge::abort_merge(&self.base_worktree_path)?;
        Ok(MergeAbortOutcome {
            base_worktree_path: self.base_worktree_path,
        })
    }
}

impl Command for StageResolved {
    type Outcome = StageResolvedOutcome;
    const NAME: &'static str = "stage-resolved";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
        let file =
            merge::load_conflicted_file(&self.worktree_path, &self.path).map_err(denied_by_git)?;
        let remaining = file.remaining_conflicts();
        if remaining == 0 {
            return Ok(());
        }
        Err(Denied::new(
            "merge-file-not-fully-resolved",
            format!(
                "{} still has {remaining} unresolved conflict hunks",
                self.path.display()
            ),
        ))
    }

    fn execute(self, _ctx: &Ctx) -> Result<StageResolvedOutcome, Error> {
        merge::stage_conflict_resolution(&self.worktree_path, &self.path)?;
        Ok(StageResolvedOutcome { path: self.path })
    }
}

/// Mirrors `jerry_app::work_surface::agents::AgentKind`'s three labels - jerry-core cannot
/// depend on jerry-app (§1), so this is an independent wire type the app converts to/from at
/// the dispatch boundary (a `From<AgentSpec> for AgentKind` there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AgentSpec {
    Claude,
    Codex,
    Cursor,
}

impl AgentSpec {
    /// Parses a CLI-typed name case-insensitively (`claude`, `Claude`, `CLAUDE` all match);
    /// `None` for anything else, never a guessed kind.
    pub fn parse(name: &str) -> Option<AgentSpec> {
        match name.to_ascii_lowercase().as_str() {
            "claude" => Some(AgentSpec::Claude),
            "codex" => Some(AgentSpec::Codex),
            "cursor" => Some(AgentSpec::Cursor),
            _ => None,
        }
    }
}

/// Creates a sibling worktree on a new branch - see `docs/architecture/decisions.md` §21.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeCreate {
    pub branch: String,
    /// The start point for `branch`; `None` lets git pick its default (`HEAD`).
    pub from: Option<String>,
    pub agent: Option<AgentSpec>,
    pub prompt: Option<String>,
    /// Grants the spawned agent orchestrator policy (`docs/architecture/decisions.md` §26): the
    /// app applies `Settings.agents.orchestrator.grants` to its `SessionSpawn` when this is set,
    /// rather than spawning it as an ordinary agent. Meaningless without `agent` - `jerry wt new
    /// --orchestrator` with no `--agent` still creates a plain worktree with nothing to grant.
    #[serde(default)]
    pub orchestrator: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCreateOutcome {
    pub path: PathBuf,
}

impl Command for WorktreeCreate {
    type Outcome = WorktreeCreateOutcome;
    const NAME: &'static str = "worktree-create";

    fn invocability(&self) -> Invocability {
        Invocability::Allowed
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        worktree_target(&ctx.repo_path, &self.branch)
            .map(|_| ())
            .map_err(|error| Denied::new(error.code, error.message))
    }

    fn execute(self, ctx: &Ctx) -> Result<WorktreeCreateOutcome, Error> {
        let path = worktree_target(&ctx.repo_path, &self.branch)?;
        jerry_git::add_worktree(
            &ctx.repo_path,
            &path,
            Some(&self.branch),
            self.from.as_deref(),
        )?;
        Ok(WorktreeCreateOutcome { path })
    }
}

/// Where a new worktree for `branch` goes: a sibling of the main worktree, under
/// `<main-dir-name>-worktrees/<branch, sanitized>` - never nested inside the main worktree
/// itself, which would otherwise show up there as an untracked directory in `git status`.
/// `common_git_dir` is `Ctx::repo_path`, always the main worktree's own `.git` directory
/// regardless of which linked worktree the caller is acting from (§15).
///
/// `branch` is untrusted (an agent may call this Command directly), so every step here is a real
/// check, not just a happy-path derivation: `branch` must be a git-valid ref name before anything
/// else runs (rejects `..`, a leading `-`, `\`, and every other ref-format violation - see
/// [`jerry_git::is_valid_branch_name`]), the sanitized directory name it becomes must itself be
/// non-empty and not `.`/`..`, and the resulting path must land exactly one level under the
/// worktrees container - never escape it, however `branch` was spelled.
fn worktree_target(common_git_dir: &Path, branch: &str) -> Result<PathBuf, Error> {
    if !jerry_git::is_valid_branch_name(common_git_dir, branch)? {
        return Err(invalid_branch(branch));
    }
    let main_root = common_git_dir.parent().ok_or_else(|| {
        Error::new(
            "worktree-location-unavailable",
            format!("{} has no parent directory", common_git_dir.display()),
        )
    })?;
    let container_parent = main_root.parent().ok_or_else(|| {
        Error::new(
            "worktree-location-unavailable",
            format!(
                "{} has no parent directory to place a sibling worktree in",
                main_root.display()
            ),
        )
    })?;
    let repo_name = main_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    let container = container_parent.join(format!("{repo_name}-worktrees"));
    let target = container.join(sanitize_branch_for_path(branch)?);
    if !target.starts_with(&container) || target.parent() != Some(container.as_path()) {
        return Err(invalid_branch(branch));
    }
    if target.exists() {
        return Err(Error::new(
            "worktree-path-exists",
            format!("{} already exists", target.display()),
        ));
    }
    Ok(target)
}

fn invalid_branch(branch: &str) -> Error {
    Error::new(
        "worktree-branch-invalid",
        format!("{branch:?} is not a usable git branch name"),
    )
}

/// A branch name may contain `/`; a directory name may not treat it as a separator, and `\` is a
/// path separator on Windows - both become `-`. `jerry_git::is_valid_branch_name` already refuses
/// a branch containing either at the git level, so this is a second, independent line of defense
/// rather than the only one: nothing here trusts that check alone to keep `Path::join` from ever
/// seeing a rooted or `..` component.
fn sanitize_branch_for_path(branch: &str) -> Result<String, Error> {
    let sanitized = branch.replace(['/', '\\'], "-");
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        return Err(invalid_branch(branch));
    }
    Ok(sanitized)
}

/// One rebase todo row on the wire, mirroring `jerry_git::rebase::RebasePlanEntry` - never
/// deriving `Serialize` on the domain type itself (which would put a wire-format contract on a
/// pure-domain crate that has no `serde` dependency at all), so the wire shape and jerry-git's own
/// verb names can't silently diverge. Mirrors `ConflictKind`'s identical reasoning for merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RebasePlanEntryWire {
    /// A full object id, not abbreviated - `jerry_git::rebase::RebasePlanEntry::commit`'s own
    /// contract.
    pub commit: String,
    pub action: RebaseActionWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RebaseActionWire {
    Pick,
    Reword {
        #[serde(default)]
        message: Option<String>,
    },
    Edit,
    Squash,
    Fixup,
    Drop,
}

impl From<RebasePlanEntryWire> for jerry_git::rebase::RebasePlanEntry {
    fn from(wire: RebasePlanEntryWire) -> Self {
        jerry_git::rebase::RebasePlanEntry {
            commit: wire.commit,
            action: wire.action.into(),
        }
    }
}

impl From<RebaseActionWire> for jerry_git::rebase::RebaseAction {
    fn from(wire: RebaseActionWire) -> Self {
        use jerry_git::rebase::RebaseAction as A;
        match wire {
            RebaseActionWire::Pick => A::Pick,
            RebaseActionWire::Reword { message } => A::Reword(message),
            RebaseActionWire::Edit => A::Edit,
            RebaseActionWire::Squash => A::Squash,
            RebaseActionWire::Fixup => A::Fixup,
            RebaseActionWire::Drop => A::Drop,
        }
    }
}

/// The other direction - the app builds a plan in memory as `jerry_git::rebase` types (they are
/// plain data, so it already has them) and wires it up only to dispatch `RebaseStart`.
impl From<jerry_git::rebase::RebasePlanEntry> for RebasePlanEntryWire {
    fn from(entry: jerry_git::rebase::RebasePlanEntry) -> Self {
        RebasePlanEntryWire {
            commit: entry.commit,
            action: entry.action.into(),
        }
    }
}

impl From<jerry_git::rebase::RebaseAction> for RebaseActionWire {
    fn from(action: jerry_git::rebase::RebaseAction) -> Self {
        use jerry_git::rebase::RebaseAction as A;
        match action {
            A::Pick => RebaseActionWire::Pick,
            A::Reword(message) => RebaseActionWire::Reword { message },
            A::Edit => RebaseActionWire::Edit,
            A::Squash => RebaseActionWire::Squash,
            A::Fixup => RebaseActionWire::Fixup,
            A::Drop => RebaseActionWire::Drop,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReasonWire {
    Edit,
    RewordNeedsMessage,
}

impl From<jerry_git::rebase::StopReason> for StopReasonWire {
    fn from(reason: jerry_git::rebase::StopReason) -> Self {
        match reason {
            jerry_git::rebase::StopReason::Edit => StopReasonWire::Edit,
            jerry_git::rebase::StopReason::RewordNeedsMessage => StopReasonWire::RewordNeedsMessage,
        }
    }
}

impl From<StopReasonWire> for jerry_git::rebase::StopReason {
    fn from(reason: StopReasonWire) -> Self {
        match reason {
            StopReasonWire::Edit => jerry_git::rebase::StopReason::Edit,
            StopReasonWire::RewordNeedsMessage => jerry_git::rebase::StopReason::RewordNeedsMessage,
        }
    }
}

/// `jerry_git::rebase::RebaseOutcome`, mirrored: paths and names only, never file contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum RebaseOutcomeReport {
    Completed,
    StoppedForEdit {
        commit: String,
        reason: Option<StopReasonWire>,
    },
    StoppedForConflict {
        commit: String,
        conflicted_files: Vec<PathBuf>,
    },
}

impl From<jerry_git::rebase::RebaseOutcome> for RebaseOutcomeReport {
    fn from(outcome: jerry_git::rebase::RebaseOutcome) -> Self {
        match outcome {
            jerry_git::rebase::RebaseOutcome::Completed => RebaseOutcomeReport::Completed,
            jerry_git::rebase::RebaseOutcome::StoppedForEdit { commit, reason } => {
                RebaseOutcomeReport::StoppedForEdit {
                    commit,
                    reason: reason.map(Into::into),
                }
            }
            jerry_git::rebase::RebaseOutcome::StoppedForConflict {
                commit,
                conflicted_files,
            } => RebaseOutcomeReport::StoppedForConflict {
                commit,
                conflicted_files,
            },
        }
    }
}

/// The other direction - the app's rebase mode keeps its `RebasePhase` in terms of
/// `jerry_git::rebase::RebaseOutcome` (plain data it already has, reused rather than growing a
/// second local mirror), reconstructed from the wire `Report` every dispatch returns.
impl From<RebaseOutcomeReport> for jerry_git::rebase::RebaseOutcome {
    fn from(outcome: RebaseOutcomeReport) -> Self {
        match outcome {
            RebaseOutcomeReport::Completed => jerry_git::rebase::RebaseOutcome::Completed,
            RebaseOutcomeReport::StoppedForEdit { commit, reason } => {
                jerry_git::rebase::RebaseOutcome::StoppedForEdit {
                    commit,
                    reason: reason.map(Into::into),
                }
            }
            RebaseOutcomeReport::StoppedForConflict {
                commit,
                conflicted_files,
            } => jerry_git::rebase::RebaseOutcome::StoppedForConflict {
                commit,
                conflicted_files,
            },
        }
    }
}

/// Starts a real interactive rebase onto `onto`, driving `plan` to completion or the first stop.
/// `Denied` to agents: git already gives an agent a rebase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RebaseStart {
    pub onto: String,
    pub plan: Vec<RebasePlanEntryWire>,
}

/// Resumes a stopped rebase, driving it to completion or the next stop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RebaseContinue {}

/// Skips the commit a stopped rebase is at, driving it to completion or the next stop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RebaseSkip {}

/// Aborts an in-progress rebase, restoring the pre-rebase state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RebaseAbort {}

/// Amends the message of the commit a rebase is stopped at - applying a message obtained *after*
/// a message-less `reword` stop, which `jerry_git::rebase::start_interactive_rebase`'s own reword
/// queue can never pick up retroactively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AmendHeadMessage {
    pub message: String,
}

impl Command for RebaseStart {
    type Outcome = RebaseOutcomeReport;
    const NAME: &'static str = "rebase-start";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        jerry_git::rebase::rebase_preflight(&ctx.worktree_path).map_err(denied_by_git)?;
        require_jerry_binary()?;
        Ok(())
    }

    fn execute(self, ctx: &Ctx) -> Result<RebaseOutcomeReport, Error> {
        let jerry_binary = crate::jerry_binary::locate().ok_or_else(jerry_binary_not_found)?;
        let plan: Vec<jerry_git::rebase::RebasePlanEntry> =
            self.plan.into_iter().map(Into::into).collect();
        let outcome = jerry_git::rebase::start_interactive_rebase(
            &ctx.worktree_path,
            &self.onto,
            &plan,
            &jerry_binary,
        )?;
        Ok(outcome.into())
    }
}

impl Command for RebaseContinue {
    type Outcome = RebaseOutcomeReport;
    const NAME: &'static str = "rebase-continue";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        require_rebase_in_progress(&ctx.worktree_path)
    }

    fn execute(self, ctx: &Ctx) -> Result<RebaseOutcomeReport, Error> {
        let outcome = jerry_git::rebase::continue_rebase(&ctx.worktree_path)?;
        Ok(outcome.into())
    }
}

impl Command for RebaseSkip {
    type Outcome = RebaseOutcomeReport;
    const NAME: &'static str = "rebase-skip";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        require_rebase_in_progress(&ctx.worktree_path)
    }

    fn execute(self, ctx: &Ctx) -> Result<RebaseOutcomeReport, Error> {
        let outcome = jerry_git::rebase::skip_rebase_commit(&ctx.worktree_path)?;
        Ok(outcome.into())
    }
}

impl Command for RebaseAbort {
    type Outcome = ();
    const NAME: &'static str = "rebase-abort";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        require_rebase_in_progress(&ctx.worktree_path)
    }

    fn execute(self, ctx: &Ctx) -> Result<(), Error> {
        jerry_git::rebase::abort_rebase(&ctx.worktree_path)?;
        Ok(())
    }
}

impl Command for AmendHeadMessage {
    type Outcome = ();
    const NAME: &'static str = "amend-head-message";

    fn invocability(&self) -> Invocability {
        Invocability::Denied
    }

    fn locality(&self) -> Locality {
        Locality::Git
    }

    fn validate(&self, ctx: &Ctx) -> Result<(), Denied> {
        require_rebase_stopped(&ctx.worktree_path)
    }

    fn execute(self, ctx: &Ctx) -> Result<(), Error> {
        let status = jerry_git::rebase::rebase_status(&ctx.worktree_path)?;
        let stopped_commit = status
            .and_then(|status| status.stopped_commit)
            .ok_or_else(|| {
                Error::new(
                    "rebase-not-stopped",
                    format!("no rebase is stopped at {}", ctx.worktree_path.display()),
                )
            })?;
        jerry_git::rebase::amend_head_message(&ctx.worktree_path, &stopped_commit, &self.message)?;
        Ok(())
    }
}

fn require_rebase_in_progress(worktree_path: &Path) -> Result<(), Denied> {
    match jerry_git::rebase::rebase_status(worktree_path) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(Denied::new(
            "rebase-not-in-progress",
            format!("no rebase is in progress at {}", worktree_path.display()),
        )),
        Err(error) => Err(denied_by_git(error)),
    }
}

fn require_rebase_stopped(worktree_path: &Path) -> Result<(), Denied> {
    match jerry_git::rebase::rebase_status(worktree_path) {
        Ok(Some(status)) if status.stopped_commit.is_some() => Ok(()),
        Ok(_) => Err(Denied::new(
            "rebase-not-stopped",
            format!("no rebase is stopped at {}", worktree_path.display()),
        )),
        Err(error) => Err(denied_by_git(error)),
    }
}

fn require_jerry_binary() -> Result<(), Denied> {
    if crate::jerry_binary::locate().is_some() {
        return Ok(());
    }
    Err(Denied::new(
        "jerry-binary-not-found",
        "could not locate the jerry binary needed to drive git's rebase editor hooks",
    ))
}

fn jerry_binary_not_found() -> Error {
    Error::new(
        "jerry-binary-not-found",
        "could not locate the jerry binary needed to drive git's rebase editor hooks",
    )
}

fn require_merge_in_progress(base_worktree_path: &Path) -> Result<(), Denied> {
    match merge::merge_head_exists(base_worktree_path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Denied::new(
            "merge-not-in-progress",
            format!(
                "no merge is in progress at {}",
                base_worktree_path.display()
            ),
        )),
        Err(error) => Err(denied_by_git(error)),
    }
}

fn denied_by_git(error: jerry_git::Error) -> Denied {
    Denied::new(git_error_code(&error), error.to_string())
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Projects git's outcome onto the wire: paths and hunk counts, classified from git's own
/// index so a modify/delete or binary conflict is never mistaken for an editable one.
fn report_outcome(start: MergeStart, outcome: MergeOutcome) -> Result<MergeAttemptOutcome, Error> {
    Ok(match outcome {
        MergeOutcome::AlreadyUpToDate => MergeAttemptOutcome::AlreadyUpToDate {
            base_branch: start.base_branch,
        },
        MergeOutcome::Clean { files } => MergeAttemptOutcome::Clean {
            base_branch: start.base_branch,
            base_worktree_path: start.base_worktree_path,
            files,
        },
        MergeOutcome::Conflicted {
            conflicted_files,
            clean_files,
        } => {
            let mut conflicted = Vec::with_capacity(conflicted_files.len());
            for path in conflicted_files {
                let kind = match merge::classify_conflicted_file(&start.base_worktree_path, &path)?
                {
                    ConflictedPath::Text(file) => ConflictKind::Text {
                        remaining_hunks: file.remaining_conflicts(),
                    },
                    ConflictedPath::Unmergeable {
                        reason: UnmergeableReason::ModifyDelete,
                        ..
                    } => ConflictKind::ModifyDelete,
                    ConflictedPath::Unmergeable {
                        reason: UnmergeableReason::Binary,
                        ..
                    } => ConflictKind::Binary,
                };
                conflicted.push(ConflictedPathReport { path, kind });
            }
            MergeAttemptOutcome::Conflicted {
                base_branch: start.base_branch,
                base_worktree_path: start.base_worktree_path,
                clean_files,
                conflicted,
            }
        }
    })
}

#[cfg(test)]
mod worktree_create_tests {
    use super::{Command, WorktreeCreate, WorktreeCreateOutcome};
    use crate::command::{run_command, validate_command, Invocability};
    use crate::ctx::{Caller, Ctx};
    use crate::report::Report;
    use std::path::Path;
    use test_support::{commit, git_output, seed_repo};

    fn ctx_for(worktree: &Path) -> Ctx {
        Ctx::from_cwd(worktree, Caller::Human).expect("ctx")
    }

    fn worktree_create(branch: &str) -> WorktreeCreate {
        WorktreeCreate {
            branch: branch.to_owned(),
            from: None,
            agent: None,
            prompt: None,
            orchestrator: false,
        }
    }

    fn outcome(report: Report) -> WorktreeCreateOutcome {
        match report {
            Report::Ok { outcome } => serde_json::from_value(outcome).expect("outcome"),
            other => panic!("expected ok, got {other:?}"),
        }
    }

    /// The sibling directory this test's worktrees land in never lives inside `seed_repo`'s own
    /// tempdir (see `worktree_target`'s docs), so nothing removes it automatically.
    fn cleanup(worktree_path: &Path) {
        if let Some(container) = worktree_path.parent() {
            let _ = std::fs::remove_dir_all(container);
        }
    }

    #[test]
    fn agents_may_call_it_unlike_merge_and_rebase() {
        assert_eq!(worktree_create("x").invocability(), Invocability::Allowed);
    }

    #[test]
    fn a_fresh_branch_creates_a_sibling_worktree_and_reports_its_path() {
        let repo = seed_repo();
        let ctx = ctx_for(repo.path());

        assert_eq!(
            validate_command(&worktree_create("feature-a"), &ctx),
            Report::Ok {
                outcome: serde_json::Value::Null
            }
        );

        let created = outcome(run_command(worktree_create("feature-a"), &ctx));
        assert!(created.path.is_dir(), "{}", created.path.display());
        assert!(
            !created.path.starts_with(repo.path()),
            "a Jerry worktree must be a sibling, never nested inside the main checkout"
        );
        assert_eq!(
            created.path.file_name().and_then(|n| n.to_str()),
            Some("feature-a")
        );
        let repo_name = repo
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("repo dir name");
        assert_eq!(
            created
                .path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some(format!("{repo_name}-worktrees").as_str())
        );
        let branch = git_output(&created.path, &["branch", "--show-current"]);
        assert_eq!(branch.trim(), "feature-a");
        cleanup(&created.path);
    }

    #[test]
    fn a_branch_name_with_a_slash_becomes_a_flat_directory_name() {
        let repo = seed_repo();
        let ctx = ctx_for(repo.path());

        let created = outcome(run_command(worktree_create("feature/login"), &ctx));
        assert_eq!(
            created.path.file_name().and_then(|n| n.to_str()),
            Some("feature-login")
        );
        let branch = git_output(&created.path, &["branch", "--show-current"]);
        assert_eq!(branch.trim(), "feature/login");
        cleanup(&created.path);
    }

    #[test]
    fn a_colliding_target_path_is_denied_without_touching_git_again() {
        let repo = seed_repo();
        let ctx = ctx_for(repo.path());
        let created = outcome(run_command(worktree_create("feature-b"), &ctx));

        let denied = validate_command(&worktree_create("feature-b"), &ctx);
        assert!(
            matches!(denied, Report::Denied { ref code, .. } if code == "worktree-path-exists"),
            "{denied:?}"
        );
        cleanup(&created.path);
    }

    #[test]
    fn from_picks_the_branchs_real_start_point() {
        let repo = seed_repo();
        commit(repo.path(), "a.txt", "1\n", "commit 1");
        let base = git_output(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        commit(repo.path(), "b.txt", "2\n", "commit 2");
        let ctx = ctx_for(repo.path());

        let created = outcome(run_command(
            WorktreeCreate {
                branch: "from-base".into(),
                from: Some(base),
                agent: None,
                prompt: None,
                orchestrator: false,
            },
            &ctx,
        ));
        assert!(created.path.join("a.txt").exists());
        assert!(
            !created.path.join("b.txt").exists(),
            "must start from the given ref, before the later commit"
        );
        cleanup(&created.path);
    }

    /// An agent may call this Command directly (`Invocability::Allowed`), so `branch` is
    /// untrusted input: none of these must ever create a worktree outside the intended
    /// `<repo>-worktrees` container, however `branch` is spelled.
    #[test]
    fn a_branch_that_would_escape_the_worktrees_container_is_denied_and_creates_nothing() {
        let repo = seed_repo();
        let ctx = ctx_for(repo.path());
        let container = repo.path().parent().expect("parent").join(format!(
            "{}-worktrees",
            repo.path().file_name().expect("name").to_string_lossy()
        ));

        for branch in [
            r"\Temp\x",
            r"..\..\evil",
            "..",
            ".",
            "",
            "-evil",
            "a/../../b",
            "C:evil",
        ] {
            let denied = validate_command(&worktree_create(branch), &ctx);
            assert!(
                matches!(denied, Report::Denied { ref code, .. } if code == "worktree-branch-invalid"),
                "{branch:?}: {denied:?}"
            );
            let executed = run_command(worktree_create(branch), &ctx);
            assert!(
                matches!(executed, Report::Denied { ref code, .. } if code == "worktree-branch-invalid"),
                "{branch:?}: {executed:?}"
            );
        }
        assert!(
            !container.exists(),
            "nothing must be created outside a genuinely valid branch"
        );
    }

    #[test]
    fn an_ordinary_slash_branch_still_creates_a_real_contained_worktree() {
        let repo = seed_repo();
        let ctx = ctx_for(repo.path());
        let created = outcome(run_command(worktree_create("feature/still-works"), &ctx));
        assert!(created.path.is_dir());
        let container = created.path.parent().expect("parent");
        assert!(created.path.starts_with(container));
        cleanup(&created.path);
    }
}

#[cfg(test)]
mod merge_command_tests {
    use super::{
        ConflictKind, MergeAbort, MergeAttempt, MergeAttemptOutcome, MergeComplete, StageResolved,
    };
    use crate::command::{run_command, validate_command};
    use crate::ctx::{Caller, Ctx};
    use crate::report::Report;
    use std::fs;
    use std::path::{Path, PathBuf};
    use test_support::{add_worktree, commit, git_output, seed_repo, write_file, TempDir};

    /// A repo with `main` checked out at its root and a linked worktree on `feature`, kept
    /// outside the main checkout so the base worktree stays clean.
    fn repo_with_feature() -> (TempDir, TempDir, PathBuf) {
        let repo = seed_repo();
        // A file both sides will edit: a true three-stage conflict, not an add/add one.
        commit(repo.path(), "shared.txt", "base\n", "seed shared.txt");
        let outside = TempDir::new().expect("tempdir");
        let feature = outside.path().join("wt-feature");
        add_worktree(repo.path(), "feature", &feature);
        (repo, outside, feature)
    }

    fn ctx_for(worktree: &Path) -> Ctx {
        Ctx::from_cwd(worktree, Caller::Human).expect("ctx")
    }

    fn outcome(report: Report) -> MergeAttemptOutcome {
        match report {
            Report::Ok { outcome } => serde_json::from_value(outcome).expect("outcome"),
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_merge_reports_its_files_and_complete_commits_it() {
        let (_repo, _outside, feature) = repo_with_feature();
        commit(&feature, "new.txt", "hello\n", "add new.txt");

        let ctx = ctx_for(&feature);
        assert_eq!(
            validate_command(&MergeAttempt::default(), &ctx),
            Report::Ok {
                outcome: serde_json::Value::Null
            }
        );
        let MergeAttemptOutcome::Clean {
            base_branch,
            base_worktree_path,
            files,
        } = outcome(run_command(MergeAttempt::default(), &ctx))
        else {
            panic!("expected a clean merge");
        };
        assert_eq!(base_branch, "main");
        assert_eq!(files, vec![PathBuf::from("new.txt")]);

        let complete = run_command(
            MergeComplete {
                base_worktree_path: base_worktree_path.clone(),
            },
            &ctx,
        );
        assert!(complete.is_ok(), "{complete:?}");
        assert!(base_worktree_path.join("new.txt").exists());
        assert!(!jerry_git::merge::merge_head_exists(&base_worktree_path).expect("head"));
    }

    #[test]
    fn a_dirty_base_is_refused_by_validate_before_anything_runs() {
        let (repo, _outside, feature) = repo_with_feature();
        commit(&feature, "new.txt", "hello\n", "add new.txt");
        write_file(repo.path(), "dirty.txt", "uncommitted\n");

        let report = validate_command(&MergeAttempt::default(), &ctx_for(&feature));
        assert!(
            matches!(report, Report::Denied { ref code, .. } if code == "merge-target-dirty"),
            "{report:?}"
        );
    }

    #[test]
    fn conflicts_are_reported_by_path_and_hunk_count_then_staged_and_completed() {
        let (repo, _outside, feature) = repo_with_feature();
        commit(repo.path(), "shared.txt", "base\nmain side\n", "main edit");
        commit(
            &feature,
            "shared.txt",
            "base\nfeature side\n",
            "feature edit",
        );

        let ctx = ctx_for(&feature);
        let MergeAttemptOutcome::Conflicted {
            base_worktree_path,
            conflicted,
            ..
        } = outcome(run_command(MergeAttempt::default(), &ctx))
        else {
            panic!("expected conflicts");
        };
        assert_eq!(conflicted.len(), 1);
        assert_eq!(conflicted[0].path, PathBuf::from("shared.txt"));
        assert_eq!(
            conflicted[0].kind,
            ConflictKind::Text { remaining_hunks: 1 }
        );

        let complete = MergeComplete {
            base_worktree_path: base_worktree_path.clone(),
        };
        assert!(
            matches!(validate_command(&complete, &ctx), Report::Denied { ref code, .. } if code == "merge-files-still-conflicted")
        );

        let stage = StageResolved {
            worktree_path: base_worktree_path.clone(),
            path: PathBuf::from("shared.txt"),
        };
        assert!(
            matches!(validate_command(&stage, &ctx), Report::Denied { ref code, .. } if code == "merge-file-not-fully-resolved")
        );
        fs::write(base_worktree_path.join("shared.txt"), "base\nboth sides\n").expect("resolve");
        assert!(run_command(stage, &ctx).is_ok());
        assert!(run_command(complete, &ctx).is_ok());
        let log = git_output(&base_worktree_path, &["log", "--oneline", "-1"]);
        assert!(log.contains("Merge"), "{log}");
    }

    #[test]
    fn abort_needs_a_merge_in_progress_and_then_restores_the_base() {
        let (repo, _outside, feature) = repo_with_feature();
        let ctx = ctx_for(&feature);
        let abort = MergeAbort {
            base_worktree_path: repo.path().to_path_buf(),
        };
        assert!(
            matches!(validate_command(&abort, &ctx), Report::Denied { ref code, .. } if code == "merge-not-in-progress")
        );

        commit(repo.path(), "shared.txt", "base\nmain side\n", "main edit");
        commit(
            &feature,
            "shared.txt",
            "base\nfeature side\n",
            "feature edit",
        );
        assert!(matches!(
            outcome(run_command(MergeAttempt::default(), &ctx)),
            MergeAttemptOutcome::Conflicted { .. }
        ));
        assert!(run_command(abort, &ctx).is_ok());
        assert!(!jerry_git::merge::merge_head_exists(repo.path()).expect("head"));
    }
}
