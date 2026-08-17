//! Review baselines: snapshotting a worktree as a git tree, and diffing against that later.
//!
//! A different question from [`crate::diff`]: the base is a moment rather than a branch, so a
//! worktree long diverged from `main` has a large git diff and an empty review diff. Everything
//! downstream of the base point reuses [`crate::diff::compute_diff`].
//!
//! Snapshotting runs alongside a live agent, so it touches neither index, working tree, `HEAD`
//! nor stash - every mutation lands in a throwaway `GIT_INDEX_FILE`. It does write blobs and
//! trees; [`anchor_tree`] keeps `gc` off a baseline still in use.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::diff::{
    capture_git_stdout, compute_diff, prepare_shadow_index, validate_object_id, ShadowIndexContent,
    WorktreeDiff, MAX_DIFF_OUTPUT_BYTES,
};
use crate::error::Error;
use crate::{check_success, format_args, git_command, run_git};

/// Cap on untracked bytes a snapshot will hash before falling back to a tracked-only baseline.
///
/// Without it, an un-ignored build directory - hundreds of megabytes is realistic - would be
/// hashed into `.git/objects` on every agent spawn, and left as loose objects until a `git gc
/// --prune` two weeks later. 128 MiB sits well above any legitimate untracked set while bounding
/// the pathological case to seconds. Gitignored content is never counted.
pub const MAX_UNTRACKED_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;

/// A separate cap because per-file cost is not only bytes: each path costs a `stat`, an index
/// entry and a loose object, so many tiny files slip under a byte cap causing the same problem.
pub const MAX_UNTRACKED_SNAPSHOT_FILES: usize = 5_000;

/// Its own top-level namespace, the idiom `refs/stash` and `refs/notes/*` use: a baseline is
/// neither branch nor tag, and putting it in either would surface bookkeeping in `git branch` and
/// in the user's own tooling. Refs outside the well-known namespaces still protect objects from
/// `gc`.
pub const REVIEW_REF_PREFIX: &str = "refs/jerry/review/";

/// Must be passed back to [`diff_against_tree`]/[`changed_paths_against_tree`]: measuring a
/// tracked-only baseline under the other rules reports every pre-existing untracked file as new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackedCoverage {
    /// Untracked, non-ignored files were captured with their content.
    Included,
    /// The untracked set exceeded [`MAX_UNTRACKED_SNAPSHOT_BYTES`] or
    /// [`MAX_UNTRACKED_SNAPSHOT_FILES`], so only tracked files are covered.
    ///
    /// Such reviews are narrower but honest: they say nothing about untracked files in either
    /// direction, and callers are expected to surface that rather than imply a complete answer.
    Excluded,
}

