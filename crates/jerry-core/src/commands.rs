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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAttempt {}

/// Merge `source_branch` into whatever the caller's worktree has checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeBranchIntoCurrent {
    pub source_branch: String,
}

/// Commit the merge in progress at `base_worktree_path`, once nothing is unmerged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeComplete {
    pub base_worktree_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAbort {
    pub base_worktree_path: PathBuf,
}

/// Stage one file whose conflict markers are all gone. The write itself is a plain disk edit;
/// only staging touches the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
