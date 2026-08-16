//! Per-line `git blame` for a single file, plus lazy full commit-message lookup.
//!
//! Shells out to `git`, since `gix` has no blame API - see `docs/architecture/decisions.md` §5.
//! `--line-porcelain` rather than `--porcelain`, because repeating every commit's header on every
//! line keeps this parser stateless across lines.
//!
//! A line differing from `HEAD` is attributed to an all-zero sha with author
//! `"Not Committed Yet"`; [`BlameLine::is_uncommitted`] marks exactly those.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{open_repo, run_git};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// The 40-character commit sha, or the all-zero sha when [`Self::is_uncommitted`].
    pub sha: String,
    pub author: String,
    pub author_time_unix: i64,
    /// The subject line only; see [`commit_message`] for the full message.
    pub summary: String,
    /// `true` when the line differs from `HEAD`, in which case `author`/`summary` carry no
    /// commit information.
    pub is_uncommitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBlame {
    pub lines: Vec<BlameLine>,
    /// What `HEAD` resolved to when this was computed; part of the caller's cache key.
    pub head_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameOutcome {
    Blame(FileBlame),
    NotARepo,
    NotTracked,
}

/// "Nothing to show" is [`BlameOutcome::NotARepo`]/[`BlameOutcome::NotTracked`] rather than an
/// [`Error`]: both are expected, and a caller should render them as "no blame available".
pub fn blame_file(worktree_path: &Path, relative_path: &Path) -> Result<BlameOutcome, Error> {
    // Probing with `gix` first distinguishes "not a repo" without spawning `git`, and resolves
    // the `HEAD` this blame is cached against. An unborn `HEAD` has no history for any path.
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
        // `git blame` exits non-zero for a path with no history in `HEAD`, whether untracked or
        // absent. Nothing else here fails that way for a valid path.
        return Ok(BlameOutcome::NotTracked);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(BlameOutcome::Blame(FileBlame {
        lines: parse_line_porcelain(&text),
        head_commit,
    }))
}

/// Meant to be called lazily per sha rather than eagerly for every blamed line. `Ok(None)` for
/// the all-zero sha, for a non-hex `sha` (which is rejected before reaching `git` as an
/// argument), and for one `git log` does not recognize.
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

/// A block runs from a `<sha> <orig-line> <final-line>` header through its `<key> <value>` fields
/// to the `\t`-prefixed content line that ends it. One malformed block is skipped rather than
/// aborting the file: losing a line's attribution beats losing the whole blame.
fn parse_line_porcelain(text: &str) -> Vec<BlameLine> {
    let mut lines = Vec::new();

    let mut current_sha: Option<String> = None;
    let mut author: Option<String> = None;
    let mut author_time: Option<i64> = None;
    let mut summary: Option<String> = None;

    for raw_line in text.split('\n') {
        if raw_line.starts_with('\t') {
            // Ends the block: emit if the header was complete, and reset either way.
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

/// Told apart by requiring exactly 40 hex characters followed by two or three numeric tokens; no
/// header key is hex-shaped and 40 characters long.
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
