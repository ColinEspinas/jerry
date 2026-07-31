//! Per-line `git blame` for a single file, plus lazy full commit-message lookup.
//!
//! ## Why the `git` CLI, not `gix`
//!
//! `gix` (as of the version this workspace pins, 0.68) has no public blame API - there is no
//! `gix::Repository::blame` or equivalent walking the same incremental-blame algorithm `git
//! blame` itself uses. Re-implementing that algorithm (copy detection across renames, line
//! attribution across merges, "boundary" commits in a shallow clone) on top of `gix`'s
//! lower-level object/diff primitives would mean re-deriving `git blame`'s own, quite
//! intricate, output from scratch. `git blame` already does this correctly, so - matching
//! `crate::diff`'s identical call on `git diff` (see that module's own "gix vs. the git CLI"
//! docs) - this module shells out to it instead, via a real argument vector
//! (`std::process::Command` with `&[OsString]`, never an interpolated shell string), per this
//! crate's own git-invocation convention.
//!
//! `--line-porcelain` (not plain `--porcelain`) is used deliberately: it repeats every
//! commit's full header (author, author-time, summary, ...) on *every* line's block instead of
//! only the first line that introduced a given commit. That makes this parser stateless across
//! lines - no "remember the last commit's fields for a line that only prints `<sha> <line>
//! <line>`" bookkeeping - at the cost of a larger (but still line-bounded, and this app already
//! caps opened files at [`crate::code_surface`]'s own 2MB read limit, mirrored here by nothing
//! extra being needed) amount of stdout to parse.
//!
//! ## Uncommitted lines
//!
//! A line whose content differs from `HEAD` (staged or not - `git blame` always blames the
//! working tree, not the index) is attributed to a synthetic all-zero sha
//! (`0000000000000000000000000000000000000000`) with author `"Not Committed Yet"`.
//! [`BlameLine::is_uncommitted`] is `true` exactly for these - callers should not print the
//! zero sha or treat it as a real commit.
//!
//! ## Graceful absence
//!
//! [`blame_file`] returns [`BlameOutcome::NotARepo`] or [`BlameOutcome::NotTracked`] - not an
//! [`Error`] - for the two "there is nothing to show, and that's not a failure" cases this
//! feature needs to distinguish from a real error: `worktree_path` isn't inside a git
//! repository at all, or `relative_path` has no history in `HEAD` (a genuinely untracked file,
//! or one that doesn't exist there). Both are real, expected outcomes a caller should render as
//! "no blame available", never as an error toast - see `crate::code_surface::blame`'s own docs
//! in the `app` crate for how the UI layer uses this distinction. A shallow clone is *not* a
//! third variant: `git blame` in a shallow clone still succeeds, attributing lines whose real
//! history is missing to the shallow boundary commit it does have (marked `boundary` in
//! porcelain output, which this parser reads the same as any other commit) - so shallow clones
//! need no special handling here at all, only real, always-on graceful degradation of what data
//! is available.
//!
//! Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{open_repo, run_git};

/// One line's real blame attribution, in file order (index 0 is line 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// The full 40-character commit sha, or the synthetic all-zero sha for an uncommitted
    /// line (see [`Self::is_uncommitted`]).
    pub sha: String,
    /// The commit author's real name, as `git blame` reports it (`"Not Committed Yet"` for an
    /// uncommitted line).
    pub author: String,
    /// The commit's author-time, as seconds since the Unix epoch (UTC).
    pub author_time_unix: i64,
    /// The commit's subject line (`git blame --line-porcelain`'s own `summary` field) - never
    /// the full, possibly-multi-paragraph commit message; see [`commit_message`] for that.
    pub summary: String,
    /// `true` for a line whose content differs from `HEAD` (an uncommitted local
    /// modification) - `sha` is the synthetic all-zero id and `author`/`summary` carry no real
    /// commit information in that case.
    pub is_uncommitted: bool,
}

/// A real, computed blame of one file, one entry per line in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBlame {
    pub lines: Vec<BlameLine>,
    /// The full commit id `HEAD` resolved to at the moment this blame was computed - part of
    /// this feature's cache key (see `crate::code_surface::blame`'s own docs in the `app`
    /// crate): a cached [`FileBlame`] is only trusted while both this and the file's own
    /// mtime/length still match.
    pub head_commit: String,
}