impl UntrackedCoverage {
    /// The matching shadow-index flavour, kept in one place so a snapshot and every later diff
    /// against it agree about whether untracked files are in scope.
    fn shadow_index_content(self, for_snapshot: bool) -> ShadowIndexContent {
        match (self, for_snapshot) {
            (UntrackedCoverage::Included, true) => ShadowIndexContent::FullContent,
            // A diff reads the working tree directly, so a stub entry is enough to make the path
            // visible - no untracked content needs hashing.
            (UntrackedCoverage::Included, false) => ShadowIndexContent::IntentToAdd,
            (UntrackedCoverage::Excluded, _) => ShadowIndexContent::TrackedOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    pub tree_id: String,
    pub untracked: UntrackedCoverage,
    /// Untracked, non-ignored files found at capture time, kept even when they were included so a
    /// caller can explain why a baseline is tracked-only. Saturates at
    /// [`MAX_UNTRACKED_SNAPSHOT_FILES`] + 1 when counting stopped early.
    pub untracked_files: usize,
    /// Their total size. `0` when counting stopped on the file cap before sizes were summed.
    pub untracked_bytes: u64,
}

/// Measures exactly what a [`ShadowIndexContent::FullContent`] snapshot would hash, so gitignored
/// content is excluded here too.
///
/// Stops at [`MAX_UNTRACKED_SNAPSHOT_FILES`], returning `(cap + 1, 0)`: the answer is already
/// "too many", and stat-ing the rest is the unbounded work this exists to avoid.
fn measure_untracked(worktree_path: &Path) -> Result<(usize, u64), Error> {
    let args: Vec<OsString> = vec![
        "ls-files".into(),
        "-o".into(),
        "--exclude-standard".into(),
        "-z".into(),
    ];
    let (output, truncated) =
        capture_git_stdout(worktree_path, &args, MAX_DIFF_OUTPUT_BYTES, None)?;
    if truncated {
        return Ok((MAX_UNTRACKED_SNAPSHOT_FILES + 1, 0));
    }

    let mut files = 0usize;
    let mut bytes = 0u64;
    for record in output.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        files += 1;
        if files > MAX_UNTRACKED_SNAPSHOT_FILES {
            return Ok((MAX_UNTRACKED_SNAPSHOT_FILES + 1, 0));
        }
        let relative = PathBuf::from(String::from_utf8_lossy(record).into_owned());
        // `symlink_metadata`: a symlink is staged as the link, so following it would over-count
        // and could wander outside the worktree.
        if let Ok(meta) = std::fs::symlink_metadata(worktree_path.join(&relative)) {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    Ok((files, bytes))
}

/// Snapshots the worktree as it stands - tracked modifications, staged changes, and, within the
/// caps, untracked files - as a git tree object.
///
/// Captures the working tree rather than `HEAD`: an agent that committed some of its work and
/// left the rest uncommitted has changed the same set of things either way.
///
/// The untracked set is measured first, and only staged with content if it fits within
/// [`MAX_UNTRACKED_SNAPSHOT_BYTES`] and [`MAX_UNTRACKED_SNAPSHOT_FILES`]; otherwise the snapshot
/// falls back to tracked paths and says so via [`WorktreeSnapshot::untracked`].
///
/// The index, working tree, `HEAD` and stash are left byte-identical - every `git add` runs
/// against a throwaway `GIT_INDEX_FILE` copy. New blobs and trees *are* written; anchor the
/// returned id with [`anchor_tree`] if it must survive a `git gc`.
///
/// Stages content rather than intent-to-add stubs, unlike [`crate::diff::diff_against_base`]:
/// `git write-tree` needs a blob to name and refuses a stub outright.
pub fn snapshot_worktree_tree(worktree_path: &Path) -> Result<WorktreeSnapshot, Error> {
    let (untracked_files, untracked_bytes) = measure_untracked(worktree_path)?;
    let untracked = if untracked_files > MAX_UNTRACKED_SNAPSHOT_FILES
        || untracked_bytes > MAX_UNTRACKED_SNAPSHOT_BYTES
    {
        UntrackedCoverage::Excluded
    } else {
        UntrackedCoverage::Included
    };

    let shadow_index = prepare_shadow_index(worktree_path, untracked.shadow_index_content(true))?;

    let args: Vec<OsString> = vec!["write-tree".into()];
    let output = git_command(worktree_path, &args)
        .env("GIT_INDEX_FILE", &shadow_index)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)?;

    let tree_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // Refused here rather than stored as a baseline every later diff would reject one at a time.
    validate_object_id(&tree_id, "written tree id")?;
    Ok(WorktreeSnapshot {
        tree_id,
        untracked,
        untracked_files,
        untracked_bytes,
    })
}

/// How the worktree differs right now from the `tree_id` snapshot.
///
/// A validated wrapper over [`crate::diff::compute_diff`], which resolves a tree id exactly as it
/// resolves a commit id - so the pinned config, shadow index, rename detection, caps and parser
/// all apply unchanged, and the result is structurally identical to a git diff.
///
/// `label` is descriptive only, landing in [`WorktreeDiff::base_branch`]; a review diff has no
/// base branch, so callers pass something like `"since it started"` rather than a branch name.
///
/// Errors if `tree_id` is not hex or git cannot resolve it, so a garbage-collected baseline fails
/// here rather than degrading into some other comparison.
pub fn diff_against_tree(
    worktree_path: &Path,
    tree_id: &str,
    untracked: UntrackedCoverage,
    label: String,
) -> Result<WorktreeDiff, Error> {
    validate_object_id(tree_id, "baseline tree id")?;
    compute_diff(
        worktree_path,
        tree_id,
        untracked.shadow_index_content(false),
        label,
    )
}

/// Just the paths that differ from the `tree_id` snapshot, with no hunk parsing.
///
/// The cheap counterpart to [`diff_against_tree`], for callers that need only a count or a file
/// list and would otherwise allocate every hunk of every file to get one.
///
/// `-z` so a path containing a newline, quote or backslash cannot be misread: git emits raw paths
/// and never applies C-style quoting, making `core.quotePath` irrelevant here.
///
/// Capped at [`MAX_DIFF_OUTPUT_BYTES`]; a record truncated by the cap is dropped rather than
/// returned as a path that was never reported.
pub fn changed_paths_against_tree(
    worktree_path: &Path,
    tree_id: &str,
    untracked: UntrackedCoverage,
) -> Result<Vec<PathBuf>, Error> {
    validate_object_id(tree_id, "baseline tree id")?;

    let shadow_index = prepare_shadow_index(worktree_path, untracked.shadow_index_content(false))?;

    let args: Vec<OsString> = vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--name-only".into(),
        "-z".into(),
        tree_id.into(),
    ];
    let (output, truncated) = capture_git_stdout(
        worktree_path,
        &args,
        MAX_DIFF_OUTPUT_BYTES,
        Some(&shadow_index),
    )?;

    // Records are NUL-terminated, so a complete run ends in an empty split; a truncated run's
    // final split is an incomplete path instead.
    let mut records: Vec<&[u8]> = output.split(|byte| *byte == 0).collect();
    if truncated {
        records.pop();
    }
    Ok(records
        .into_iter()
        .filter(|record| !record.is_empty())
        .map(|record| PathBuf::from(String::from_utf8_lossy(record).into_owned()))
        .collect())
}

/// Turns an arbitrary caller-supplied `key` into a full, always-valid ref name under
/// [`REVIEW_REF_PREFIX`].
///
/// Keys contain `/`, spaces and anything else a filesystem allows, none of it legal in a ref
/// name. Hashing the whole key avoids reimplementing `git check-ref-format`'s rules - and avoids
/// two keys sanitizing to the same name, which would silently share one baseline.
///
/// A digest rather than hex encoding, because hex doubles the length and a loose ref is a file
/// bounded by `NAME_MAX`. That capped the worktree path at roughly 107 bytes, past which
/// `update-ref` failed with "File name too long" and left the Review surface silently disabled.
/// SHA-256 is a fixed 64 characters, so the ref is always 82 bytes.
///
/// The cost is an unreadable ref name, acceptable for internal bookkeeping no user reads.
pub fn baseline_ref_name(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut encoded = String::with_capacity(REVIEW_REF_PREFIX.len() + digest.len() * 2);
    encoded.push_str(REVIEW_REF_PREFIX);
    for byte in digest {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

/// `true` if `name` sits under [`REVIEW_REF_PREFIX`] with a plain hex remainder.
///
/// Checked before every `git update-ref` here, so a bug or a hand-edited state file can never
/// point one of these at `refs/heads/main`. That matters most for [`delete_ref`].
fn is_review_ref(name: &str) -> bool {
    match name.strip_prefix(REVIEW_REF_PREFIX) {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        None => false,
    }
}

fn check_review_ref(name: &str) -> Result<(), Error> {
    if is_review_ref(name) {
        Ok(())
    } else {
        Err(Error::WorktreeIo(std::io::Error::other(format!(
            "{name} is not a {REVIEW_REF_PREFIX}* ref"
        ))))
    }
}

/// Points `ref_name` at the `tree_id` snapshot so `gc` cannot collect it, creating the ref or
/// moving an existing one.
///
/// A ref naming a tree rather than a commit is unusual but legal: refs name arbitrary objects,
/// and gc reachability follows the object, not its type. Wrapping each snapshot in a throwaway
/// commit would add an object, an author identity and a timestamp for nothing.
///
/// Refuses any `ref_name` outside [`REVIEW_REF_PREFIX`].
pub fn anchor_tree(worktree_path: &Path, ref_name: &str, tree_id: &str) -> Result<(), Error> {
    check_review_ref(ref_name)?;
    validate_object_id(tree_id, "baseline tree id")?;

    let args: Vec<OsString> = vec!["update-ref".into(), ref_name.into(), tree_id.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Deletes `ref_name`, releasing its snapshot's objects to a future `gc`.
///
/// Not an error when the ref is already gone: a caller cleaning up after an agent should not have
/// to tell "deleted it" from "it was not there".
///
/// Refuses any `ref_name` outside [`REVIEW_REF_PREFIX`] - without which a corrupted state file
/// naming `refs/heads/main` would delete a branch.
pub fn delete_ref(worktree_path: &Path, ref_name: &str) -> Result<(), Error> {
    check_review_ref(ref_name)?;

    let args: Vec<OsString> = vec!["update-ref".into(), "-d".into(), ref_name.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use test_support::{git, git_output, git_try, seed_repo};

    /// Snapshots, asserts untracked content was captured, and returns just the tree id.
    fn snapshot_tree(path: &Path) -> String {
        let snapshot = snapshot_worktree_tree(path).expect("snapshot");
        assert_eq!(
            snapshot.untracked,
            UntrackedCoverage::Included,
            "these fixtures are far under the untracked caps, so a tracked-only fallback here \
             would mean the cap logic is misfiring"
        );
        snapshot.tree_id
    }

    /// The core promise of a baseline: nothing has changed since it was taken, so the review
    /// diff against it is genuinely empty - even though this worktree has a real, non-empty
    /// *git* diff (an uncommitted edit and an untracked file), which is exactly the distinction
    /// GitHub issue #225 is about.
    #[test]
    fn a_fresh_snapshot_reports_no_review_changes_even_when_the_git_diff_is_not_empty() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        fs::write(repo.path().join("untracked.txt"), "brand new\n").expect("write");

        let tree = snapshot_tree(repo.path());
        let review = diff_against_tree(
            repo.path(),
            &tree,
            UntrackedCoverage::Included,
            "since it started".to_string(),
        )
        .expect("diff_against_tree");

        assert!(
            review.files.is_empty(),
            "nothing changed since the snapshot, so the review diff must be empty - got {:?}",
            review
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>()
        );

        // The git side does have changes, proving the empty review diff above means "nothing new
        // since you looked" rather than "empty worktree".
        let git_diff = crate::diff::diff_against_base(repo.path()).expect("diff_against_base");
        let git_files = git_diff.diff().expect("a real git diff").files.len();
        assert!(
            git_files > 0,
            "the git diff must be non-empty here, or this test isn't proving the two answers \
             can differ"
        );
    }

    #[test]
    fn changes_made_after_a_snapshot_show_up_in_the_review_diff_with_real_hunks() {
        let repo = seed_repo();
        let tree = snapshot_tree(repo.path());

        fs::write(repo.path().join("file.txt"), "hello\nafter the snapshot\n").expect("write");

        let review = diff_against_tree(
            repo.path(),
            &tree,
            UntrackedCoverage::Included,
            "since it started".to_string(),
        )
        .expect("diff_against_tree");
        assert_eq!(review.files.len(), 1, "exactly one file changed");
        let file = &review.files[0];
        assert_eq!(file.path, PathBuf::from("file.txt"));
        assert!(
            file.hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter())
                .any(|line| line.content == "after the snapshot"),
            "the real added line must be present in the parsed hunks - got {:?}",
            file.hunks
        );
        assert_eq!(
            review.base_commit, tree,
            "a review diff records the real tree it was taken against"
        );
    }

    #[test]
    fn an_untracked_file_created_after_a_snapshot_is_reported_as_an_addition() {
        let repo = seed_repo();
        let tree = snapshot_tree(repo.path());

        fs::write(repo.path().join("brand_new.txt"), "written by an agent\n").expect("write");

        let review = diff_against_tree(
            repo.path(),
            &tree,
            UntrackedCoverage::Included,
            "since".to_string(),
        )
        .expect("diff_against_tree");
        let paths: Vec<PathBuf> = review.files.iter().map(|file| file.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("brand_new.txt")]);
        assert_eq!(review.files[0].status, crate::diff::FileChangeStatus::Added);
    }

    #[test]
    fn a_file_untracked_at_snapshot_time_is_captured_with_its_real_content() {
        let repo = seed_repo();
        fs::write(repo.path().join("notes.txt"), "line one\n").expect("write");

        let tree = snapshot_tree(repo.path());
        fs::write(repo.path().join("notes.txt"), "line one\nline two\n").expect("write");

        let review = diff_against_tree(
            repo.path(),
            &tree,
            UntrackedCoverage::Included,
            "since".to_string(),
        )
        .expect("diff_against_tree");
        assert_eq!(review.files.len(), 1);
        assert_eq!(
            review.files[0].status,
            crate::diff::FileChangeStatus::Modified,
            "the snapshot really captured `line one`, so adding `line two` is a modification - \
             an intent-to-add stub would have made this a whole-file addition instead"
        );
        let added: Vec<&str> = review.files[0]
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| line.kind == crate::diff::DiffLineKind::Added)
            .map(|line| line.content.as_str())
            .collect();
        assert_eq!(added, vec!["line two"]);
    }

    #[test]
    fn snapshotting_a_worktree_leaves_real_git_status_byte_identical() {
        let repo = seed_repo();
        // A genuinely mixed state: one staged change, one unstaged change, one untracked file.
        fs::write(repo.path().join("staged.txt"), "staged content\n").expect("write");
        git(repo.path(), &["add", "staged.txt"]);
        fs::write(repo.path().join("file.txt"), "hello\nunstaged edit\n").expect("write");
        fs::write(repo.path().join("untracked.txt"), "untracked\n").expect("write");

        // Raw stdout, not `git_output`: this test's subject is byte-identity, so it must not
        // compare through a helper that trims.
        let status = |dir: &Path| git_try(dir, &["status", "--porcelain"]).stdout;
        let status_before = status(repo.path());
        let head_before = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let index_before = fs::read(repo.path().join(".git").join("index")).expect("read index");
        let stash_before = git_output(repo.path(), &["stash", "list"]);
        let rendered = String::from_utf8_lossy(&status_before).into_owned();
        assert!(
            rendered.contains("A  staged.txt")
                && rendered.contains(" M file.txt")
                && rendered.contains("?? untracked.txt"),
            "the test fixture must really have staged, unstaged and untracked entries - got:\n\
             {rendered}"
        );

        snapshot_worktree_tree(repo.path()).expect("snapshot");

        assert_eq!(
            status(repo.path()),
            status_before,
            "snapshotting must not change one byte of what git reports about the worktree"
        );
        assert_eq!(git_output(repo.path(), &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            fs::read(repo.path().join(".git").join("index")).expect("read index"),
            index_before,
            "the real index file must be untouched - every mutation goes to the shadow copy"
        );
        assert_eq!(git_output(repo.path(), &["stash", "list"]), stash_before);
    }

    #[test]
    fn a_snapshot_id_tracks_real_content_not_the_time_it_was_taken() {
        let repo = seed_repo();
        let first = snapshot_tree(repo.path());
        let unchanged = snapshot_tree(repo.path());
        assert_eq!(
            first, unchanged,
            "an unchanged worktree must snapshot to the same tree"
        );

        fs::write(repo.path().join("file.txt"), "hello\nchanged\n").expect("write");
        let after = snapshot_tree(repo.path());
        assert_ne!(
            first, after,
            "a real change must produce a genuinely different baseline"
        );
    }

    #[test]
    fn changed_paths_agrees_with_the_full_review_diffs_file_list() {
        let repo = seed_repo();
        let tree = snapshot_tree(repo.path());

        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        fs::write(repo.path().join("added.txt"), "new\n").expect("write");

        let mut paths = changed_paths_against_tree(repo.path(), &tree, UntrackedCoverage::Included)
            .expect("changed_paths");
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("added.txt"), PathBuf::from("file.txt")]
        );

        let mut full: Vec<PathBuf> = diff_against_tree(
            repo.path(),
            &tree,
            UntrackedCoverage::Included,
            "since".to_string(),
        )
        .expect("diff_against_tree")
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
        full.sort();
        assert_eq!(paths, full, "the cheap path list must match the full diff");
    }

    #[test]
    fn changed_paths_is_empty_immediately_after_a_snapshot() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        let tree = snapshot_tree(repo.path());
        assert!(
            changed_paths_against_tree(repo.path(), &tree, UntrackedCoverage::Included)
                .expect("changed_paths")
                .is_empty()
        );
    }

