//! How far a worktree's branch has moved on since a moment in time - the real git side of
//! GitHub issue #227's **drift** axis.
//!
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-13.md` §4 defines drift as a count of
//! "commits since" a finished agent run ended, banded into `at the tip` / `1-2 commits since` /
//! `3+ commits since`. The band and its wording are the app's ([`app::run_history::model`]); the
//! *number* is this module, and it is a real `git rev-list` in the run's own checkout, never an
//! estimate.
//!
//! ## Why a timestamp and not a recorded commit id
//!
//! A run record could have stored the `HEAD` sha at the moment its agent closed, and drift could
//! then be `git rev-list --count <sha>..HEAD`. That was considered and deliberately not done, for
//! two reasons worth writing down because both are judgement calls rather than facts:
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

/// How many commits reachable from `HEAD` in the worktree at `worktree_path` were committed at or
/// after `since_unix` (seconds since the Unix epoch).
///
/// `Ok(None)` - never a fabricated `0` - for the one real case where the question has no answer:
/// an unborn `HEAD` (a freshly `git init`ed checkout with no commit at all), which `git rev-list`
/// reports as a failure rather than as an empty history. Every other failure is a real [`Error`]
/// the caller can surface or log; nothing here silently degrades into a number.
///
/// `since_unix` is passed as git's own `@<seconds>` "fixed timestamp" date format rather than a
/// formatted local date string, so no timezone interpretation happens anywhere between the record
/// and the traversal.
///
/// Performs blocking I/O: spawns a real `git` child process. Callers on a UI thread must hand it
/// to a background executor (see `app::run_history::flow`).
pub fn commits_since(worktree_path: &Path, since_unix: i64) -> Result<Option<usize>, Error> {
    // A negative (or zero) timestamp is not a real run-end moment - `@-1` is also a date git
    // would happily accept and traverse the entire history for. Refuse to guess.
    if since_unix <= 0 {
        return Ok(None);
    }
    let args: Vec<OsString> = vec![
        "rev-list".into(),
        "--count".into(),
        format!("--since=@{since_unix}").into(),
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
        {
            return Ok(None);
        }
        check_success(&args, &output)?;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_commit_count(&text))
}

/// Parses `git rev-list --count`'s stdout into a real count. `None` - not a fabricated `0` - for
/// output this doesn't recognise, so a future git whose output shape changed can never be
/// rendered as "at the tip".
fn parse_commit_count(text: &str) -> Option<usize> {
    text.trim().parse::<usize>().ok()
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
    fn unrecognised_rev_list_output_parses_to_none_not_a_fabricated_zero() {
        assert_eq!(parse_commit_count("3\n"), Some(3));
        assert_eq!(parse_commit_count("  12  "), Some(12));
        assert_eq!(parse_commit_count(""), None);
        assert_eq!(parse_commit_count("not a number"), None);
    }
}
