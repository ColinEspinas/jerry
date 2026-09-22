//! `RebaseStart`/`RebaseContinue`/`RebaseSkip`/`RebaseAbort`/`AmendHeadMessage` end to end,
//! against real temporary repositories - the Command layer's own `validate`/`execute` wiring on
//! top of `jerry_git::rebase`, already covered for real by `jerry-git`'s own
//! `tests/rebase_editor.rs`.
//!
//! An integration test, not `src/commands.rs`'s own `#[cfg(test)]` module, so `RebaseStart::
//! execute`'s `jerry_binary::locate` call finds a real, executable file - see this crate's
//! `Cargo.toml` and `docs/architecture/decisions.md` §20 for why. `ensure_jerry_binary()` copies
//! this crate's own `jerry_core_test_jerry` `[[bin]]` next to this test binary's own executable.

// An integration test file is its own crate root - `src/lib.rs`'s crate-level
// `cfg_attr(test, allow(...))` does not reach here, so it is repeated (CLAUDE.md's
// `unwrap`/`expect` rule is scoped to non-test code, and this whole file is test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use jerry_core::command::{run_command, run_query, validate_command};
use jerry_core::ctx::{Caller, Ctx};
use jerry_core::queries::{RebaseStatusOutcome, RebaseStatusQuery};
use jerry_core::report::Report;
use jerry_core::{
    AmendHeadMessage, RebaseAbort, RebaseActionWire, RebaseContinue, RebaseOutcomeReport,
    RebasePlanEntryWire, RebaseSkip, RebaseStart, StopReasonWire,
};
use std::path::{Path, PathBuf};
use std::sync::Once;
use test_support::{commit, git, git_output, seed_empty_repo, TempDir};

fn ensure_jerry_binary() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let real = env!("CARGO_BIN_EXE_jerry_core_test_jerry");
        let current_exe = std::env::current_exe().expect("current_exe");
        let dir = current_exe.parent().expect("current_exe has a parent");
        let name = if cfg!(windows) { "jerry.exe" } else { "jerry" };
        std::fs::copy(real, dir.join(name)).expect("copy the test jerry binary into place");
    });
}

fn ctx_for(worktree: &Path) -> Ctx {
    Ctx::from_cwd(worktree, Caller::Human).expect("ctx")
}

fn head(dir: &Path) -> String {
    git_output(dir, &["rev-parse", "HEAD"])
}

fn outcome(report: Report) -> RebaseOutcomeReport {
    match report {
        Report::Ok { outcome } => serde_json::from_value(outcome).expect("outcome"),
        other => panic!("expected ok, got {other:?}"),
    }
}

fn status(report: Report) -> Option<RebaseStatusOutcome> {
    match report {
        Report::Ok { outcome } => serde_json::from_value(outcome).expect("status"),
        other => panic!("expected ok, got {other:?}"),
    }
}

/// `base` sets `file.txt`, `v1` and `v2` change it differently - replaying `v2` straight onto
/// `base` (skipping `v1`) is a real, three-stage conflict, all within one worktree: a rebase
/// never needs a second one the way a merge does.
fn conflicting_repo() -> (TempDir, String, String) {
    let repo = seed_empty_repo();
    commit(repo.path(), "file.txt", "base\n", "base");
    let base = head(repo.path());
    commit(repo.path(), "file.txt", "v1\n", "commit v1");
    commit(repo.path(), "file.txt", "v2\n", "commit v2");
    let v2 = head(repo.path());
    (repo, base, v2)
}

#[test]
fn a_clean_plan_completes_and_the_status_query_shows_nothing_in_progress() {
    ensure_jerry_binary();
    let repo = seed_empty_repo();
    commit(repo.path(), "base.txt", "base\n", "base");
    let base = head(repo.path());
    commit(repo.path(), "a.txt", "1\n", "commit 1");
    let c1 = head(repo.path());
    let ctx = ctx_for(repo.path());

    let start = RebaseStart {
        onto: base,
        plan: vec![RebasePlanEntryWire {
            commit: c1,
            action: RebaseActionWire::Pick,
        }],
    };
    assert_eq!(
        validate_command(&start, &ctx),
        Report::Ok {
            outcome: serde_json::Value::Null
        }
    );
    assert_eq!(
        outcome(run_command(start, &ctx)),
        RebaseOutcomeReport::Completed
    );
    assert_eq!(status(run_query(&RebaseStatusQuery::default(), &ctx)), None);
}

#[test]
fn a_real_conflict_stops_the_rebase_and_a_second_start_is_denied_while_it_is_in_progress() {
    ensure_jerry_binary();
    let (repo, base, v2) = conflicting_repo();
    let ctx = ctx_for(repo.path());

    let start = RebaseStart {
        onto: base,
        plan: vec![RebasePlanEntryWire {
            commit: v2.clone(),
            action: RebaseActionWire::Pick,
        }],
    };
    match outcome(run_command(start.clone(), &ctx)) {
        RebaseOutcomeReport::StoppedForConflict {
            commit,
            conflicted_files,
        } => {
            assert_eq!(commit, v2);
            assert_eq!(conflicted_files, vec![PathBuf::from("file.txt")]);
        }
        other => panic!("expected StoppedForConflict, got {other:?}"),
    }

    let second_start = validate_command(&start, &ctx);
    assert!(
        matches!(second_start, Report::Denied { ref code, .. } if code == "rebase-already-in-progress"),
        "got {second_start:?}"
    );

    std::fs::write(repo.path().join("file.txt"), "resolved\n").expect("resolve");
    git(repo.path(), &["add", "file.txt"]);
    assert_eq!(
        outcome(run_command(RebaseContinue::default(), &ctx)),
        RebaseOutcomeReport::Completed
    );
}