/// The outcome of trying to compute a file's blame - see the module docs' "Graceful absence"
/// section for why "nothing to show" is two real, non-error variants rather than folded into
/// [`Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameOutcome {
    Blame(FileBlame),
    /// `worktree_path` is not inside a git repository at all.
    NotARepo,
    /// `relative_path` has no history in `HEAD` - untracked, or it simply doesn't exist there
    /// (a brand new, never-committed file).
    NotTracked,
}

/// Computes the real blame of `relative_path` (resolved against `worktree_path`) via `git
/// blame --line-porcelain`. See the module docs for the CLI-vs-`gix` choice, the uncommitted-
/// line convention, and the two non-error "nothing to show" outcomes.
///
/// Performs blocking I/O: opens the repository via `gix` (just to distinguish "not a repo"
/// before ever spawning `git`) and spawns a real `git blame` child process.
pub fn blame_file(worktree_path: &Path, relative_path: &Path) -> Result<BlameOutcome, Error> {
    // `gix::open` is the same cheap "is this even a real repository" probe `crate::diff` and
    // `crate::list_worktrees` already use - real graceful-absence handling per the module docs,
    // not an error path a caller has to sift out of a generic `Err`. Its resolved `HEAD` commit
    // id also becomes this blame's real cache-key revision (see [`FileBlame::head_commit`]);
    // `None` (an unborn `HEAD` - a brand new repository with no commits yet) degrades to
    // [`BlameOutcome::NotTracked`] below without ever reaching `git blame` at all, since there
    // is provably no history yet for any path to have.
    let Ok(repo) = open_repo(worktree_path) else {
        return Ok(BlameOutcome::NotARepo);
    };
    let head_commit = repo
        .head()
        .ok()
        .and_then(|mut head| head.try_peel_to_id_in_place().ok().flatten())
        .map(|id| id.to_string());
    let Some(head_commit) = head_commit else {
        return Ok(BlameOutcome::NotTracked);
    };

    let args: Vec<OsString> = vec![
        "blame".into(),
        "--line-porcelain".into(),
        "--".into(),
        relative_path.as_os_str().to_owned(),
    ];
    let output = run_git(worktree_path, &args)?;
    if !output.status.success() {
        // `git blame` fails with a real, non-zero exit for a path with no history in `HEAD` -
        // both a genuinely untracked file and one that plain doesn't exist there report this
        // the same way (`fatal: no such path '<path>' in HEAD` / `fatal: <path>: no such
        // path in HEAD`); nothing else `blame_file` calls should ever fail this way for an
        // otherwise-valid path, so any non-zero exit here is treated as "nothing tracked",
        // never surfaced as an [`Error`] - matching this module's own documented "no error
        // toast" contract for untracked files.
        return Ok(BlameOutcome::NotTracked);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(BlameOutcome::Blame(FileBlame {
        lines: parse_line_porcelain(&text),
        head_commit,
    }))
}

/// Fetches the real, full commit message body (subject + blank line + body, exactly as `git
/// log`'s `%B` format placeholder produces it) for `sha` - the data
/// [`BlameLine::summary`]/[`FileBlame`] don't carry, needed for a real hover card showing "the
/// full commit message and SHA" (GitHub issue #29). Fetched lazily, one commit at a time, by
/// the `app` crate's own per-sha cache (`crate::code_surface::blame`'s docs) rather than eagerly
/// for every line `blame_file` returns - a file with a long history could otherwise mean one
/// `git log` call per distinct commit on every blame refresh for data most lines' hover never
/// actually needs.
///
/// `sha` is validated as a plausible hex object id before being handed to `git` as an argument -
/// the same defensive check `crate::diff::diff_against_base` applies to a merge-base sha before
/// its own `git diff` call, so a future change to how a caller derives `sha` can't silently
/// become a git-argument injection.
///
/// Returns `Ok(None)` for the synthetic all-zero "uncommitted" sha (never a real commit `git
/// log` could look up) and for a sha `git log` genuinely doesn't recognize, rather than an
/// `Error` - both are "nothing real to show", not a failure.
///
/// Performs blocking I/O: spawns a real `git log` child process.
pub fn commit_message(worktree_path: &Path, sha: &str) -> Result<Option<String>, Error> {
    if sha.is_empty()
        || !sha.bytes().all(|b| b.is_ascii_hexdigit())
        || sha.bytes().all(|b| b == b'0')
    {
        return Ok(None);
    }

    let args: Vec<OsString> = vec![
        "log".into(),
        "-1".into(),
        "--format=%B".into(),
        sha.to_string().into(),
        "--".into(),
    ];
    let output = run_git(worktree_path, &args)?;
    if !output.status.success() {
        return Ok(None);
    }
    let message = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string();
    if message.is_empty() {
        Ok(None)
    } else {
        Ok(Some(message))
    }
}

/// Parses `git blame --line-porcelain`'s stdout into a [`FileBlame`] - one [`BlameLine`] per
/// real content line, in file order. A commit header line looks like `<40-hex-sha> <orig-line>
/// <final-line>[ <num-lines-in-group>]`; every field until the next one of those (or a `\t`-
/// prefixed content line, which ends the current entry) is a `<key> <value>` header line for
/// the block just started. Because `--line-porcelain` repeats every header on every line
/// (unlike plain `--porcelain`), this never needs to remember a previous entry's fields across
/// iterations.
///
/// A parse failure on one malformed-looking block (missing `author`/`author-time`/`summary`) is
/// skipped rather than aborting the whole file - defensive against a future `git` version
/// changing field names slightly; better to lose one line's attribution than the whole blame.
fn parse_line_porcelain(text: &str) -> Vec<BlameLine> {
    let mut lines = Vec::new();

    let mut current_sha: Option<String> = None;
    let mut author: Option<String> = None;
    let mut author_time: Option<i64> = None;
    let mut summary: Option<String> = None;

    for raw_line in text.split('\n') {
        if raw_line.starts_with('\t') {
            // Content line: ends the current block. Emit an entry if the header was complete
            // enough to make sense of; either way, reset for the next block.
            if let (Some(sha), Some(author), Some(author_time)) =
                (current_sha.take(), author.take(), author_time.take())
            {
                let is_uncommitted = sha.bytes().all(|b| b == b'0');
                lines.push(BlameLine {
                    sha,
                    author,
                    author_time_unix: author_time,
                    summary: summary.take().unwrap_or_default(),
                    is_uncommitted,
                });
            } else {
                summary = None;
            }
            continue;
        }

        if let Some(header) = parse_commit_header(raw_line) {
            current_sha = Some(header);
            author = None;
            author_time = None;
            summary = None;
            continue;
        }

        let Some((key, value)) = raw_line.split_once(' ') else {
            continue;
        };
        match key {
            "author" => author = Some(value.to_string()),
            "author-time" => author_time = value.parse::<i64>().ok(),
            "summary" => summary = Some(value.to_string()),
            _ => {}
        }
    }

    lines
}

/// Recognizes a `git blame --line-porcelain` commit-header line (`<40-hex-sha> <orig-line>
/// <final-line>[ <num-lines-in-group>]`), returning the sha if `raw_line` matches. Distinguished
/// from an ordinary `<key> <value>` header field by requiring the first token to be exactly 40
/// hex characters (no real header key is hex-shaped and 40 characters long) followed by one or
/// two more purely-numeric tokens.
fn parse_commit_header(raw_line: &str) -> Option<String> {
    let mut parts = raw_line.split(' ');
    let sha = parts.next()?;
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let orig_line = parts.next()?;
    if !orig_line.bytes().all(|b| b.is_ascii_digit()) || orig_line.is_empty() {
        return None;
    }
    let final_line = parts.next()?;
    if !final_line.bytes().all(|b| b.is_ascii_digit()) || final_line.is_empty() {
        return None;
    }
    if let Some(extra) = parts.next() {
        if !extra.bytes().all(|b| b.is_ascii_digit()) || extra.is_empty() {
            return None;
        }
    }
    Some(sha.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        dir
    }

    #[test]
    fn not_a_repo_is_graceful() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write");
        let outcome = blame_file(dir.path(), Path::new("file.txt")).expect("blame_file");
        assert_eq!(outcome, BlameOutcome::NotARepo);
    }

    #[test]
    fn untracked_file_is_graceful() {
        let repo = init_repo();
        fs::write(repo.path().join("untracked.txt"), "hello\n").expect("write");
        let outcome = blame_file(repo.path(), Path::new("untracked.txt")).expect("blame_file");
        assert_eq!(outcome, BlameOutcome::NotTracked);
    }

    #[test]
    fn nonexistent_path_is_graceful() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        git(repo.path(), &["commit", "-m", "initial"]);
        let outcome = blame_file(repo.path(), Path::new("nope.txt")).expect("blame_file");
        assert_eq!(outcome, BlameOutcome::NotTracked);
    }

    #[test]
    fn committed_lines_are_attributed_to_the_real_commit() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "line one\nline two\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "add file.txt\n\nA real body paragraph."],
        );
        let real_sha = {
            let output = Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let outcome = blame_file(repo.path(), Path::new("file.txt")).expect("blame_file");
        let BlameOutcome::Blame(blame) = outcome else {
            panic!("expected BlameOutcome::Blame, got {outcome:?}");
        };
        assert_eq!(blame.lines.len(), 2);
        for line in &blame.lines {
            assert_eq!(line.sha, real_sha);
            assert_eq!(line.author, "Test User");
            assert_eq!(line.summary, "add file.txt");
            assert!(!line.is_uncommitted);
            assert!(line.author_time_unix > 0);
        }
    }

    #[test]
    fn uncommitted_local_modification_is_flagged() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "line one\nline two\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        git(repo.path(), &["commit", "-m", "initial"]);
        fs::write(repo.path().join("file.txt"), "line one\nCHANGED\n").expect("rewrite");

        let outcome = blame_file(repo.path(), Path::new("file.txt")).expect("blame_file");
        let BlameOutcome::Blame(blame) = outcome else {
            panic!("expected BlameOutcome::Blame, got {outcome:?}");
        };
        assert_eq!(blame.lines.len(), 2);
        assert!(!blame.lines[0].is_uncommitted, "line one is unchanged");
        assert!(
            blame.lines[1].is_uncommitted,
            "the modified line must be flagged uncommitted"
        );
        assert_eq!(blame.lines[1].sha, "0".repeat(40));
        assert_eq!(blame.lines[1].author, "Not Committed Yet");
    }

    #[test]
    fn commit_message_returns_the_real_full_body() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        git(
            repo.path(),
            &[
                "commit",
                "-m",
                "a real subject\n\nA real, multi-line\nbody paragraph.",
            ],
        );
        let real_sha = {
            let output = Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let message = commit_message(repo.path(), &real_sha)
            .expect("commit_message")
            .expect("a real commit must have a real message");
        assert!(message.starts_with("a real subject"));
        assert!(message.contains("A real, multi-line\nbody paragraph."));
    }

    #[test]
    fn commit_message_is_none_for_the_synthetic_uncommitted_sha() {
        let repo = init_repo();
        let message = commit_message(repo.path(), &"0".repeat(40)).expect("commit_message");
        assert_eq!(message, None);
    }

    #[test]
    fn commit_message_rejects_non_hex_input_without_spawning_a_bad_argument() {
        let repo = init_repo();
        let message =
            commit_message(repo.path(), "--not-a-sha").expect("commit_message should not error");
        assert_eq!(message, None);
    }

    #[test]
    fn blame_file_reports_the_real_current_head_commit() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        git(repo.path(), &["commit", "-m", "initial"]);
        let real_sha = {
            let output = Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let outcome = blame_file(repo.path(), Path::new("file.txt")).expect("blame_file");
        let BlameOutcome::Blame(blame) = outcome else {
            panic!("expected BlameOutcome::Blame, got {outcome:?}");
        };
        assert_eq!(blame.head_commit, real_sha);
    }
}
