use std::path::PathBuf;
use std::process::ExitStatus;

/// How a spawned `git` process terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitExit {
    /// The process exited normally with the given exit code.
    Code(i32),
    /// The process was terminated by the given signal number (e.g. `SIGKILL`, `SIGSEGV`).
    /// Only ever produced on Unix; a non-zero-but-codeless status on other platforms is
    /// reported as [`GitExit::Unknown`].
    Signal(i32),
    /// The process terminated abnormally and neither an exit code nor a signal number
    /// could be determined.
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

/// Errors produced by `wt-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to open the git repository at the given path via `gix`.
    #[error("failed to open git repository at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: Box<gix::open::Error>,
    },

    /// An I/O error occurred while reading worktree metadata from the filesystem, or while
    /// communicating with a spawned `git` process (e.g. its stdout pipe failed mid-read).
    #[error("failed to read worktree information: {0}")]
    WorktreeIo(#[from] std::io::Error),

    /// Failed to read the `HEAD` reference of a worktree.
    #[error("failed to read HEAD: {0}")]
    Head(#[source] Box<gix::reference::find::existing::Error>),

    /// Failed to resolve `HEAD` to a commit id.
    #[error("failed to resolve HEAD to a commit: {0}")]
    PeelHead(#[source] Box<gix::head::peel::Error>),

    /// Failed to compute the merge-base between a worktree's `HEAD` and the detected
    /// default branch.
    #[error("failed to compute merge-base: {0}")]
    MergeBase(#[source] Box<gix::repository::merge_base::Error>),

    /// Failed to spawn the `git` process.
    #[error("failed to run `git {args}`: {source}")]
    GitSpawn {
        args: String,
        #[source]
        source: std::io::Error,
    },

    /// The `git` process ran but exited with a non-zero status (or was killed by a
    /// signal; see [`GitExit`]).
    #[error("`git {args}` {exit}: {stderr}")]
    GitCommand {
        args: String,
        exit: GitExit,
        stderr: String,
    },

    /// Refused to remove a worktree with uncommitted changes (modified tracked files or
    /// untracked files) because `force` was not set.
    #[error(
        "worktree at {path} has uncommitted changes (tracked or untracked); pass force=true to remove it anyway"
    )]
    DirtyWorktree { path: PathBuf },

    /// [`crate::merge::attempt_merge`] could not detect any default/base branch to merge
    /// into (same "no fabricated answer" contract as [`crate::diff::DiffBase::NoBaseFound`]).
    #[error("no default/base branch could be detected for this repository")]
    MergeNoBaseBranch,

    /// The session worktree's own branch *is* the detected base branch - there is nothing
    /// meaningful to merge.
    #[error("worktree is already on the base branch {branch:?}; nothing to merge")]
    MergeSourceIsBaseBranch { branch: String },

    /// The session worktree's `HEAD` is detached (no branch checked out), so there is no
    /// branch name to hand to `git merge`.
    #[error("worktree at {path} has no branch checked out (detached HEAD); nothing to merge")]
    MergeSourceDetached { path: PathBuf },

    /// The detected base branch is not checked out in *any* worktree of this repository, so
    /// there is nowhere to run `git merge` from - see `crate::merge`'s module docs for why a
    /// worktree already checked out on the base branch is required (a branch checked out in
    /// one worktree can never be checked out, even temporarily, in another).
    #[error(
        "base branch {branch:?} is not checked out in any worktree; check it out somewhere \
         before merging into it"
    )]
    MergeBaseBranchNotCheckedOut { branch: String },

    /// Refused to attempt a merge because the worktree the merge would run in (the one with
    /// the base branch checked out) has uncommitted changes: `git merge` would either refuse
    /// outright or risk overwriting real, unrelated work in progress there.
    #[error(
        "worktree at {path} (base branch) has uncommitted changes; commit or discard them \
         before merging"
    )]
    MergeTargetDirty { path: PathBuf },

    /// A conflicted file's real on-disk content was not valid UTF-8, so its conflict markers
    /// could not be parsed as text.
    #[error("conflicted file {path} is not valid UTF-8; cannot parse its conflict markers")]
    MergeConflictFileNotUtf8 { path: PathBuf },

    /// A conflicted file's real conflict markers did not close (`<<<<<<<`/`=======`/
    /// `>>>>>>>` all present and in order) - unexpected for a file `git merge` itself just
    /// produced, but this module is a real *parser* of the file's real bytes, not a trusted
    /// oracle, so a malformed file is reported rather than corrupting content on write-back.
    #[error("conflicted file {path} has malformed/unterminated conflict markers")]
    MergeMalformedConflictMarkers { path: PathBuf },

    /// A conflicted file uses `diff3`-style conflict markers (`|||||||` base-content
    /// section), which this parser does not understand - [`crate::merge::attempt_merge`]
    /// always pins `merge.conflictStyle=merge` for merges it starts itself (see that
    /// function's docs), so this should only ever arise from a merge some other tool started
    /// with a different `merge.conflictStyle`.
    #[error("conflicted file {path} uses diff3-style conflict markers, which are not supported")]
    MergeUnsupportedConflictStyle { path: PathBuf },

    /// [`crate::merge::resolve_hunk`] was asked to resolve a hunk index that either doesn't
    /// exist in the file, or was already resolved (turned into an ordinary, non-conflicted
    /// segment by an earlier call).
    #[error("no unresolved conflict hunk at index {index} in {path}")]
    MergeNoSuchHunk { path: PathBuf, index: usize },

    /// [`crate::merge::write_resolved_file`] was called on a file that still has one or more
    /// unresolved conflict hunks - writing it back to disk in that state would leave real
    /// `<<<<<<<`/`=======`/`>>>>>>>` conflict markers in the file, which this refuses to do.
    #[error("{path} still has unresolved conflict hunks; cannot write it back yet")]
    MergeFileNotFullyResolved { path: PathBuf },

    /// [`crate::merge::complete_merge`] was called on a worktree with no `MERGE_HEAD` at all -
    /// there is no real in-progress merge to finish committing.
    #[error("no merge is in progress at {path}; nothing to complete")]
    MergeNotInProgress { path: PathBuf },

    /// [`crate::merge::complete_merge`] was called while `git` still reports real unmerged
    /// paths (`git diff --name-only --diff-filter=U`) - real defense in depth against
    /// committing a merge the UI *believes* is fully resolved but isn't, e.g. a modify/delete
    /// or binary conflict a caller's own conflict-marker-based "is this resolved" check can't
    /// see (see [`crate::merge::ConflictedPath`]'s docs).
    #[error(
        "cannot complete the merge: {} file(s) are still unmerged: {}",
        paths.len(),
        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    MergeFilesStillConflicted { paths: Vec<PathBuf> },
}
