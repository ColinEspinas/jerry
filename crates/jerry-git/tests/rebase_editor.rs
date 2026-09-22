//! Every `jerry_git::rebase` test that drives a real `git rebase -i` end to end, needing a
//! genuinely executable `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR` - this crate's own `[[bin]]`,
//! `jerry_git_test_editor`, a minimal real stand-in for `jerry git-sequence-editor`/`jerry
//! git-editor` (`crates/jerry-git/src/bin/test_git_editor.rs`).
//!
//! Lives here, not in `src/rebase.rs`'s own `#[cfg(test)]` module, because `CARGO_BIN_EXE_<name>`
//! is only reliable for an integration test target - a `--lib` unit test in this same crate,
//! referencing this exact same-package binary, could not see the environment variable at all
//! (verified against this crate's own build before writing this file).

// An integration test file is its own crate root - `src/lib.rs`'s crate-level
// `cfg_attr(test, allow(...))` does not reach here, so it is repeated (CLAUDE.md's
// `unwrap`/`expect` rule is scoped to non-test code, and this whole file is test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use jerry_git::rebase::{
    abort_rebase, continue_rebase, rebase_preflight, rebase_status, skip_rebase_commit,
    start_interactive_rebase, RebaseAction, RebaseOutcome, RebasePlanEntry, StopReason,
};
use jerry_git::Error;
use std::path::{Path, PathBuf};
use test_support::{git, git_output, seed_empty_repo, TempDir};

fn jerry_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jerry_git_test_editor"))
}

fn commit(dir: &Path, file: &str, contents: &str, message: &str) -> String {
    std::fs::write(dir.join(file), contents).expect("write file");
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", message]);
    git_output(dir, &["rev-parse", "HEAD"])
}

fn log_subjects(dir: &Path) -> Vec<String> {
    git_output(dir, &["log", "--format=%s", "--reverse"])
        .lines()
        .map(str::to_string)
        .collect()
}

fn commit_message(dir: &Path, commit: &str) -> String {
    git_output(dir, &["log", "-1", "--format=%B", commit])
}

/// `base` sets `file.txt`, `v1` and `v2` change it differently, and the plan replays `v2`
/// straight onto `base` - a real, three-stage conflict.
fn conflicting_repo() -> (TempDir, String, String) {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "file.txt", "base", "base");
    commit(repo.path(), "file.txt", "v1", "commit v1");
    let v2 = commit(repo.path(), "file.txt", "v2", "commit v2");
    (repo, base, v2)
}

// --- start_interactive_rebase: no-stop plans --------------------------------------------

#[test]
fn all_pick_plan_completes_cleanly_with_history_matching_the_plan() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
    let c2 = commit(repo.path(), "b.txt", "2", "commit 2");
    let c3 = commit(repo.path(), "c.txt", "3", "commit 3");

    let plan = vec![
        RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c3,
            action: RebaseAction::Pick,
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(
        log_subjects(repo.path()),
        vec!["base", "commit 1", "commit 2", "commit 3"]
    );
}

#[test]
fn drop_removes_the_commit_from_history() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
    let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

    let plan = vec![
        RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Drop,
        },
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Pick,
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(log_subjects(repo.path()), vec!["base", "commit 2"]);
    assert!(!repo.path().join("a.txt").exists());
}

#[test]
fn squash_folds_into_the_previous_commit_keeping_both_original_messages() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
    let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

    let plan = vec![
        RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Squash,
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(log_subjects(repo.path()), vec!["base", "commit 1"]);
    let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
    let message = commit_message(repo.path(), &head);
    assert!(
        message.contains("commit 1") && message.contains("commit 2"),
        "the combined message must contain both original messages unmodified, got: {message:?}"
    );
    assert!(repo.path().join("a.txt").exists());
    assert!(repo.path().join("b.txt").exists());
}

#[test]
fn fixup_folds_into_the_previous_commit_discarding_its_own_message() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
    let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

    let plan = vec![
        RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Fixup,
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(log_subjects(repo.path()), vec!["base", "commit 1"]);
    let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
    let message = commit_message(repo.path(), &head);
    assert!(message.contains("commit 1"));
    assert!(
        !message.contains("commit 2"),
        "fixup must discard the folded commit's own message, got: {message:?}"
    );
}

#[test]
fn reword_with_a_supplied_message_runs_straight_through_with_no_stop() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "original message");

    let plan = vec![RebasePlanEntry {
        commit: c1,
        action: RebaseAction::Reword(Some("new message".to_string())),
    }];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
    assert_eq!(commit_message(repo.path(), &head), "new message");
}

// --- Deliberate stops: reword-without-message and edit -----------------------------------

