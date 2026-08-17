//! Enumerating a worktree's content the way `git` sees it, rather than by walking the filesystem.
//!
//! "Content" is `--cached` (tracked, even under a later-added ignore rule, as `git status` treats
//! it) plus `--others --exclude-standard` (untracked, minus anything git would refuse to stage).
//!
//! This is the one place that answers "what does this worktree contain", so that callers do not
//! hand-roll a directory walk - which descends into gitignored build output before it can discover
//! there is nothing there. See `docs/architecture/decisions.md` §6.

use std::ffi::OsString;
use std::path::Path;

use crate::diff::capture_git_stdout;
use crate::error::Error;

/// Cap on buffered `git ls-files -z` stdout, past which [`WorktreeFileList::truncated`] is set.
///
/// Generous on purpose - well over 300,000 entries - because callers apply their own, smaller
/// caps on top. This one only bounds memory against an enormous repository.
pub const MAX_LIST_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeFileList {
    /// Worktree-relative, `/`-separated paths, in git's own path format.
    pub files: Vec<String>,
    /// `true` when the listing exceeded [`MAX_LIST_OUTPUT_BYTES`]; the last record read is
    /// dropped, since a byte cap can land mid-path.
    pub truncated: bool,
}

/// `Err` when this is not a git worktree, or `git` could not run at all; a caller that must work
/// outside a repository needs its own fallback.
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
        // The final slice may be a path sliced in half, and there is no way to tell that from a
        // complete one that happened to land on the boundary.
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
    use test_support::{git, seed_empty_repo};

    #[test]
    fn lists_tracked_and_untracked_but_never_a_gitignored_build_directory() {
        let dir = seed_empty_repo();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n").expect("write");
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").expect("write");
        git(root, &["add", ".gitignore", "src/lib.rs"]);
        git(root, &["commit", "-q", "-m", "init"]);

        fs::write(root.join("src/new.rs"), "// new\n").expect("write");
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
        let dir = seed_empty_repo();
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
