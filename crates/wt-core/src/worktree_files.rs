//! Enumerating a worktree's *real* content: what `git` itself would show you, not what a raw
//! directory walk stumbles into.
//!
//! ## Why this exists
//!
//! The `app` crate's search panel (GitHub issue #162, `crate::search::engine` there) originally
//! listed candidate files with its own recursive `fs::read_dir` walk, skipping only `.git`. That
//! looked reasonable against the small fixture it was built and measured against, and was badly
//! wrong at real scale: this very repository's own checkout has 125,242 files on disk, of which
//! 99,250 sit under `target/` - this project's own `.gitignore`d Rust build output, currently
//! 31 GB. A raw walk has to open every directory in that subtree and `stat` every file in it just
//! to discover there was nothing there worth searching; `git ls-files --exclude-standard` never
//! opens a gitignored directory at all, because git's own exclude matching happens *before* it
//! descends, not after. Measured directly against this repository: `git ls-files --cached
//! --others --exclude-standard -z` returns all 25,992 real files in **73ms**; the search panel's
//! own `MAX_SCANNED_FILES` cap (20,000) is smaller than `target/` alone (99,250 files), so the
//! raw walk it replaces was hitting that cap - and reporting itself truncated - entirely inside
//! `target/`, before a single real source file was ever read.
//!
//! This is also the same failure mode `review::MAX_UNTRACKED_SNAPSHOT_BYTES` already documents
//! from the git-snapshot side of this codebase - a 19 GB, 21,471-file untracked build directory
//! that an unconditional `git add -A` would have hashed on every agent spawn. Two different
//! features, hand-rolled two different times, tripped on the same real directory for the same
//! real reason. This module is the one place that answers "what does this worktree actually
//! contain" so a third feature doesn't hand-roll it a third time.
//!
//! ## What "real content" means here
//!
//! `--cached` (tracked, regardless of what `.gitignore` says now - an explicitly tracked file
//! stays visible even under a later-added ignore rule, exactly like `git status` treats it) plus
//! `--others --exclude-standard` (untracked, but only the untracked files git itself would ever
//! offer to stage - i.e. not `.gitignore`d, not `.git/info/exclude`d, not `core.excludesFile`d).
//! That is the same list `review::measure_untracked` already trusted for exactly this reason -
//! this module generalizes it into something a second caller can use directly instead of
//! re-deriving its own copy of the same two flags.
//!
//! Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::path::Path;

use crate::diff::capture_git_stdout;
use crate::error::Error;

/// How many bytes of `git ls-files -z` stdout this will buffer before giving up on the rest and
/// reporting [`WorktreeFileList::truncated`] instead.
///
/// Generous on purpose: even a real path averaging 100 bytes, this holds well over 300,000
/// entries - comfortably past `search::engine::MAX_SCANNED_FILES` (20,000), which is the cap a
/// caller like the search panel applies on top of this one regardless. This cap exists only to
/// bound memory and read time against a repository with a genuinely enormous tracked file count,
/// not to be the search panel's own truncation notice.
pub const MAX_LIST_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// The result of [`list_worktree_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeFileList {
    /// Worktree-relative, `/`-separated paths (`git`'s own path format, and the same format
    /// `search::engine::FileMatches::relative` already uses) - one entry per real file.
    pub files: Vec<String>,
    /// `true` when the real list was longer than [`MAX_LIST_OUTPUT_BYTES`] would hold. The last
    /// record read is dropped rather than kept when this is set, since a byte cap can only ever
    /// land mid-path, never reliably on a record boundary.
    pub truncated: bool,
}

/// Lists every real file `worktree_path` contains, the way `git` itself would: every tracked
/// path, plus every untracked path `git` would actually offer to stage - see this module's own
/// docs for why that, and not a raw filesystem walk, is the right definition of "real content".
///
/// Returns `Err` when `worktree_path` is not (or is no longer) inside a real git worktree, or
/// `git` itself could not be run at all - a caller that must keep working outside a git repo
/// needs its own fallback for that case, the same way `search::engine::search_worktree_cancellable`
/// falls back to a plain recursive walk.
pub fn list_worktree_files(worktree_path: &Path) -> Result<WorktreeFileList, Error> {
    let args: Vec<OsString> = vec![
        "ls-files".into(),
        "--cached".into(),
        "--others".into(),
        "--exclude-standard".into(),
        "-z".into(),
    ];
    let (output, truncated) =
        capture_git_stdout(worktree_path, &args, MAX_LIST_OUTPUT_BYTES, None)?;

    let mut records: Vec<&[u8]> = output.split(|byte| *byte == 0).collect();
    if truncated {
        // The very last slice is whatever the child had written up to the byte cap - it may be
        // a complete path that happened to land on a boundary, but there is no way to tell that
        // apart from a path sliced in half, so it is dropped rather than risk reporting a
        // truncated filename as a real one.
        records.pop();
    }

    let files = records
        .into_iter()
        .filter(|record| !record.is_empty())
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect();

    Ok(WorktreeFileList { files, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        dir
    }

    #[test]
    fn lists_tracked_and_untracked_but_never_a_gitignored_build_directory() {
        let dir = init_repo();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n").expect("write");
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").expect("write");
        git(root, &["add", ".gitignore", "src/lib.rs"]);
        git(root, &["commit", "-q", "-m", "init"]);

        // An untracked file that is not ignored - must show up.
        fs::write(root.join("src/new.rs"), "// new\n").expect("write");
        // A real, sizeable gitignored build directory - must never show up, and must never even
        // need to be individually stat-ed by the caller.
        fs::create_dir_all(root.join("target/debug/deps")).expect("mkdir");
        for i in 0..50 {
            fs::write(
                root.join(format!("target/debug/deps/artifact-{i}.o")),
                "binary junk",
            )
            .expect("write");
        }

        let listing = list_worktree_files(root).expect("a real git worktree");
        assert!(!listing.truncated);
        assert!(
            listing.files.contains(&"src/lib.rs".to_string()),
            "tracked files must be listed: {:?}",
            listing.files
        );
        assert!(
            listing.files.contains(&"src/new.rs".to_string()),
            "untracked, non-ignored files must be listed: {:?}",
            listing.files
        );
        assert!(
            !listing.files.iter().any(|f| f.starts_with("target/")),
            "a gitignored build directory must never appear: {:?}",
            listing.files
        );
    }

    #[test]
    fn an_explicitly_tracked_file_stays_listed_even_under_a_later_ignore_rule() {
        let dir = init_repo();
        let root = dir.path();
        fs::create_dir_all(root.join("generated")).expect("mkdir");
        fs::write(root.join("generated/schema.rs"), "// generated\n").expect("write");
        git(root, &["add", "generated/schema.rs"]);
        git(root, &["commit", "-q", "-m", "track generated file"]);

        fs::write(root.join(".gitignore"), "generated/\n").expect("write");

        let listing = list_worktree_files(root).expect("a real git worktree");
        assert!(
            listing.files.contains(&"generated/schema.rs".to_string()),
            "an explicitly tracked path must not disappear just because a later .gitignore rule \
             would otherwise hide it - `git status` does not hide it either: {:?}",
            listing.files
        );
    }

    #[test]
    fn a_directory_that_is_not_a_git_worktree_is_a_real_error_not_an_empty_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = list_worktree_files(dir.path());
        assert!(
            result.is_err(),
            "a non-repo must be a real error a caller can fall back on, not a silent empty result"
        );
    }
}