#[test]
fn reword_with_no_message_stops_and_reports_the_right_commit_and_reason() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");

    let plan = vec![RebasePlanEntry {
        commit: c1.clone(),
        action: RebaseAction::Reword(None),
    }];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    match outcome {
        RebaseOutcome::StoppedForEdit { commit, reason } => {
            assert_eq!(commit, c1);
            assert_eq!(reason, Some(StopReason::RewordNeedsMessage));
        }
        other => panic!("expected StoppedForEdit, got {other:?}"),
    }

    git(repo.path(), &["commit", "--amend", "-m", "amended message"]);
    let outcome = continue_rebase(repo.path()).expect("continue_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
    assert_eq!(commit_message(repo.path(), &head), "amended message");
}

#[test]
fn edit_always_stops_even_with_no_special_handling_and_continue_completes_it() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");

    let plan = vec![RebasePlanEntry {
        commit: c1.clone(),
        action: RebaseAction::Edit,
    }];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    match outcome {
        RebaseOutcome::StoppedForEdit { commit, reason } => {
            assert_eq!(commit, c1);
            assert_eq!(reason, Some(StopReason::Edit));
        }
        other => panic!("expected StoppedForEdit, got {other:?}"),
    }

    let outcome = continue_rebase(repo.path()).expect("continue_rebase with no changes");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(log_subjects(repo.path()), vec!["base", "commit 1"]);
}

// --- Real conflicts, abort, skip -----------------------------------------------------

#[test]
fn a_real_conflict_is_reported_with_the_right_file_and_abort_restores_original_state() {
    let (repo, base, v2) = conflicting_repo();
    let before_head = git_output(repo.path(), &["rev-parse", "HEAD"]);

    let plan = vec![RebasePlanEntry {
        commit: v2.clone(),
        action: RebaseAction::Pick,
    }];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    match outcome {
        RebaseOutcome::StoppedForConflict {
            commit,
            conflicted_files,
        } => {
            assert_eq!(commit, v2);
            assert_eq!(conflicted_files, vec![PathBuf::from("file.txt")]);
        }
        other => panic!("expected StoppedForConflict, got {other:?}"),
    }

    abort_rebase(repo.path()).expect("abort_rebase");
    let after_head = git_output(repo.path(), &["rev-parse", "HEAD"]);
    assert_eq!(
        before_head, after_head,
        "abort must restore HEAD to exactly its pre-rebase state"
    );
    assert!(!repo.path().join(".git").join("rebase-merge").exists());
}

#[test]
fn resolving_a_real_conflict_for_real_and_continuing_completes_the_rebase() {
    let (repo, base, v2) = conflicting_repo();

    let plan = vec![RebasePlanEntry {
        commit: v2,
        action: RebaseAction::Pick,
    }];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

    std::fs::write(repo.path().join("file.txt"), "resolved").expect("write resolution");
    git(repo.path(), &["add", "file.txt"]);
    let outcome = continue_rebase(repo.path()).expect("continue_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("file.txt")).expect("read file.txt"),
        "resolved"
    );
}