#[test]
fn continue_skip_and_abort_are_all_denied_with_no_rebase_in_progress() {
    let repo = seed_empty_repo();
    commit(repo.path(), "a.txt", "1\n", "commit 1");
    let ctx = ctx_for(repo.path());
    for report in [
        validate_command(&RebaseContinue::default(), &ctx),
        validate_command(&RebaseSkip::default(), &ctx),
        validate_command(&RebaseAbort::default(), &ctx),
    ] {
        assert!(
            matches!(report, Report::Denied { ref code, .. } if code == "rebase-not-in-progress"),
            "{report:?}"
        );
    }
}

#[test]
fn skip_genuinely_skips_the_stopped_commit_and_continues() {
    ensure_jerry_binary();
    let (repo, base, v2) = conflicting_repo();
    commit(repo.path(), "other.txt", "3\n", "commit 3");
    let c3 = head(repo.path());
    let ctx = ctx_for(repo.path());

    let start = RebaseStart {
        onto: base,
        plan: vec![
            RebasePlanEntryWire {
                commit: v2,
                action: RebaseActionWire::Pick,
            },
            RebasePlanEntryWire {
                commit: c3,
                action: RebaseActionWire::Pick,
            },
        ],
    };
    let started = outcome(run_command(start, &ctx));
    assert!(
        matches!(started, RebaseOutcomeReport::StoppedForConflict { .. }),
        "got {started:?}"
    );

    assert_eq!(
        outcome(run_command(RebaseSkip::default(), &ctx)),
        RebaseOutcomeReport::Completed
    );
    assert_eq!(
        git_output(repo.path(), &["log", "-1", "--format=%s"]),
        "commit 3"
    );
}

#[test]
fn abort_restores_head_to_exactly_its_pre_rebase_state() {
    ensure_jerry_binary();
    let (repo, base, v2) = conflicting_repo();
    let before_head = head(repo.path());
    let ctx = ctx_for(repo.path());

    let start = RebaseStart {
        onto: base,
        plan: vec![RebasePlanEntryWire {
            commit: v2,
            action: RebaseActionWire::Pick,
        }],
    };
    let started = outcome(run_command(start, &ctx));
    assert!(
        matches!(started, RebaseOutcomeReport::StoppedForConflict { .. }),
        "got {started:?}"
    );

    assert!(run_command(RebaseAbort::default(), &ctx).is_ok());
    assert_eq!(head(repo.path()), before_head);
}

#[test]
fn amend_head_message_applies_after_a_message_less_reword_stop_then_continue_completes() {
    ensure_jerry_binary();
    let repo = seed_empty_repo();
    commit(repo.path(), "base.txt", "base\n", "base");
    let base = head(repo.path());
    commit(repo.path(), "a.txt", "1\n", "original message");
    let c1 = head(repo.path());
    let ctx = ctx_for(repo.path());

    let start = RebaseStart {
        onto: base,
        plan: vec![RebasePlanEntryWire {
            commit: c1.clone(),
            action: RebaseActionWire::Reword { message: None },
        }],
    };
    match outcome(run_command(start, &ctx)) {
        RebaseOutcomeReport::StoppedForEdit { commit, reason } => {
            assert_eq!(commit, c1);
            assert_eq!(reason, Some(StopReasonWire::RewordNeedsMessage));
        }
        other => panic!("expected StoppedForEdit, got {other:?}"),
    }

    let amend = AmendHeadMessage {
        message: "a real amended message".into(),
    };
    assert_eq!(
        validate_command(&amend, &ctx),
        Report::Ok {
            outcome: serde_json::Value::Null
        }
    );
    assert!(run_command(amend, &ctx).is_ok());

    assert_eq!(
        outcome(run_command(RebaseContinue::default(), &ctx)),
        RebaseOutcomeReport::Completed
    );
    assert_eq!(
        git_output(repo.path(), &["log", "-1", "--format=%B"]).trim(),
        "a real amended message"
    );
}

#[test]
fn amend_head_message_is_denied_with_no_rebase_stopped() {
    let repo = seed_empty_repo();
    commit(repo.path(), "a.txt", "1\n", "commit 1");
    let ctx = ctx_for(repo.path());
    let amend = AmendHeadMessage {
        message: "x".into(),
    };
    let denied = validate_command(&amend, &ctx);
    assert!(
        matches!(denied, Report::Denied { ref code, .. } if code == "rebase-not-stopped"),
        "got {denied:?}"
    );
}

#[test]
fn the_status_query_reflects_a_real_stop_and_clears_once_completed() {
    ensure_jerry_binary();
    let repo = seed_empty_repo();
    commit(repo.path(), "base.txt", "base\n", "base");
    let base = head(repo.path());
    commit(repo.path(), "a.txt", "1\n", "commit 1");
    let c1 = head(repo.path());
    let ctx = ctx_for(repo.path());

    let start = RebaseStart {
        onto: base,
        plan: vec![RebasePlanEntryWire {
            commit: c1.clone(),
            action: RebaseActionWire::Edit,
        }],
    };
    let started = outcome(run_command(start, &ctx));
    assert!(
        matches!(started, RebaseOutcomeReport::StoppedForEdit { .. }),
        "got {started:?}"
    );

    let mid_flight = status(run_query(&RebaseStatusQuery::default(), &ctx))
        .expect("a rebase is really in progress");
    assert_eq!(mid_flight.stopped_commit.as_deref(), Some(c1.as_str()));
    assert_eq!(mid_flight.stop_reason, Some(StopReasonWire::Edit));

    assert_eq!(
        outcome(run_command(RebaseContinue::default(), &ctx)),
        RebaseOutcomeReport::Completed
    );
    assert_eq!(status(run_query(&RebaseStatusQuery::default(), &ctx)), None);
}
