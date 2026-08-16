use std::path::PathBuf;
use std::process::ExitStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitExit {
    Code(i32),
    /// Unix only; elsewhere a codeless status is [`GitExit::Unknown`].
    Signal(i32),
    Unknown,
}

impl GitExit {
    pub(crate) fn from_status(status: &ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return GitExit::Code(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return GitExit::Signal(signal);
            }
        }
        GitExit::Unknown
    }
}

impl std::fmt::Display for GitExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitExit::Code(code) => write!(f, "exited with status {code}"),
            GitExit::Signal(signal) => write!(f, "was terminated by signal {signal}"),
            GitExit::Unknown => write!(f, "terminated abnormally"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to open git repository at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: Box<gix::open::Error>,
    },

    #[error("failed to read worktree information: {0}")]
    WorktreeIo(#[from] std::io::Error),

    #[error("failed to read HEAD: {0}")]
    Head(#[source] Box<gix::reference::find::existing::Error>),

    #[error("failed to resolve HEAD to a commit: {0}")]
    PeelHead(#[source] Box<gix::head::peel::Error>),

    #[error("failed to resolve branch to a commit: {0}")]
    PeelReference(#[source] Box<gix::reference::peel::Error>),

    #[error("failed to compute merge-base: {0}")]
    MergeBase(#[source] Box<gix::repository::merge_base::Error>),

    #[error("failed to start the commit graph walk: {0}")]
    RevWalk(#[source] Box<gix::traverse::commit::topo::Error>),

    #[error("failed while walking the commit graph: {0}")]
    RevWalkIter(#[source] Box<gix::traverse::commit::topo::Error>),

    #[error("failed to read a commit object: {0}")]
    RevWalkObject(#[source] Box<gix::object::find::existing::with_conversion::Error>),

    #[error("failed to decode a commit: {0}")]
    RevWalkDecode(#[source] Box<gix::objs::decode::Error>),

    #[error("failed to read a commit's committer time: {0}")]
    RevWalkCommit(#[source] Box<gix::object::commit::Error>),

    #[error("failed to open the reference store: {0}")]
    References(#[source] Box<gix::reference::iter::Error>),

    #[error("failed to iterate references: {0}")]
    ReferencesIter(#[source] Box<gix::reference::iter::init::Error>),

    /// `gix` yields broken refs rather than skipping them, so one fails the whole graph build
    /// instead of silently dropping the commits it might point at.
    #[error("failed to read a reference: {0}")]
    ReferenceEntry(Box<dyn std::error::Error + Send + Sync + 'static>),

    #[error("failed to run `git {args}`: {source}")]
    GitSpawn {
        args: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`git {args}` {exit}: {stderr}")]
    GitCommand {
        args: String,
        exit: GitExit,
        stderr: String,
    },

    #[error(
        "worktree at {path} has uncommitted changes (tracked or untracked); pass force=true to remove it anyway"
    )]
    DirtyWorktree { path: PathBuf },

    #[error("no default/base branch could be detected for this repository")]
    MergeNoBaseBranch,

    #[error("worktree is already on the base branch {branch:?}; nothing to merge")]
    MergeSourceIsBaseBranch { branch: String },

    #[error("worktree at {path} has no branch checked out (detached HEAD); nothing to merge")]
    MergeSourceDetached { path: PathBuf },

    #[error("worktree at {path} has no branch checked out (detached HEAD); nothing to merge into")]
    MergeTargetDetached { path: PathBuf },

    #[error("branch {branch:?} is already the branch checked out here; nothing to merge")]
    MergeSourceIsCurrentBranch { branch: String },

    /// git can only check a branch out in one worktree at a time.
    #[error(
        "base branch {branch:?} is not checked out in any worktree; check it out somewhere \
         before merging into it"
    )]
    MergeBaseBranchNotCheckedOut { branch: String },

    #[error("worktree at {path} has uncommitted changes; commit or discard them before merging")]
    MergeTargetDirty { path: PathBuf },

    #[error("conflicted file {path} is not valid UTF-8; cannot parse its conflict markers")]
    MergeConflictFileNotUtf8 { path: PathBuf },

    #[error("conflicted file {path} has malformed/unterminated conflict markers")]
    MergeMalformedConflictMarkers { path: PathBuf },

    /// Only reachable from a merge some other tool started: [`crate::merge::attempt_merge`]
    /// pins `merge.conflictStyle=merge`.
    #[error("conflicted file {path} uses diff3-style conflict markers, which are not supported")]
    MergeUnsupportedConflictStyle { path: PathBuf },

    #[error("no unresolved conflict hunk at index {index} in {path}")]
    MergeNoSuchHunk { path: PathBuf, index: usize },

    #[error("{path} still has unresolved conflict hunks; cannot write it back yet")]
    MergeFileNotFullyResolved { path: PathBuf },

    #[error("no merge is in progress at {path}; nothing to complete")]
    MergeNotInProgress { path: PathBuf },

    /// Defence in depth for conflicts a marker-based check cannot see, such as modify/delete
    /// or binary conflicts.
    #[error(
        "cannot complete the merge: {} file(s) are still unmerged: {}",
        paths.len(),
        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    MergeFilesStillConflicted { paths: Vec<PathBuf> },

    #[error("worktree at {path} has no uncommitted changes; nothing to commit")]
    NothingToCommit { path: PathBuf },

    #[error(
        "refusing to move HEAD in {path}: expected it to be {expected}, but it is now {actual} \
         (something else was committed there since)"
    )]
    HeadMovedSinceRecorded {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error(
        "refusing to amend HEAD in {path}: expected it to still be {expected}, but it is now \
         {actual} (the rebase already moved on, or something else changed HEAD)"
    )]
    RebaseAmendHeadMoved {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error(
        "refusing to amend HEAD in {path}: the index has real staged changes that were never \
         part of this amend"
    )]
    RebaseAmendIndexDirty { path: PathBuf },

    #[error(
        "cannot undo the first commit on a detached-HEAD worktree at {path}: no branch ref to \
         unmake"
    )]
    CommitHasNoParentAndNoBranch { path: PathBuf },

    #[error("worktree at {path} has no commits yet; nothing real to discard")]
    DiscardSourceUnborn { path: PathBuf },

    #[error("{path} is the repository's main worktree; it cannot be discarded")]
    DiscardSourceIsMainWorktree { path: PathBuf },

    /// `git stash push` can exit 0 without pushing anything - a dirty submodule pointer does
    /// this - so a missing or unchanged `refs/stash` is refused rather than force-removed over.
    #[error("failed to snapshot uncommitted changes in {path} before discarding it")]
    DiscardSnapshotFailed { path: PathBuf },

    #[error(
        "took a real snapshot of {path} (stash {stash}) but could not remove the worktree \
         itself: {source}. The stash is safe and recoverable (`git stash apply {stash}`), but \
         the worktree directory may now be in an inconsistent state"
    )]
    DiscardRemovalFailedAfterStash {
        path: PathBuf,
        stash: String,
        #[source]
        source: Box<Error>,
    },

    #[error("cannot undo: {path} is already occupied by another worktree or directory")]
    DiscardWorktreePathReoccupied { path: PathBuf },

    #[error(
        "cannot undo: branch {branch:?} no longer matches the state it was discarded in (moved, \
         deleted, or checked out elsewhere since)"
    )]
    DiscardBranchMovedOrReoccupied { branch: String },
}
