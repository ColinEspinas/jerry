//! How far a worktree's branch has moved on since a moment in time - the real git side of
//! GitHub issue #227's **drift** axis.
//!
//! The design defines drift as a count of
//! "commits since" a finished agent run ended, banded into `at the tip` / `1-2 commits since` /
//! `3+ commits since`. The band and its wording are the app's (`app::run_history::model`); the
//! *number* is this module, and it is a real `git log` traversal in the run's own checkout, never
//! an estimate.
//!
//! ## Why a timestamp and not a recorded commit id
//!
//! A run record could have stored the `HEAD` sha at the moment its agent closed, and drift could
//! then be `git rev-list --count <sha>..HEAD`. That was considered and deliberately not done, for
//! two reasons worth writing down because both are judgement calls rather than settled facts:
//!
//! 1. **It would only ever work for runs recorded after the field was added.** Every record Jerry
//!    has already written carries a real `updated_at_unix` and no sha, and a drift band that is
//!    blank for a user's whole existing history is worse than one derived from the timestamp they
//!    do have. There is no third "unknown" band in the design to render such a record in.
//! 2. **After a rebase the sha answer is arguably the worse one.** A rebased branch makes every
//!    pre-run commit unreachable from the recorded sha, so `<sha>..HEAD` counts the entire
//!    branch - a large, alarming number for a history that, from the user's point of view, gained
//!    nothing new.
//!
//! The timestamp has its own known limitation, stated here rather than hidden: it counts by
//! *committer date*, so a rebase (which rewrites committer dates) makes every rewritten commit
//! look like it landed after the run. That is a real property of the repository, not a
//! fabrication, and the sentence the app renders around it ("N commits have landed since") stays
//! true of it.

use std::ffi::OsString;
use std::path::Path;

use crate::{check_success, run_git, Error};

/// Drift for **every** run in one checkout, from one `git` invocation.
///
/// Returns, for each entry of `since_unix`, how many commits reachable from `HEAD` in the worktree
/// at `worktree_path` were committed at or after that moment - in the same order as the input.
///
/// One invocation rather than one per run, and that is the whole reason this is the primitive
/// [`commits_since`] delegates to: a worktree with three runs in its history would otherwise cost
/// three `git` child processes every time the History view refreshed. Reading each commit's own
/// committer date once and counting locally answers all of them, with exactly the same semantics
/// `git rev-list --count --since=@<t>` has (both filter on committer date), and with no cap on how
/// far back a run can be - the traversal is bounded by the *oldest* moment asked about.
///
/// `Ok(None)` - never a fabricated vector of zeros - for the one real case where the question has
/// no answer at all: an unborn `HEAD` (a freshly `git init`ed checkout with no commit in it),
/// which git reports as a failure rather than as an empty history. An empty `since_unix` gets an
/// empty vector and spawns nothing.
///
/// Each moment is passed to git as its own `@<seconds>` "fixed timestamp" date, so no timezone
/// interpretation happens anywhere between a run record and the traversal.
///
/// Performs blocking I/O: spawns a real `git` child process. Callers on a UI thread must hand it
/// to a background executor (see `app::run_history::flow`).
pub fn commits_since_each(
    worktree_path: &Path,
    since_unix: &[i64],
) -> Result<Option<Vec<usize>>, Error> {
    if since_unix.is_empty() {
        return Ok(Some(Vec::new()));
    }
    // A negative (or zero) timestamp is not a real run-end moment, and `@-1` is a date git would
    // happily accept and traverse the entire history for. Such an entry answers `0` rather than
    // widening everyone else's traversal.
    let Some(oldest) = since_unix.iter().copied().filter(|at| *at > 0).min() else {
        return Ok(Some(vec![0; since_unix.len()]));
    };

    let args: Vec<OsString> = vec![
        "log".into(),
        "--format=%ct".into(),
        format!("--since=@{oldest}").into(),
        "HEAD".into(),
    ];
    let output = run_git(worktree_path, &args)?;
    if !output.status.success() {
        // `HEAD` is unborn (or otherwise unresolvable): git says so on stderr and exits non-zero.
        // That is a real, expected state for a brand-new checkout, not an error worth surfacing.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision")
            || stderr.contains("ambiguous argument")
            || stderr.contains("bad revision")
            || stderr.contains("does not have any commits yet")
        {
            return Ok(None);
        }
        check_success(&args, &output)?;
    }
    let landed = parse_commit_dates(&String::from_utf8_lossy(&output.stdout));
    Ok(Some(
        since_unix
            .iter()
            .map(|at| {
                if *at <= 0 {
                    return 0;
                }
                landed.iter().filter(|committed| *committed >= at).count()
            })
            .collect(),
    ))
}

