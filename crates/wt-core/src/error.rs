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

    /// Failed to resolve a real, already-found branch reference to a commit id -
    /// [`crate::graph::resolve_commit`]'s own peel step, distinct from [`Self::PeelHead`] because
    /// the reference being peeled here is an ordinary branch, never `HEAD` itself.
    #[error("failed to resolve branch to a commit: {0}")]
    PeelReference(#[source] Box<gix::reference::peel::Error>),

    /// Failed to compute the merge-base between a worktree's `HEAD` and the detected
    /// default branch.
    #[error("failed to compute merge-base: {0}")]
    MergeBase(#[source] Box<gix::repository::merge_base::Error>),

    /// [`crate::graph::build_graph`] failed to configure the commit walk (e.g. a bad `git`
    /// commit-graph cache).
    #[error("failed to start the commit graph walk: {0}")]
    RevWalk(#[source] Box<gix::revision::walk::Error>),

    /// [`crate::graph::build_graph`] failed while stepping the commit walk (a corrupt or
    /// unreadable object encountered mid-traversal).
    #[error("failed while walking the commit graph: {0}")]
    RevWalkIter(#[source] Box<gix::revision::walk::iter::Error>),

    /// [`crate::graph::build_graph`] could not read a commit object the walk yielded an id for.
    #[error("failed to read a commit object: {0}")]
    RevWalkObject(#[source] Box<gix::object::find::existing::Error>),

    /// [`crate::graph::build_graph`] could not decode a commit's message or author signature.
    #[error("failed to decode a commit: {0}")]
    RevWalkDecode(#[source] Box<gix::objs::decode::Error>),

    /// [`crate::graph::build_graph`] could not decode a commit's committer time.
    #[error("failed to read a commit's committer time: {0}")]
    RevWalkCommit(#[source] Box<gix::object::commit::Error>),

    /// [`crate::graph::build_graph`] failed to open the repository's reference store.
    #[error("failed to open the reference store: {0}")]
    References(#[source] Box<gix::reference::iter::Error>),

    /// [`crate::graph::build_graph`] failed to start iterating references.
    #[error("failed to iterate references: {0}")]
    ReferencesIter(#[source] Box<gix::reference::iter::init::Error>),

    /// [`crate::graph::build_graph`] encountered a broken or unparsable reference while
    /// enumerating them. Per [`gix::Repository::references`]'s own docs, even broken refs are
    /// yielded (not silently skipped) so a caller can decide how to handle them; this crate
    /// treats one broken ref as a hard error for the whole graph build rather than silently
    /// dropping commits it might point at.
    #[error("failed to read a reference: {0}")]
    ReferenceEntry(Box<dyn std::error::Error + Send + Sync + 'static>),

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

    /// [`crate::merge::attempt_merge_into_current`]'s mirror of [`Error::MergeSourceDetached`]
    /// for the opposite direction: the *target* worktree's `HEAD` is detached, so there is no
    /// branch to merge into - `git merge` would move a detached `HEAD` instead, leaving the
    /// merge commit on no branch at all.
    #[error("worktree at {path} has no branch checked out (detached HEAD); nothing to merge into")]
    MergeTargetDetached { path: PathBuf },

    /// [`crate::merge::attempt_merge_into_current`]'s mirror of
    /// [`Error::MergeSourceIsBaseBranch`]: the branch asked for *is* the one already checked out
    /// in the target worktree, so there is nothing meaningful to merge.
    #[error("branch {branch:?} is already the branch checked out here; nothing to merge")]
    MergeSourceIsCurrentBranch { branch: String },

    /// The detected base branch is not checked out in *any* worktree of this repository, so
    /// there is nowhere to run `git merge` from - see `crate::merge`'s module docs for why a
    /// worktree already checked out on the base branch is required (a branch checked out in
    /// one worktree can never be checked out, even temporarily, in another).
    #[error(
        "base branch {branch:?} is not checked out in any worktree; check it out somewhere \
         before merging into it"
    )]
    MergeBaseBranchNotCheckedOut { branch: String },

    /// Refused to attempt a merge because the worktree the merge would run in has uncommitted
    /// changes: `git merge` would either refuse outright or risk overwriting real, unrelated work
    /// in progress there. Shared by both directions - the worktree with the detected base branch
    /// checked out for [`crate::merge::attempt_merge`], the caller-supplied target worktree for
    /// [`crate::merge::attempt_merge_into_current`] - since `path` already names exactly which
    /// worktree is dirty either way.
    #[error("worktree at {path} has uncommitted changes; commit or discard them before merging")]
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

    /// [`crate::undo::commit_all_changes`] was called on a worktree with no uncommitted
    /// changes at all - there is nothing real to commit.
    #[error("worktree at {path} has no uncommitted changes; nothing to commit")]
    NothingToCommit { path: PathBuf },

    /// [`crate::undo::undo_commit_all_changes`]/[`crate::undo::redo_commit_all_changes`]'s
    /// mandatory identity guard: `HEAD` no longer matches the commit id the guard expected -
    /// something else (another `ade` action, or an external `git` call) moved it since the
    /// state this undo/redo was computed from, so moving `HEAD` now would silently discard
    /// whatever that was.
    #[error(
        "refusing to move HEAD in {path}: expected it to be {expected}, but it is now {actual} \
         (something else was committed there since)"
    )]
    HeadMovedSinceRecorded {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    /// [`crate::rebase::amend_head_message`]'s own identity guard: `HEAD` no longer matches the
    /// commit id the caller expected to be amending - the rebase already advanced past this stop,
    /// something else moved `HEAD` in the meantime, or the caller is stale. Refuses rather than
    /// amending whatever commit `HEAD` now happens to be, the same "never silently discard
    /// something else's real work" reasoning [`Error::HeadMovedSinceRecorded`] documents.
    #[error(
        "refusing to amend HEAD in {path}: expected it to still be {expected}, but it is now \
         {actual} (the rebase already moved on, or something else changed HEAD)"
    )]
    RebaseAmendHeadMoved {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    /// [`crate::rebase::amend_head_message`] found real staged changes (`git diff --cached`)
    /// already in the index before amending. Refused rather than silently folding unrelated
    /// staged content (something else - an unpaused agent, a stray `git add` - added while the
    /// rebase was stopped) into the amended commit alongside the real message change.
    #[error(
        "refusing to amend HEAD in {path}: the index has real staged changes that were never \
         part of this amend"
    )]
    RebaseAmendIndexDirty { path: PathBuf },

    /// [`crate::undo::undo_commit_all_changes`] needs to un-make a branch that had no parent
    /// commit (the very first commit ever made on it), but the worktree's `HEAD` isn't a real
    /// branch (detached) - there is no ref for `git update-ref -d` to remove. Not reachable in
    /// practice for a worktree `crate::add_worktree` created (always a real branch), kept as a
    /// real, explicit refusal rather than an unreachable panic.
    #[error(
        "cannot undo the first commit on a detached-HEAD worktree at {path}: no branch ref to \
         unmake"
    )]
    CommitHasNoParentAndNoBranch { path: PathBuf },

    /// [`crate::undo::discard_worktree`] was called on a worktree whose `HEAD` has no commit at
    /// all yet (a freshly initialized repository/branch that was never committed to) - there is
    /// no real commit id to snapshot, and `git stash` itself refuses to run with no `HEAD`
    /// commit to diff against.
    #[error("worktree at {path} has no commits yet; nothing real to discard")]
    DiscardSourceUnborn { path: PathBuf },

    /// [`crate::undo::discard_worktree`] was called on the repository's *main* worktree.
    /// `git worktree remove --force --force` can never remove it (git itself always refuses
    /// outright) - live-reproduced as a real data-loss path in an earlier version of that
    /// function (stashed real content, then failed to remove anything, with nothing left
    /// pointing back at the stash) before this refusal was added; see that function's own docs.
    #[error("{path} is the repository's main worktree; it cannot be discarded")]
    DiscardSourceIsMainWorktree { path: PathBuf },

    /// [`crate::undo::discard_worktree`] found the worktree dirty (real uncommitted/untracked
    /// content) but `git stash push` either produced no real stash commit id to store, or (a
    /// real, live-reproduced case - see [`crate::undo::push_and_capture_stash`]'s own docs)
    /// exited successfully without actually pushing anything at all (e.g. a dirty submodule
    /// pointer, which `git stash push` cannot capture), leaving `refs/stash` pointing at
    /// whatever unrelated stash existed before. Refused rather than proceeding with
    /// `remove_worktree(force: true)` uncaptured or captured-under-the-wrong-sha, which is the
    /// entire reason this function exists over a bare force-remove.
    #[error("failed to snapshot uncommitted changes in {path} before discarding it")]
    DiscardSnapshotFailed { path: PathBuf },

    /// [`crate::undo::discard_worktree`] took a real stash snapshot, but the worktree removal
    /// itself then failed for some other real reason (a filesystem permission error, a lock
    /// `--force --force` didn't override, ...). Live-reproduced: by the time this happens, `git
    /// worktree remove` has typically already deleted the worktree's own contents and
    /// deregistered it as a worktree entirely, failing only at the final, now-empty directory
    /// entry's own removal - so there is no reliable way to restore the stash back into that
    /// directory in place (it may no longer even be a valid git worktree any more). The stash
    /// itself is still real, durable, and independently recoverable regardless: `stash` names
    /// it, and `git stash apply <stash>`/`git stash list` from any worktree of this repository
    /// can get it back by hand.
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

    /// [`crate::undo::undo_discard_worktree`]'s mandatory identity guard: something already
    /// occupies the worktree's original path.
    #[error("cannot undo: {path} is already occupied by another worktree or directory")]
    DiscardWorktreePathReoccupied { path: PathBuf },

    /// [`crate::undo::undo_discard_worktree`]'s mandatory identity guard: the recorded branch no
    /// longer exists, is checked out in a different worktree already, or its tip commit is no
    /// longer what was recorded at discard time - recreating the worktree would either clobber
    /// whatever now occupies that branch or resurrect stale content on top of real newer work.
    #[error(
        "cannot undo: branch {branch:?} no longer matches the state it was discarded in (moved, \
         deleted, or checked out elsewhere since)"
    )]
    DiscardBranchMovedOrReoccupied { branch: String },
}
