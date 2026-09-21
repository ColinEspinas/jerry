//! The one error shape a `Report` carries: a stable machine `code`, a human `message`, and
//! optional structured `data`. Callers branch on the code; the message may be reworded freely.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    /// Stable, kebab-case, part of the wire contract.
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// A failure with no more specific code: a serialization that cannot fail did, and so on.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<jerry_git::Error> for Error {
    fn from(error: jerry_git::Error) -> Self {
        Error {
            code: git_error_code(&error).to_owned(),
            message: error.to_string(),
            data: git_error_data(&error),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::internal(format!("serialization failed: {error}"))
    }
}

/// The stable code for every `jerry-git` failure. Exhaustive on purpose: a new variant must be
/// classified here before it compiles, so no failure ever reaches the wire without a code.
pub fn git_error_code(error: &jerry_git::Error) -> &'static str {
    use jerry_git::Error as E;
    match error {
        E::Open { .. } => "repository-open-failed",
        E::WorktreeIo(_) => "worktree-io",
        E::Head(_)
        | E::PeelHead(_)
        | E::PeelReference(_)
        | E::MergeBase(_)
        | E::RevWalk(_)
        | E::RevWalkIter(_)
        | E::RevWalkObject(_)
        | E::RevWalkDecode(_)
        | E::RevWalkCommit(_)
        | E::References(_)
        | E::ReferencesIter(_)
        | E::ReferenceEntry(_) => "git-read-failed",
        E::GitSpawn { .. } => "git-spawn-failed",
        E::GitCommand { .. } => "git-command-failed",
        E::DirtyWorktree { .. } => "dirty-worktree",
        E::MergeNoBaseBranch => "merge-no-base-branch",
        E::MergeSourceIsBaseBranch { .. } => "merge-source-is-base-branch",
        E::MergeSourceDetached { .. } => "merge-source-detached",
        E::MergeTargetDetached { .. } => "merge-target-detached",
        E::MergeSourceIsCurrentBranch { .. } => "merge-source-is-current-branch",
        E::MergeBaseBranchNotCheckedOut { .. } => "merge-base-branch-not-checked-out",
        E::MergeTargetDirty { .. } => "merge-target-dirty",
        E::MergeConflictFileNotUtf8 { .. } => "merge-conflict-file-not-utf8",
        E::MergeMalformedConflictMarkers { .. } => "merge-malformed-conflict-markers",
        E::MergeUnsupportedConflictStyle { .. } => "merge-unsupported-conflict-style",
        E::MergeNoSuchHunk { .. } => "merge-no-such-hunk",
        E::MergeFileNotFullyResolved { .. } => "merge-file-not-fully-resolved",
        E::MergeNotInProgress { .. } => "merge-not-in-progress",
        E::MergeFilesStillConflicted { .. } => "merge-files-still-conflicted",
        E::NothingToCommit { .. } => "nothing-to-commit",
        E::HeadMovedSinceRecorded { .. } => "head-moved-since-recorded",
        E::RebaseAmendHeadMoved { .. } => "rebase-amend-head-moved",
        E::RebaseAmendIndexDirty { .. } => "rebase-amend-index-dirty",
        E::CommitHasNoParentAndNoBranch { .. } => "commit-has-no-parent-and-no-branch",
        E::DiscardSourceUnborn { .. } => "discard-source-unborn",
        E::DiscardSourceIsMainWorktree { .. } => "discard-source-is-main-worktree",
        E::DiscardSnapshotFailed { .. } => "discard-snapshot-failed",
        E::DiscardRemovalFailedAfterStash { .. } => "discard-removal-failed-after-stash",
        E::DiscardWorktreePathReoccupied { .. } => "discard-worktree-path-reoccupied",
        E::DiscardBranchMovedOrReoccupied { .. } => "discard-branch-moved-or-reoccupied",
    }
}

/// The actionable part of a failure, when there is one: the paths to open, the branch named.
fn git_error_data(error: &jerry_git::Error) -> Option<Value> {
    use jerry_git::Error as E;
    match error {
        E::MergeFilesStillConflicted { paths } => Some(serde_json::json!({ "paths": paths })),
        E::DirtyWorktree { path }
        | E::MergeSourceDetached { path }
        | E::MergeTargetDetached { path }
        | E::MergeTargetDirty { path }
        | E::MergeConflictFileNotUtf8 { path }
        | E::MergeMalformedConflictMarkers { path }
        | E::MergeUnsupportedConflictStyle { path }
        | E::MergeFileNotFullyResolved { path }
        | E::MergeNotInProgress { path }
        | E::NothingToCommit { path } => Some(serde_json::json!({ "path": path })),
        E::MergeNoSuchHunk { path, index } => {
            Some(serde_json::json!({ "path": path, "index": index }))
        }
        E::MergeSourceIsBaseBranch { branch }
        | E::MergeSourceIsCurrentBranch { branch }
        | E::MergeBaseBranchNotCheckedOut { branch } => {
            Some(serde_json::json!({ "branch": branch }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod error_code_tests {
    use super::Error;
    use std::path::PathBuf;

    #[test]
    fn a_git_failure_becomes_a_stable_code_with_its_actionable_data() {
        let paths = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")];
        let error: Error = jerry_git::Error::MergeFilesStillConflicted { paths }.into();
        assert_eq!(error.code, "merge-files-still-conflicted");
        assert_eq!(
            error.data,
            Some(serde_json::json!({ "paths": ["src/a.rs", "src/b.rs"] }))
        );
        assert!(error.message.contains("2 file(s)"));
    }

    #[test]
    fn data_is_omitted_from_the_wire_when_absent() {
        let error: Error = jerry_git::Error::MergeNoBaseBranch.into();
        let json = serde_json::to_value(&error).expect("serializable");
        assert_eq!(
            json,
            serde_json::json!({
                "code": "merge-no-base-branch",
                "message": "no default/base branch could be detected for this repository",
            })
        );
    }
}