/// Drift for one run - a thin wrapper over [`commits_since_each`], so there is exactly one
/// traversal and one counting rule in this module rather than two that could disagree.
///
/// `Ok(None)` for an unborn `HEAD`, and for a `since_unix` that is not a real moment.
pub fn commits_since(worktree_path: &Path, since_unix: i64) -> Result<Option<usize>, Error> {
    if since_unix <= 0 {
        return Ok(None);
    }
    Ok(
        commits_since_each(worktree_path, &[since_unix])?
            .and_then(|counts| counts.first().copied()),
    )
}

/// Parses `git log --format=%ct`'s stdout - one committer date per line. A line that isn't a real
/// timestamp is skipped rather than counted as zero (which would make it land "since" every run).
fn parse_commit_dates(text: &str) -> Vec<i64> {
    text.lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn git(dir: &Path, args: &[&str]) {
        git_at(dir, args, None);
    }

    /// `at` pins the commit's real author *and* committer date, so a test can place commits on
    /// either side of a mark deterministically rather than racing the wall clock (the counting
    /// this module does is by committer date - see [`commits_since`]'s own docs).
    fn git_at(dir: &Path, args: &[&str], at: Option<i64>) {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        if let Some(at) = at {
            command
                .env("GIT_AUTHOR_DATE", format!("@{at} +0000"))
                .env("GIT_COMMITTER_DATE", format!("@{at} +0000"));
        }
        let status = command.status().expect("git must run");
        assert!(status.success(), "git {args:?} must succeed");
    }

    fn commit_at(dir: &Path, name: &str, at: i64) {
        std::fs::write(dir.join(name), name).expect("write");
        git_at(dir, &["add", "."], Some(at));
        git_at(dir, &["commit", "-m", name], Some(at));
    }

    fn commit(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), name).expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", name]);
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn a_real_repository_reports_the_real_number_of_commits_since_a_moment() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path();
        git(path, &["init", "-b", "main"]);
        commit_at(path, "before-a", 1_700_000_000);
        commit_at(path, "before-b", 1_700_000_100);

        // The run ended here. Everything above it predates the run.
        let mark = 1_700_000_500;
        assert_eq!(
            commits_since(path, mark).expect("must run"),
            Some(0),
            "a branch nobody has touched since the mark is at the tip"
        );

        commit_at(path, "after-a", 1_700_001_000);
        commit_at(path, "after-b", 1_700_001_100);
        commit_at(path, "after-c", 1_700_001_200);

        assert_eq!(
            commits_since(path, mark).expect("must run"),
            Some(3),
            "exactly the three commits made after the mark must be counted"
        );
    }

    #[test]
    fn an_unborn_head_has_no_answer_rather_than_a_fabricated_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        git(dir.path(), &["init", "-b", "main"]);
        assert_eq!(
            commits_since(dir.path(), now()).expect("must not error"),
            None,
            "a checkout with no commit at all cannot report a drift of zero"
        );
    }

    #[test]
    fn a_nonsense_timestamp_is_refused_rather_than_traversing_everything() {
        let dir = tempfile::tempdir().expect("temp dir");
        git(dir.path(), &["init", "-b", "main"]);
        commit(dir.path(), "one");
        assert_eq!(commits_since(dir.path(), 0).expect("must not error"), None);
        assert_eq!(commits_since(dir.path(), -5).expect("must not error"), None);
    }

    #[test]
    fn unparsable_commit_dates_are_skipped_rather_than_counted_as_the_epoch() {
        assert_eq!(
            parse_commit_dates("1700000000\n1700000100\n"),
            vec![1_700_000_000, 1_700_000_100]
        );
        assert_eq!(parse_commit_dates(""), Vec::<i64>::new());
        assert_eq!(
            parse_commit_dates("1700000000\nnot a number\n"),
            vec![1_700_000_000]
        );
    }

    /// The point of the batched form: several runs in one checkout, answered from one traversal,
    /// each with its own real count.
    #[test]
    fn every_run_in_one_checkout_is_answered_from_one_traversal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path();
        git(path, &["init", "-b", "main"]);
        commit_at(path, "a", 1_700_000_000);
        commit_at(path, "b", 1_700_001_000);
        commit_at(path, "c", 1_700_002_000);
        commit_at(path, "d", 1_700_003_000);

        let counts = commits_since_each(
            path,
            &[
                1_700_003_500, // after everything
                1_700_002_500, // one commit since
                1_700_000_500, // three commits since
                0,             // not a real moment
            ],
        )
        .expect("must run")
        .expect("a real history must answer");
        assert_eq!(counts, vec![0, 1, 3, 0]);
    }

    #[test]
    fn asking_about_no_runs_at_all_spawns_nothing_and_answers_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            commits_since_each(dir.path(), &[]).expect("must not error"),
            Some(Vec::new()),
            "a worktree with no runs must not cost a git process"
        );
    }
}
