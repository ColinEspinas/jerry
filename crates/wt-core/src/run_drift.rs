//! How many commits have landed in a worktree since a moment in time - the count behind the app's
//! run-history drift bands.
//!
//! Measured from a timestamp rather than a recorded `HEAD` sha: records already written carry a
//! timestamp and no sha, and after a rebase `<sha>..HEAD` counts the whole branch. The trade-off is
//! that counting by committer date makes a rebase's rewritten commits look like they landed after
//! the run.
//!
//! Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::path::Path;

use crate::{check_success, run_git, Error};

/// For each entry of `since_unix`, how many commits reachable from `HEAD` were committed at or
/// after that moment, in input order.
///
/// One `git` invocation for all of them, bounded by the oldest moment asked about, so a worktree
/// with several runs does not cost one child process each. Timestamps are passed as git's
/// `@<seconds>` fixed-date form, so no timezone interpretation happens anywhere.
///
/// `Ok(None)`, never a vector of zeros, when `HEAD` is unborn. An empty `since_unix` spawns
/// nothing.
///
/// Performs blocking I/O.
pub fn commits_since_each(
    worktree_path: &Path,
    since_unix: &[i64],
) -> Result<Option<Vec<usize>>, Error> {
    if since_unix.is_empty() {
        return Ok(Some(Vec::new()));
    }
    // git would accept `@-1` and traverse all of history for it, so a non-positive entry answers
    // `0` rather than widening everyone else's traversal.
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
        // An unborn or unresolvable `HEAD` is an expected state for a new checkout, not an error.
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

/// Drift for a single moment. `Ok(None)` for an unborn `HEAD` or a non-positive `since_unix`.
pub fn commits_since(worktree_path: &Path, since_unix: i64) -> Result<Option<usize>, Error> {
    if since_unix <= 0 {
        return Ok(None);
    }
    Ok(
        commits_since_each(worktree_path, &[since_unix])?
            .and_then(|counts| counts.first().copied()),
    )
}

/// Parses one committer date per line. An unparseable line is skipped, not counted as zero -
/// which would make it land "since" every run.
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

    /// `at` pins both author and committer date, so commits land on a chosen side of a mark
    /// deterministically rather than racing the wall clock.
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

        // The run ended here; everything above predates it.
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
