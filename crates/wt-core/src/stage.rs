//! Real git-index staging primitives for the Changes panel's staging checkbox
//! (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §5: "The checkbox **is**
//! staging"). Before this module existed, `app::sidebar::render::AdeApp::toggle_staged` only
//! flipped an in-memory `HashSet<PathBuf>` - real git never saw anything until the commit
//! composer's own `git add` ran at commit time (`crate::undo::commit_paths`). That contradicted
//! the design's explicit framing: checking the box is supposed to be real, immediate staging,
//! not a UI-only intent recorded for later.
//!
//! [`stage_path`]/[`unstage_path`] are the real, immediate mutations `toggle_staged` now calls
//! on every click. [`staged_paths`] is the read side: a real `git diff --cached --name-only`
//! query, used both to re-derive `AdeApp::staged_files` when a worktree is first loaded or
//! switched to (so a file already staged in the real index before Jerry ever touched it reads as
//! staged, rather than starting every worktree at an empty, UI-only set) and by this module's own
//! tests to verify the real index changed.
//!
//! Performs blocking I/O everywhere in this module (shells out to `git`); see the crate-level
//! docs on offloading this to a background thread.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::{check_success, run_git};

/// Real, immediate `git add -- <path>` - stages `path` (relative to `worktree_path`, or
/// absolute so long as it resolves inside it) in the real git index. Works identically for a
/// modified tracked file, a new untracked file, or a deleted tracked file (`git add` stages a
/// deletion too, the same way `wt_core::undo::commit_paths`'s own `git add -- <paths>` already
/// relies on for its "stage exactly these paths, including deletions" behavior).
///
/// Performs blocking I/O.
pub fn stage_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["add".into(), "--".into(), path.as_os_str().to_owned()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real, immediate unstage: `git reset -- <path>` removes `path` from the real git index
/// without touching the working tree - the exact inverse of [`stage_path`]. A no-op (real,
/// successful exit) if `path` was already unstaged, matching `git reset`'s own idempotent
/// behavior, so a caller never needs to check [`staged_paths`] first just to avoid an error.
///
/// Performs blocking I/O.
pub fn unstage_path(worktree_path: &Path, path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["reset".into(), "--".into(), path.as_os_str().to_owned()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// The real, current set of staged paths in `worktree_path`'s git index (`git diff --cached
/// --name-only`), worktree-relative. The live source of truth [`app::root::AdeApp::staged_files`]
/// re-derives from on every worktree load/switch, rather than starting empty and silently
/// disagreeing with a file already staged in the real index before Jerry ever opened this
/// worktree.
///
/// Performs blocking I/O.
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

/// Every worktree-relative path in `worktree_path` that has *any* live, uncommitted delta right
/// now - staged in the index, modified (or deleted) in the working tree, or untracked. The
/// per-path counterpart to [`crate::is_dirty`]'s whole-worktree boolean, and the signal that
/// tells a **committed** change apart from an **uncommitted** one.
///
/// That distinction is not derivable from a [`crate::diff::WorktreeDiff`] alone, which is what
/// made GitHub issue #220 ("Changes are displayed as unstaged but are commited") possible:
/// `diff_against_base` deliberately diffs the working tree against the *merge-base with the
/// default branch* (see that module's docs), so its file list mixes files whose only difference
/// from the base branch is already latched into a real commit on this branch with files that
/// really do have live, uncommitted edits. A path absent from this set is in the first group:
/// there is genuinely nothing to `git add`, so presenting it as a stageable "unstaged" file is a
/// lie. A path present in it is in the second group - and [`staged_paths`] says which *half* of
/// the index/working-tree split it sits on.
///
/// One `git status --porcelain -z --untracked-files=all` call reports both status columns for
/// every path at once, so this needs no second round trip to cover the unstaged half; `-z`
/// additionally makes git emit raw, never-quoted paths regardless of the caller's `core.quotePath`
/// setting (the same class of caller-config hazard [`crate::diff`] pins `-c core.quotePath=false`
/// for). A rename or copy contributes **both** of its paths, since a live rename is a real
/// uncommitted delta at the old path as much as the new one.
///
/// `--untracked-files=all`, not the `normal` [`crate::is_dirty`] uses: `normal` collapses a
/// wholly-untracked directory into a single `?? src/` entry, which is enough for a
/// whole-worktree yes/no but would leave `src/db/query.rs` *absent* from this set - and absent
/// means "committed and clean" to every caller here, the precise opposite of the truth for a
/// brand-new file. [`crate::diff`]'s shadow index lists those files individually, so this has to
/// as well.
///
/// Shells out to `git` rather than using `gix`, per this crate's split: `gix` for object-graph
/// reads, the real `git` CLI for anything that has to reproduce git's own porcelain status/diff
/// text exactly (see [`crate::diff`]'s "gix vs. the `git` CLI" docs).
///
/// Performs blocking I/O.
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

/// Parses `git status --porcelain -z` output into the set of paths it reports.
///
/// Each record is `XY<space><path>` terminated by a NUL, where `X` is the index (staged) column
/// and `Y` the working-tree column - the exact convention this module's own
/// [`unstage_path`] tests already document (`" M file.txt"` is unstaged-only, `"M  file.txt"` is
/// staged). Any record at all means that path has a live delta, so the columns' *values* don't
/// need interpreting here; only `R` (rename) and `C` (copy) do, because those records are followed
/// by a second NUL-terminated field holding the original path.
///
/// Paths are decoded with [`String::from_utf8_lossy`], matching [`staged_paths`]'s own decoding,
/// so a path that isn't valid UTF-8 lands in the set under its lossy form. That is a real
/// limitation, not a silent one: such a path simply won't match the equally-lossy path a
/// [`crate::diff::DiffFile`] carries either, so it falls back to being treated as "not known to be
/// clean" rather than being wrongly reported as committed.
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

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "initial commit"]);
        dir
    }

    #[test]
    fn stage_path_really_adds_a_modified_file_to_the_real_index() {
        let repo = init_repo();
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
        let repo = init_repo();
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
        let repo = init_repo();
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
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path on a clean index");

        let status = git_output(repo.path(), &["status", "--porcelain", "file.txt"]);
        assert_eq!(status, "M file.txt");
    }

    #[test]
    fn staged_paths_reflects_the_real_git_index_including_files_staged_by_something_else() {
        let repo = init_repo();
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
        let repo = init_repo();
        assert!(staged_paths(repo.path()).expect("staged_paths").is_empty());
    }

    #[test]
    fn staged_paths_never_includes_an_unstaged_modification() {
        let repo = init_repo();
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
        let dir = init_repo();
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

    /// The regression this function exists for: a file changed by a real commit on this branch,
    /// with no further edits, must not be reported as dirty, while a file with a real live edit
    /// must be - even though `wt_core::diff::diff_against_base` lists both.
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
        let repo = init_repo();
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
        let repo = init_repo();
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
        let repo = init_repo();
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
        let repo = init_repo();
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
        let repo = init_repo();
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
        let repo = init_repo();
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

    /// A path with a space in it is exactly what `core.quotePath`/quoting would mangle in
    /// non-`-z` porcelain output (`"my file.txt"`, with real quotes). `-z` emits it raw.
    #[test]
    fn dirty_paths_handles_a_path_with_a_space_without_quoting_it() {
        let repo = init_repo();
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
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "changed\n").expect("modify");

        stage_path(repo.path(), Path::new("file.txt")).expect("stage_path");
        assert_eq!(
            staged_paths(repo.path()).expect("staged_paths"),
            [PathBuf::from("file.txt")].into_iter().collect()
        );

        unstage_path(repo.path(), Path::new("file.txt")).expect("unstage_path");
        assert!(staged_paths(repo.path()).expect("staged_paths").is_empty());
    }
}