#[test]
fn skip_rebase_commit_genuinely_skips_the_stopped_commit_and_continues() {
    let (repo, base, v2) = conflicting_repo();
    let c3 = commit(repo.path(), "other.txt", "3", "commit 3");

    let plan = vec![
        RebasePlanEntry {
            commit: v2,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c3,
            action: RebaseAction::Pick,
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

    let outcome = skip_rebase_commit(repo.path()).expect("skip_rebase_commit");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(log_subjects(repo.path()), vec!["base", "commit 3"]);
}

// --- The mixed-action mega-scenario: catches message-editor disambiguation bugs ---------

#[test]
fn a_plan_mixing_every_action_type_produces_the_exact_expected_history() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "f1.txt", "1", "commit 1");
    let c2 = commit(repo.path(), "f2.txt", "2", "commit 2");
    let c3 = commit(repo.path(), "f3.txt", "3", "commit 3");
    let c4 = commit(repo.path(), "f4.txt", "4", "commit 4");
    let c5 = commit(repo.path(), "f5.txt", "5", "commit 5");
    let c6 = commit(repo.path(), "f6.txt", "6", "commit 6");

    let plan = vec![
        RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Squash,
        },
        RebasePlanEntry {
            commit: c3,
            action: RebaseAction::Reword(Some("reworded commit 3".to_string())),
        },
        RebasePlanEntry {
            commit: c4.clone(),
            action: RebaseAction::Reword(None),
        },
        RebasePlanEntry {
            commit: c5,
            action: RebaseAction::Drop,
        },
        RebasePlanEntry {
            commit: c6,
            action: RebaseAction::Pick,
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    match &outcome {
        RebaseOutcome::StoppedForEdit { commit, reason } => {
            assert_eq!(commit, &c4);
            assert_eq!(*reason, Some(StopReason::RewordNeedsMessage));
        }
        other => panic!("expected StoppedForEdit at commit 4, got {other:?}"),
    }

    git(
        repo.path(),
        &["commit", "--amend", "-m", "amended commit 4"],
    );
    let outcome = continue_rebase(repo.path()).expect("continue_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);

    assert_eq!(
        log_subjects(repo.path()),
        vec![
            "base",
            "commit 1",
            "reworded commit 3",
            "amended commit 4",
            "commit 6",
        ]
    );
    let all_shas = git_output(repo.path(), &["log", "--format=%H", "--reverse"]);
    let second_sha = all_shas.lines().nth(1).expect("second commit");
    let squashed_message = commit_message(repo.path(), second_sha);
    assert!(squashed_message.contains("commit 1"));
    assert!(squashed_message.contains("commit 2"));
}

#[test]
fn cascading_conflicts_before_a_reword_do_not_misalign_the_message_queue() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "file.txt", "base", "base");
    let c1 = commit(repo.path(), "file.txt", "v1", "commit v1");
    let c2 = commit(repo.path(), "file.txt", "v2", "commit v2");
    let c3 = commit(repo.path(), "other.txt", "3", "commit 3");

    // Reordering v2 before v1 (both touching the same file) forces two cascading conflicts
    // before the reword step is ever reached.
    let plan = vec![
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Pick,
        },
        RebasePlanEntry {
            commit: c3,
            action: RebaseAction::Reword(Some("reworded commit 3".to_string())),
        },
    ];

    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));
    std::fs::write(repo.path().join("file.txt"), "resolved-1").expect("write");
    git(repo.path(), &["add", "file.txt"]);
    let outcome = continue_rebase(repo.path()).expect("continue after first conflict");
    assert!(
        matches!(outcome, RebaseOutcome::StoppedForConflict { .. }),
        "expected a second real conflict, got {outcome:?}"
    );

    std::fs::write(repo.path().join("file.txt"), "resolved-2").expect("write");
    git(repo.path(), &["add", "file.txt"]);
    let outcome = continue_rebase(repo.path()).expect("continue after second conflict");
    assert_eq!(
        outcome,
        RebaseOutcome::Completed,
        "the reword step must run straight through with its own real message, not stop"
    );

    assert_eq!(
        log_subjects(repo.path()),
        vec!["base", "commit v2", "commit v1", "reworded commit 3"]
    );
}

// --- rebase_status -------------------------------------------------------------------

#[test]
fn rebase_status_reports_real_state_when_stopped_mid_flight() {
    let repo = seed_empty_repo();
    let base = commit(repo.path(), "base.txt", "base", "base");
    let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
    let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

    let plan = vec![
        RebasePlanEntry {
            commit: c1.clone(),
            action: RebaseAction::Edit,
        },
        RebasePlanEntry {
            commit: c2,
            action: RebaseAction::Pick,
        },
    ];
    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert!(matches!(outcome, RebaseOutcome::StoppedForEdit { .. }));

    let status = rebase_status(repo.path())
        .expect("rebase_status")
        .expect("a rebase is really in progress");
    assert_eq!(status.stopped_commit.as_deref(), Some(c1.as_str()));
    assert_eq!(status.stop_reason, Some(StopReason::Edit));
    assert!(status.conflicted_files.is_empty());
    assert_eq!(status.current_step, Some(1));
    assert_eq!(status.total_steps, Some(2));
    assert!(status.onto.is_some());

    // Recovery after a real "process restart" (nothing here relies on in-memory state):
    // continuing and finishing still works using only what's on disk.
    let outcome = continue_rebase(repo.path()).expect("continue_rebase");
    assert_eq!(outcome, RebaseOutcome::Completed);
    assert_eq!(rebase_status(repo.path()).expect("rebase_status"), None);
}

#[test]
fn rebase_status_reports_conflicted_files_and_no_stop_reason_for_a_real_conflict() {
    let (repo, base, v2) = conflicting_repo();
    let plan = vec![RebasePlanEntry {
        commit: v2,
        action: RebaseAction::Pick,
    }];
    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

    let status = rebase_status(repo.path())
        .expect("rebase_status")
        .expect("a rebase is really in progress");
    assert_eq!(status.conflicted_files, vec![PathBuf::from("file.txt")]);
    assert_eq!(
        status.stop_reason, None,
        "a real conflict must never report a deliberate-stop reason"
    );
}

// --- rebase_preflight -------------------------------------------------------------------

#[test]
fn rebase_preflight_refuses_while_a_rebase_is_already_in_progress() {
    let (repo, base, v2) = conflicting_repo();
    let plan = vec![RebasePlanEntry {
        commit: v2,
        action: RebaseAction::Pick,
    }];
    let outcome = start_interactive_rebase(repo.path(), &base, &plan, &jerry_binary())
        .expect("start_interactive_rebase");
    assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

    let err = rebase_preflight(repo.path()).expect_err("already in progress");
    assert!(
        matches!(err, Error::RebaseAlreadyInProgress { .. }),
        "{err:?}"
    );
}