    /// Counts loose objects under `.git/objects` - what an unbounded `git add -A` would blow up.
    fn loose_object_count(repo: &Path) -> usize {
        let objects = repo.join(".git").join("objects");
        let Ok(shards) = std::fs::read_dir(&objects) else {
            return 0;
        };
        shards
            .flatten()
            .filter(|shard| {
                let name = shard.file_name();
                let name = name.to_string_lossy();
                name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(|shard| {
                std::fs::read_dir(shard.path())
                    .map(|entries| entries.flatten().count())
                    .unwrap_or(0)
            })
            .sum()
    }

    #[test]
    fn an_oversized_untracked_set_is_never_hashed_into_the_object_database() {
        let repo = seed_repo();
        // Comfortably past the file cap, deliberately tiny so the test stays fast - the file cap
        // exists precisely because many small files are as expensive as a few large ones.
        let junk = repo.path().join("build-output");
        std::fs::create_dir_all(&junk).expect("mkdir");
        for index in 0..(MAX_UNTRACKED_SNAPSHOT_FILES + 10) {
            fs::write(junk.join(format!("{index}.o")), b"x").expect("write");
        }

        let before = loose_object_count(repo.path());
        let snapshot = snapshot_worktree_tree(repo.path()).expect("snapshot");

        assert_eq!(
            snapshot.untracked,
            UntrackedCoverage::Excluded,
            "an untracked set past the cap must produce a tracked-only baseline"
        );
        let written = loose_object_count(repo.path()).saturating_sub(before);
        assert!(
            written < 100,
            "a tracked-only snapshot must not write the untracked set into the object database - \
             it wrote {written} new loose objects for {} untracked files",
            MAX_UNTRACKED_SNAPSHOT_FILES + 10
        );

        let paths = changed_paths_against_tree(repo.path(), &snapshot.tree_id, snapshot.untracked)
            .expect("changed_paths");
        assert!(
            paths.is_empty(),
            "nothing tracked has changed since the snapshot, and the untracked set must not leak \
             in as spurious additions either - got {paths:?}"
        );

        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        let paths = changed_paths_against_tree(repo.path(), &snapshot.tree_id, snapshot.untracked)
            .expect("changed_paths");
        assert_eq!(paths, vec![PathBuf::from("file.txt")]);
    }

    #[test]
    fn an_ordinary_untracked_set_is_still_captured_in_full() {
        let repo = seed_repo();
        fs::write(repo.path().join("scratch.txt"), "a normal untracked file\n").expect("write");

        let snapshot = snapshot_worktree_tree(repo.path()).expect("snapshot");
        assert_eq!(snapshot.untracked, UntrackedCoverage::Included);
        assert_eq!(snapshot.untracked_files, 1);
    }

    #[test]
    fn gitignored_content_does_not_count_toward_the_untracked_cap() {
        let repo = seed_repo();
        fs::write(repo.path().join(".gitignore"), "ignored/\n").expect("write");
        let ignored = repo.path().join("ignored");
        std::fs::create_dir_all(&ignored).expect("mkdir");
        // A handful is enough: this pins ignored files at *zero*, not below some threshold, so it
        // avoids the sibling test's several-thousand-file fixture.
        for index in 0..25 {
            fs::write(ignored.join(format!("{index}.o")), b"x").expect("write");
        }

        let snapshot = snapshot_worktree_tree(repo.path()).expect("snapshot");
        assert_eq!(
            snapshot.untracked,
            UntrackedCoverage::Included,
            "ignored files must never count toward the cap"
        );
        assert_eq!(
            snapshot.untracked_files, 1,
            "only the real untracked, non-ignored file (.gitignore itself) should be counted"
        );
    }

    #[test]
    fn an_anchored_baseline_survives_a_real_aggressive_gc() {
        let repo = seed_repo();
        fs::write(repo.path().join("only_here.txt"), "unreferenced content\n").expect("write");
        let tree = snapshot_tree(repo.path());

        let ref_name = baseline_ref_name("worktree-a|Claude|1700000000");
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");

        git(repo.path(), &["gc", "--prune=now", "--aggressive"]);

        assert_eq!(
            git_output(repo.path(), &["rev-parse", &ref_name]),
            tree,
            "the anchored ref must still resolve to the same tree after a real gc"
        );
        // And the tree's real content is still readable - not just the ref surviving.
        assert!(git_output(repo.path(), &["cat-file", "-p", &tree]).contains("only_here.txt"));
    }

    #[test]
    fn anchoring_again_moves_an_existing_baseline_ref_rather_than_failing() {
        let repo = seed_repo();
        let first = snapshot_tree(repo.path());
        let ref_name = baseline_ref_name("key");
        anchor_tree(repo.path(), &ref_name, &first).expect("anchor");

        fs::write(repo.path().join("file.txt"), "hello\nmarked reviewed\n").expect("write");
        let second = snapshot_tree(repo.path());
        anchor_tree(repo.path(), &ref_name, &second).expect("re-anchor");

        assert_eq!(
            git_output(repo.path(), &["rev-parse", &ref_name]),
            second,
            "'Mark reviewed' advances the same ref onto the new snapshot"
        );
    }

    #[test]
    fn deleting_a_baseline_ref_removes_it_and_is_idempotent() {
        let repo = seed_repo();
        let tree = snapshot_tree(repo.path());
        let ref_name = baseline_ref_name("key");
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");

        delete_ref(repo.path(), &ref_name).expect("delete");
        assert!(
            git_try(repo.path(), &["rev-parse", "--verify", &ref_name])
                .stdout
                .is_empty(),
            "the ref must really be gone"
        );

        delete_ref(repo.path(), &ref_name)
            .expect("deleting an already-deleted baseline ref must not be an error");
    }

    #[test]
    fn a_ref_outside_the_review_namespace_is_refused_and_the_branch_survives() {
        let repo = seed_repo();
        let head_before = git_output(repo.path(), &["rev-parse", "refs/heads/main"]);

        assert!(delete_ref(repo.path(), "refs/heads/main").is_err());
        assert!(anchor_tree(repo.path(), "refs/heads/main", &"0".repeat(40)).is_err());
        assert!(delete_ref(repo.path(), "refs/jerry/review/../../heads/main").is_err());

        assert_eq!(
            git_output(repo.path(), &["rev-parse", "refs/heads/main"]),
            head_before,
            "the real branch must be completely untouched"
        );
    }

    #[test]
    fn distinct_keys_never_collide_onto_one_baseline_ref() {
        let a = baseline_ref_name("/repo/wt a|Claude|1");
        let b = baseline_ref_name("/repo/wt-a|Claude|1");
        let c = baseline_ref_name("_repo_wt_a|Claude|1");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        for name in [&a, &b, &c] {
            assert!(name.starts_with(REVIEW_REF_PREFIX));
            assert!(
                is_review_ref(name),
                "{name} must be recognised as this module's own ref"
            );
        }
    }

    #[test]
    fn a_long_worktree_path_still_produces_a_usable_ref_name() {
        let repo = seed_repo();
        // A perfectly ordinary deep layout, well past the ~107-byte ceiling raw hex imposed.
        let long_key = format!(
            "/home/developer/Developer/some-organisation/{}/.worktrees/{}|Claude|1700000000",
            "a-reasonably-long-repository-name", "feature/a-descriptive-branch-name-here"
        );
        assert!(
            long_key.len() > 107,
            "the fixture must exceed the old ceiling, or it proves nothing"
        );

        let ref_name = baseline_ref_name(&long_key);
        assert_eq!(
            ref_name.len(),
            REVIEW_REF_PREFIX.len() + 64,
            "a digest-based ref name is a fixed length regardless of key length"
        );
        let component = ref_name.rsplit('/').next().expect("a final ref component");
        assert!(
            component.len() + ".lock".len() <= 255,
            "the ref's own filename, plus git's `.lock` suffix, must fit in NAME_MAX"
        );

        let tree = snapshot_tree(repo.path());
        anchor_tree(repo.path(), &ref_name, &tree).expect("a long key must still anchor");
        assert_eq!(git_output(repo.path(), &["rev-parse", &ref_name]), tree);
        delete_ref(repo.path(), &ref_name).expect("and must still be deletable");
    }

    #[test]
    fn ref_name_length_is_independent_of_key_length() {
        let short = baseline_ref_name("a");
        let long = baseline_ref_name(&"x".repeat(4096));
        assert_eq!(short.len(), long.len());
        assert_ne!(short, long, "but distinct keys still produce distinct refs");
    }

    #[test]
    fn an_encoded_ref_name_is_accepted_by_real_git_check_ref_format() {
        let repo = seed_repo();
        let nasty = "/home/u/my repo/../wt~1^2:3?4*5[6\\7.lock|Claude|1700000000";
        let ref_name = baseline_ref_name(nasty);

        let output = Command::new("git")
            .current_dir(repo.path())
            .args(["check-ref-format", &ref_name])
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git rejected the encoded ref name {ref_name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let tree = snapshot_tree(repo.path());
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");
        assert_eq!(git_output(repo.path(), &["rev-parse", &ref_name]), tree);
    }

    #[test]
    fn a_tree_id_that_is_not_a_hex_object_id_is_refused_before_git_ever_runs() {
        let repo = seed_repo();
        assert!(diff_against_tree(
            repo.path(),
            "--upload-pack=evil",
            UntrackedCoverage::Included,
            "x".to_string()
        )
        .is_err());
        assert!(changed_paths_against_tree(repo.path(), "", UntrackedCoverage::Included).is_err());
        assert!(diff_against_tree(
            repo.path(),
            "HEAD",
            UntrackedCoverage::Included,
            "x".to_string()
        )
        .is_err());
    }

    #[test]
    fn snapshots_are_scoped_to_the_worktree_they_were_taken_in() {
        let repo = seed_repo();
        let other = repo.path().parent().expect("parent").join("linked-wt");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                other.to_str().expect("utf8"),
            ],
        );

        fs::write(other.join("feature_only.txt"), "in the linked worktree\n").expect("write");

        let main_tree = snapshot_tree(repo.path());
        let linked_paths =
            changed_paths_against_tree(&other, &main_tree, UntrackedCoverage::Included)
                .expect("changed_paths in linked");
        assert_eq!(
            linked_paths,
            vec![PathBuf::from("feature_only.txt")],
            "the linked worktree's own new file is what differs from the main worktree's snapshot"
        );

        // Clean up the linked worktree so `TempDir`'s own drop isn't left with a registered
        // worktree pointing outside it.
        git(
            repo.path(),
            &[
                "worktree",
                "remove",
                "--force",
                other.to_str().expect("utf8"),
            ],
        );
    }
}
