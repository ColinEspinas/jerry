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
///
/// A tracked file deleted from disk but not yet staged is **excluded**: `--cached` alone would
/// keep listing it, but a file that is not on disk is not something the worktree *contains* -
/// rendering it as an ordinary row (with nothing behind it to open) is the phantom this
/// subtraction removes.
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

    let deleted_args: Vec<OsString> = vec!["ls-files".into(), "--deleted".into(), "-z".into()];
    let (deleted_output, deleted_truncated) =
        capture_git_stdout(worktree_path, &deleted_args, MAX_LIST_OUTPUT_BYTES, None)?;
    let deleted: std::collections::HashSet<&[u8]> = deleted_output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();

    let mut records: Vec<&[u8]> = output.split(|byte| *byte == 0).collect();
    if truncated {
        // The final slice may be a path sliced in half, and there is no way to tell that from a
        // complete one that happened to land on the boundary.
        records.pop();
    }

    let files = records
        .into_iter()
        .filter(|record| !record.is_empty() && !deleted.contains(record))
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect();

    Ok(WorktreeFileList {
        files,
        // A truncated deleted-set means some phantoms may have survived the subtraction, which
        // is the same "not the whole truth" callers must not prune against.
        truncated: truncated || deleted_truncated,
    })
}

/// Worktree-relative paths of this worktree's submodules (gitlink index entries), in git's own
/// `/`-separated format. Empty for the overwhelmingly common no-submodule case, gated by a
/// single `.gitmodules` stat so the extra `--stage` listing is never paid for it.
///
/// Callers need this because a plain `ls-files` prints a submodule as one pathless-of-content
/// record: without the mode there is no way to tell it from an ordinary file.
pub fn list_submodule_paths(worktree_path: &Path) -> Result<Vec<String>, Error> {
    if !worktree_path.join(".gitmodules").is_file() {
        return Ok(Vec::new());
    }
    let args: Vec<OsString> = vec![
        "ls-files".into(),
        "--cached".into(),
        "--stage".into(),
        "-z".into(),
    ];
    let (output, _truncated) =
        capture_git_stdout(worktree_path, &args, MAX_LIST_OUTPUT_BYTES, None)?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            // `<mode> <object> <stage>\t<path>` - a gitlink's mode is exactly 160000.
            let record = std::str::from_utf8(record).ok()?;
            let (meta, path) = record.split_once('\t')?;
            meta.starts_with("160000 ").then(|| path.to_string())
        })
        .collect())
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
    fn a_tracked_file_deleted_from_disk_but_not_staged_is_not_content() {
        let dir = seed_empty_repo();
        let root = dir.path();
        fs::write(root.join("kept.rs"), "// kept\n").expect("write");
        fs::write(root.join("doomed.rs"), "// doomed\n").expect("write");
        git(root, &["add", "kept.rs", "doomed.rs"]);
        git(root, &["commit", "-q", "-m", "both"]);

        fs::remove_file(root.join("doomed.rs")).expect("remove");

        let listing = list_worktree_files(root).expect("a real git worktree");
        assert!(
            listing.files.contains(&"kept.rs".to_string()),
            "the surviving file stays: {:?}",
            listing.files
        );
        assert!(
            !listing.files.contains(&"doomed.rs".to_string()),
            "an unstaged deletion is still tracked by the index, but the worktree does not \
             contain it - listing it renders a phantom row: {:?}",
            listing.files
        );
    }

    #[test]
    fn a_real_submodule_is_reported_as_a_gitlink_path() {
        let sub = seed_empty_repo();
        fs::write(sub.path().join("inner.rs"), "// inner\n").expect("write");
        git(sub.path(), &["add", "inner.rs"]);
        git(sub.path(), &["commit", "-q", "-m", "inner"]);

        let outer = seed_empty_repo();
        git(
            outer.path(),
            &[
                "-c",
                // Modern git refuses local-path submodules without this opt-in; the test is
                // exactly the local case.
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.path().to_str().expect("utf8 path"),
                "vendored",
            ],
        );

        let submodules = list_submodule_paths(outer.path()).expect("submodule listing");
        assert_eq!(submodules, vec!["vendored".to_string()]);

        let no_modules = seed_empty_repo();
        assert!(
            list_submodule_paths(no_modules.path())
                .expect("no-submodule repo")
                .is_empty(),
            "the common case pays one .gitmodules stat and no extra git spawn"
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
