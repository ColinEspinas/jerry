//! Index staging primitives: staging is an immediate mutation of the real git index, not a
//! UI-only intent recorded until commit time.
//!
//! [`stage_path`]/[`unstage_path`] mutate; [`staged_paths`] reads back, so a caller can re-derive
//! its own state from the index rather than assuming a worktree starts with nothing staged.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::{check_success, run_git};

/// Stages `path`, whether it is modified, untracked, or deleted - `git add` stages a deletion too.
pub fn stage_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["add".into(), "--".into(), path.as_os_str().to_owned()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Removes `path` from the index without touching the working tree; the inverse of [`stage_path`].
///
/// Idempotent, so a caller need not check [`staged_paths`] first just to avoid an error.
pub fn unstage_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["reset".into(), "--".into(), path.as_os_str().to_owned()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Discards one path's uncommitted changes, restoring it to `HEAD` in both index and working
/// tree, or deleting it outright if `HEAD` never had it.
///
/// **Irreversible.** Nothing is stashed behind the user's back: a silent stash would make the
/// caller's own confirmation a lie and pile up entries nothing ever mentions.
///
/// Which of the two branches runs is decided by asking git whether `HEAD` holds the path, rather
/// than by parsing a status letter:
///
/// - `HEAD` has it: `git checkout HEAD -- <path>` rewrites index and working tree together. Plain
///   `git checkout -- <path>` restores from the *index*, so an already-staged modification would
///   survive.
/// - `HEAD` does not: drop any index entry, then delete the file. `git checkout HEAD` has no blob
///   to restore here and fails. `--ignore-unmatch` covers the never-staged case.
pub fn discard_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    if head_has_path(worktree_path, path)? {
        let args: Vec<OsString> = vec![
            "checkout".into(),
            "HEAD".into(),
            "--".into(),
            path.as_os_str().to_owned(),
        ];
        let output = run_git(worktree_path, &args)?;
        return check_success(&args, &output);
    }

    let args: Vec<OsString> = vec![
        "rm".into(),
        "--cached".into(),
        "--quiet".into(),
        "--ignore-unmatch".into(),
        "--".into(),
        path.as_os_str().to_owned(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;

    let absolute = worktree_path.join(path);
    match std::fs::remove_file(&absolute) {
        Ok(()) => Ok(()),
        // Already gone is the goal state, not a failure.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::WorktreeIo(err)),
    }
}

/// A repository with no commits answers `false`, which is correct: nothing in it has a committed
/// state to go back to.
fn head_has_path(worktree_path: &Path, path: &Path) -> Result<bool, Error> {
    let mut spec = OsString::from("HEAD:");
    spec.push(path.as_os_str());
    let args: Vec<OsString> = vec!["cat-file".into(), "-e".into(), spec];
    let output = run_git(worktree_path, &args)?;
    Ok(output.status.success())
}

pub fn staged_paths(worktree_path: &Path) -> Result<HashSet<PathBuf>, Error> {
    let args: Vec<OsString> = vec!["diff".into(), "--cached".into(), "--name-only".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Every path with a live uncommitted delta: staged, modified, deleted, or untracked.
///
/// This is what separates a *committed* change from an *uncommitted* one, which a
/// [`crate::diff::WorktreeDiff`] cannot answer alone: `diff_against_base` compares against the
/// merge-base, so its file list mixes both. A path missing from this set has nothing to `git add`,
/// and presenting it as stageable would be wrong.
///
/// A rename or copy contributes both of its paths. `--untracked-files=all`, not the `normal` that
/// [`crate::is_dirty`] uses, because `normal` collapses an untracked directory to a single entry -
/// leaving the files inside it absent, which every caller here would read as "clean".
pub fn dirty_paths(worktree_path: &Path) -> Result<HashSet<PathBuf>, Error> {
    let args: Vec<OsString> = vec![
        "status".into(),
        "--porcelain".into(),
        "-z".into(),
        "--untracked-files=all".into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(parse_status_porcelain_z(&output.stdout))
}

/// Records are `XY<space><path>`, NUL-terminated. A record existing at all means that path has a
/// delta, so the column values need no interpreting - except `R`/`C`, which are followed by a
/// second field holding the original path.
///
/// Non-UTF-8 paths land under their lossy form. They then fail to match the equally-lossy path a
/// [`crate::diff::DiffFile`] carries, so they degrade to "not known to be clean" rather than being
/// reported as committed.
fn parse_status_porcelain_z(stdout: &[u8]) -> HashSet<PathBuf> {
    let mut paths = HashSet::new();
    let mut records = stdout.split(|byte| *byte == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        // `XY` + a space + at least one path byte.
        if record.len() < 4 {
            continue;
        }
        let (x, y) = (record[0], record[1]);
        paths.insert(path_from_bytes(&record[3..]));
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            if let Some(original) = records.next() {
                paths.insert(path_from_bytes(original));
            }
        }
    }
    paths
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use test_support::{git, git_output, seed_repo};

    #[test]
    fn stage_path_really_adds_a_modified_file_to_the_real_index() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        stage_path(repo.path(), Path::new("file.txt")).expect("stage_path");

        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(
            status, "M  file.txt",
            "file.txt must be staged (M in the index column), not merely modified on disk"
        );
    }

    #[test]
    fn stage_path_really_adds_a_new_untracked_file() {
        let repo = seed_repo();
        fs::write(repo.path().join("new.txt"), "new\n").expect("write new file");

        stage_path(repo.path(), Path::new("new.txt")).expect("stage_path");

        let status = git_output(repo.path(), &["status", "--porcelain", "new.txt"]);
        assert_eq!(
            status, "A  new.txt",
            "a new file must be staged as an addition"
        );
    }

    #[test]
    fn unstage_path_really_removes_a_path_from_the_real_index_without_touching_the_working_tree() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        git(repo.path(), &["add", "file.txt"]);
        let staged_before = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(staged_before, "M  file.txt");

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path");

        // `git_output` trims the whole string, so the leading `X` status char of a real
        // `" M file.txt"` porcelain line (unstaged-only modification: `X` is a blank index
        // column, `Y` is `M`) is trimmed away along with it - "M file.txt" (one space, not two)
        // is the real, correctly-unstaged shape here, not a staged `"M  file.txt"` (two spaces).
        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(
            status, "M file.txt",
            "file.txt must be unstaged (working-tree-only M) after a real unstage"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read file.txt"),
            "changed\n",
            "unstaging must never touch the real working-tree content"
        );
    }

    #[test]
    fn unstage_path_is_a_harmless_no_op_when_already_unstaged() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path on a clean index");

        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(status, "M file.txt");
    }

    #[test]
    fn staged_paths_reflects_the_real_git_index_including_files_staged_by_something_else() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        fs::write(repo.path().join("also.txt"), "also\n").expect("write");
        // Staged directly via a real `git add`, standing in for something other than
        // `stage_path` having touched the index first (an agent CLI, a manual `git add`) -
        // `staged_paths` must see it regardless of who staged it.
        git(repo.path(), &["add", "file.txt", "also.txt"]);

        let staged = staged_paths(repo.path()).expect("staged_paths");

        assert_eq!(
            staged,
            [PathBuf::from("file.txt"), PathBuf::from("also.txt")]
                .into_iter()
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn staged_paths_is_empty_on_a_clean_index() {
        let repo = seed_repo();
        assert!(staged_paths(repo.path()).expect("staged_paths").is_empty());
    }

    #[test]
    fn staged_paths_never_includes_an_unstaged_modification() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed but not staged\n").expect("modify");

        let staged = staged_paths(repo.path()).expect("staged_paths");

        assert!(
            staged.is_empty(),
            "a real, unstaged modification must never appear in staged_paths"
        );
    }

    /// A real feature branch with one commit since its merge-base with `main` - the exact shape
    /// GitHub issue #220 is about. `committed.txt` is genuinely part of a commit and clean on
    /// disk; the caller then dirties whatever else it wants to contrast against it.
    fn repo_with_a_committed_clean_file() -> TempDir {
        let dir = seed_repo();
        git(dir.path(), &["checkout", "-b", "feature"]);
        fs::write(dir.path().join("committed.txt"), "committed\n").expect("write");
        git(dir.path(), &["add", "committed.txt"]);
        git(
            dir.path(),
            &["commit", "-m", "a real commit on the feature branch"],
        );
        dir
    }

    #[test]
    fn dirty_paths_is_empty_on_a_genuinely_clean_worktree() {
        let repo = repo_with_a_committed_clean_file();
        assert!(
            dirty_paths(repo.path()).expect("dirty_paths").is_empty(),
            "a worktree whose only difference from main is a real, clean commit has no live \
             uncommitted delta at all"
        );
    }

    #[test]
    fn dirty_paths_tells_a_committed_clean_file_apart_from_a_really_edited_one() {
        let repo = repo_with_a_committed_clean_file();
        fs::write(repo.path().join("file.txt"), "really edited\n").expect("modify");

        let dirty = dirty_paths(repo.path()).expect("dirty_paths");

        assert_eq!(
            dirty,
            [PathBuf::from("file.txt")]
                .into_iter()
                .collect::<HashSet<_>>(),
            "only the really-edited file has a live delta; committed.txt is committed and clean"
        );
    }

    #[test]
    fn dirty_paths_includes_a_staged_file() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");
        git(repo.path(), &["add", "file.txt"]);

        assert!(
            dirty_paths(repo.path())
                .expect("dirty_paths")
                .contains(Path::new("file.txt")),
            "a staged-only change (`M  file.txt`: index column set, working-tree column blank) \
             is still a live uncommitted delta"
        );
    }

    #[test]
    fn dirty_paths_includes_an_unstaged_file() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        assert!(
            dirty_paths(repo.path())
                .expect("dirty_paths")
                .contains(Path::new("file.txt")),
            "an unstaged-only change (` M file.txt`) is a live uncommitted delta"
        );
    }

    #[test]
    fn dirty_paths_includes_an_untracked_file() {
        let repo = seed_repo();
        fs::write(repo.path().join("brand-new.txt"), "new\n").expect("write");

        assert!(
            dirty_paths(repo.path())
                .expect("dirty_paths")
                .contains(Path::new("brand-new.txt")),
            "an untracked file (`?? brand-new.txt`) has never been committed, so it is a live \
             uncommitted delta - `git diff --cached` would never have reported it"
        );
    }

    #[test]
    fn dirty_paths_includes_a_deleted_tracked_file() {
        let repo = seed_repo();
        fs::remove_file(repo.path().join("file.txt")).expect("remove");

        assert!(
            dirty_paths(repo.path())
                .expect("dirty_paths")
                .contains(Path::new("file.txt")),
            "a deletion (` D file.txt`) is a live uncommitted delta too"
        );
    }

    #[test]
    fn dirty_paths_includes_both_halves_of_a_live_rename() {
        let repo = seed_repo();
        git(repo.path(), &["mv", "file.txt", "renamed.txt"]);

        let dirty = dirty_paths(repo.path()).expect("dirty_paths");

        // `git mv` stages the rename, so real porcelain -z output here is a single
        // `R  renamed.txt\0file.txt\0` record: the new path in the record itself, the original
        // path in the *following* NUL-terminated field.
        assert!(
            dirty.contains(Path::new("renamed.txt")),
            "the rename's destination must be dirty; got {dirty:?}"
        );
        assert!(
            dirty.contains(Path::new("file.txt")),
            "the rename's source must be dirty too - a live rename is an uncommitted delta at \
             the old path as much as the new one; got {dirty:?}"
        );
    }

    #[test]
    fn dirty_paths_reports_a_nested_path_relative_to_the_worktree_root() {
        let repo = seed_repo();
        fs::create_dir_all(repo.path().join("src/db")).expect("mkdir");
        fs::write(repo.path().join("src/db/query.rs"), "fn q() {}\n").expect("write");

        assert!(
            dirty_paths(repo.path())
                .expect("dirty_paths")
                .contains(Path::new("src/db/query.rs")),
            "a brand-new file inside a brand-new directory must be listed individually and \
             worktree-relative - `--untracked-files=normal` would have collapsed the whole thing \
             into a single `?? src/` entry, leaving this path looking committed and clean"
        );
    }

    #[test]
    fn dirty_paths_handles_a_path_with_a_space_without_quoting_it() {
        let repo = seed_repo();
        fs::write(repo.path().join("my file.txt"), "spaces\n").expect("write");

        assert!(
            dirty_paths(repo.path())
                .expect("dirty_paths")
                .contains(Path::new("my file.txt")),
            "the path must come back raw, not wrapped in the quotes non-`-z` porcelain adds"
        );
    }

    #[test]
    fn dirty_paths_and_staged_paths_agree_on_which_half_a_change_sits_in() {
        let repo = repo_with_a_committed_clean_file();
        fs::write(repo.path().join("file.txt"), "staged edit\n").expect("modify");
        stage_path(repo.path(), Path::new("file.txt")).expect("stage_path");
        fs::write(repo.path().join("also.txt"), "unstaged new file\n").expect("write");

        let dirty = dirty_paths(repo.path()).expect("dirty_paths");
        let staged = staged_paths(repo.path()).expect("staged_paths");

        assert_eq!(
            dirty,
            [PathBuf::from("file.txt"), PathBuf::from("also.txt")]
                .into_iter()
                .collect::<HashSet<_>>()
        );
        assert_eq!(staged, [PathBuf::from("file.txt")].into_iter().collect());
        assert!(
            !dirty.contains(Path::new("committed.txt")),
            "neither query may claim the committed-clean file has anything left to stage"
        );
        assert!(!staged.contains(Path::new("committed.txt")));
    }

    #[test]
    fn stage_then_unstage_round_trips_back_to_a_clean_staged_set() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        stage_path(repo.path(), Path::new("file.txt")).expect("stage_path");
        assert_eq!(
            staged_paths(repo.path()).expect("staged_paths"),
            [PathBuf::from("file.txt")].into_iter().collect()
        );

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path");
        assert!(staged_paths(repo.path()).expect("staged_paths").is_empty());
    }

    #[test]
    fn discard_path_really_restores_a_modified_file_from_head() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "the agent wrote this\n").expect("modify");

        discard_path(repo.path(), Path::new("file.txt")).expect("discard_path");

        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "hello\n",
            "discard must put the file back exactly as HEAD has it"
        );
        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "file.txt"]),
            "",
            "and leave nothing dirty behind"
        );
    }

    #[test]
    fn discard_path_also_drops_a_change_that_was_already_staged() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "the agent wrote this\n").expect("modify");
        git(repo.path(), &["add", "file.txt"]);
        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "file.txt"]),
            "M  file.txt"
        );

        discard_path(repo.path(), Path::new("file.txt")).expect("discard_path");

        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "hello\n"
        );
        assert!(
            staged_paths(repo.path()).expect("staged_paths").is_empty(),
            "the index entry must be gone too, not just the working-tree edit"
        );
    }

    #[test]
    fn discard_path_restores_a_file_the_agent_deleted() {
        let repo = seed_repo();
        fs::remove_file(repo.path().join("file.txt")).expect("delete");

        discard_path(repo.path(), Path::new("file.txt")).expect("discard_path");

        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read"),
            "hello\n",
            "a deleted tracked file comes back"
        );
    }

    #[test]
    fn discard_path_removes_an_untracked_file_outright() {
        let repo = seed_repo();
        fs::write(repo.path().join("new.txt"), "brand new\n").expect("write");

        discard_path(repo.path(), Path::new("new.txt")).expect("discard_path");

        assert!(
            !repo.path().join("new.txt").exists(),
            "HEAD has no version of a brand-new file to restore, so discarding it means \
             deleting it"
        );
    }

    #[test]
    fn discard_path_removes_a_staged_addition_from_both_the_index_and_the_disk() {
        let repo = seed_repo();
        fs::write(repo.path().join("new.txt"), "brand new\n").expect("write");
        git(repo.path(), &["add", "new.txt"]);
        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "new.txt"]),
            "A  new.txt"
        );

        discard_path(repo.path(), Path::new("new.txt")).expect("discard_path");

        assert!(!repo.path().join("new.txt").exists());
        assert!(
            staged_paths(repo.path()).expect("staged_paths").is_empty(),
            "the staged addition must be gone from the real index"
        );
    }

    #[test]
    fn discarding_one_path_leaves_every_other_dirty_path_alone() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "edited\n").expect("modify");
        fs::write(repo.path().join("other.txt"), "also new\n").expect("write");

        discard_path(repo.path(), Path::new("file.txt")).expect("discard_path");

        assert_eq!(
            fs::read_to_string(repo.path().join("other.txt")).expect("read"),
            "also new\n",
            "discard is per-file - it must never touch a neighbouring change"
        );
    }
}
