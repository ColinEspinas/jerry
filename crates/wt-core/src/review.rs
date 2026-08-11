//! Per-agent **review** baselines: snapshotting a worktree's exact current state as a real git
//! tree, and diffing the working tree against one of those snapshots later.
//!
//! ## Why this is a separate concept from [`crate::diff`]
//!
//! GitHub issue #225 ("Separate diffs for git and agents"). [`crate::diff::diff_against_base`]
//! answers exactly one question: *how does this worktree differ from the merge-base with the
//! repository's default branch* - the **git** question, the one the Changes sidebar and the
//! File/Diff toggle are built around. It is a property of the branch, and it is the same answer
//! no matter who or what produced those changes.
//!
//! A **review** diff answers a different question: *what has changed since the point I last
//! looked*. Its base point is not a branch at all - it is a snapshot taken at a moment in time
//! (when an agent was spawned, or when the user last clicked "Mark reviewed"), and it has its own
//! lifetime, advancing only when the user says so. An agent spawned into a worktree whose branch
//! already diverged from `main` has a large git diff and an empty review diff, and that is the
//! honest answer to both questions - the confusion the issue reports is exactly what comes of
//! only ever being able to answer the first one.
//!
//! Everything *downstream* of the base point is deliberately shared, not duplicated: a review
//! diff is parsed into the very same [`crate::diff::WorktreeDiff`]/[`crate::diff::DiffFile`]/
//! [`crate::diff::DiffHunk`] types, by the very same parser, through the very same pinned
//! `git diff` invocation ([`crate::diff::compute_diff`], which resolves a tree id exactly as it
//! resolves a commit id). Only the base point and the lifetime differ.
//!
//! ## Snapshotting safely, alongside a live agent
//!
//! An agent CLI running inside the worktree is running its own `git` commands at unpredictable
//! moments (a documented, real hazard - see [`crate::undo::commit_paths`]' own docs). A snapshot
//! must therefore never touch the real index, the working tree, `HEAD`, or the stash.
//! [`snapshot_worktree_tree`] builds on [`crate::diff::prepare_shadow_index`]'s existing
//! mechanism for exactly that reason: every mutation lands in a throwaway `GIT_INDEX_FILE` copy.
//!
//! The one thing a snapshot genuinely *does* write is blobs and trees into the object database -
//! unavoidable, since "remember this exact working tree" has no other representation in git. That
//! is additive and invisible to every git command the agent might run; nothing existing is
//! rewritten. Those objects are then anchored under a real ref ([`anchor_tree`]) so a `git gc`
//! can't collect a baseline that is still in use.
//!
//! Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::diff::{
    capture_git_stdout, compute_diff, prepare_shadow_index, validate_object_id, ShadowIndexContent,
    WorktreeDiff, MAX_DIFF_OUTPUT_BYTES,
};
use crate::error::Error;
use crate::{check_success, format_args, git_command, run_git};

/// The ref namespace every review baseline is anchored under. Deliberately **not** under
/// `refs/heads/` or `refs/tags/`: a baseline is not a branch and not a tag, and putting it in
/// either would make it show up in `git branch`/`git tag`, in this app's own graph tab
/// (`crate::graph::build_graph` walks `refs/heads`, `refs/remotes` and `refs/tags`), and in a
/// user's own tooling - noise for something that is purely this app's internal bookkeeping.
///
/// A custom top-level namespace is the standard git idiom for exactly this (the same shape
/// `refs/stash`, `refs/notes/*` and `refs/replace/*` use); refs outside the well-known
/// namespaces are ignored by essentially everything while still being real, gc-protecting refs.
pub const REVIEW_REF_PREFIX: &str = "refs/jerry/review/";

/// Snapshots the worktree at `worktree_path` exactly as it stands right now - tracked
/// modifications, staged changes, and untracked files alike - as a real git tree object, and
/// returns its hex object id.
///
/// This is a *review baseline*: the "since" point a later [`diff_against_tree`] compares
/// against. It captures the working tree, not `HEAD`, because that is what a reviewer actually
/// wants a baseline for - an agent that has committed some of its work and left the rest
/// uncommitted has still changed the same set of things either way.
///
/// ## What it does not touch
///
/// The real index, the working tree, `HEAD`, and the stash are all left byte-identical. Every
/// `git add` here runs against a throwaway copy of the index via a `GIT_INDEX_FILE` override
/// ([`crate::diff::prepare_shadow_index`], whose own docs cover the mechanics, including the
/// racy-index mtime handling GitHub issue #163 needed), and `git write-tree` only reads that
/// copy. The `snapshotting_a_worktree_leaves_real_git_status_byte_identical` test proves this
/// against a real repository with real staged, unstaged and untracked files present.
///
/// It *does* write new blob and tree objects into the object database - see the module docs for
/// why that is both unavoidable and harmless. Anchor the returned id under a ref
/// ([`anchor_tree`]) if it needs to survive a `git gc`.
///
/// ## `FullContent`, not `--intent-to-add`
///
/// Unlike [`crate::diff::diff_against_base`]'s own shadow index, this one stages real content
/// ([`ShadowIndexContent::FullContent`]): `git write-tree` needs a real blob to name for every
/// entry, and refuses outright (`fatal: ... has intent-to-add entries`) on a stub.
///
/// Performs blocking I/O (spawns real `git` child processes).
pub fn snapshot_worktree_tree(worktree_path: &Path) -> Result<String, Error> {
    let shadow_index = prepare_shadow_index(worktree_path, ShadowIndexContent::FullContent)?;

    let args: Vec<OsString> = vec!["write-tree".into()];
    let output = git_command(worktree_path, &args)
        .env("GIT_INDEX_FILE", shadow_index.path())
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)?;

    let tree_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // `git write-tree` succeeding with output that isn't a hex object id would mean something is
    // very wrong; refusing it here (rather than storing it as a baseline that every later
    // `diff_against_tree` would reject one at a time) keeps the failure at the point it happened.
    validate_object_id(&tree_id, "written tree id")?;
    Ok(tree_id)
}

/// The real review diff: how the worktree at `worktree_path` differs *right now* from the
/// `tree_id` snapshot - i.e. everything that has changed since that baseline was taken.
///
/// A thin, validated wrapper over [`crate::diff::compute_diff`], deliberately reusing that
/// function verbatim rather than reimplementing it: `git diff <object>` resolves a tree id
/// exactly as it resolves a commit id, so the pinned config, the shadow index (untracked files
/// still show up as additions), `-M` rename detection, the output caps and the whole unified-diff
/// parser all apply unchanged. The result is a genuine [`WorktreeDiff`], structurally identical
/// to a git diff - only its base point differs.
///
/// `label` is purely descriptive, landing in [`WorktreeDiff::base_branch`]. A review diff has no
/// base *branch* at all, so callers pass something honest about what this is diffed against
/// (e.g. `"since it started"`) rather than a branch name that would be a lie.
///
/// Returns an error if `tree_id` isn't a hex object id, or if git can't resolve it - a baseline
/// whose objects were garbage-collected (never anchored, or its ref deleted) fails honestly here
/// rather than silently degrading into some other comparison.
///
/// Performs blocking I/O (spawns a real `git diff` child process).
pub fn diff_against_tree(
    worktree_path: &Path,
    tree_id: &str,
    label: String,
) -> Result<WorktreeDiff, Error> {
    validate_object_id(tree_id, "baseline tree id")?;
    compute_diff(worktree_path, tree_id, label)
}

/// Just the paths that differ from the `tree_id` snapshot - one `git diff --name-only` process,
/// no hunk parsing at all.
///
/// The cheap counterpart to [`diff_against_tree`], for callers that only need "how many files"
/// or "which files" and would otherwise pay to parse (and allocate) every hunk of every file for
/// a number. The session rail's per-agent `N files` trailing text is the real motivating caller:
/// it needs this for every visible agent row, on a surface that re-renders constantly.
///
/// Uses `-z` (NUL-separated records) rather than newline-separated output, so a path containing
/// a newline, a quote or a backslash can't be misread - with `-z`, git emits every path raw and
/// never applies its C-style quoting, which makes the `core.quotePath` handling
/// [`crate::diff`]'s own text parser needs irrelevant here.
///
/// Output is capped exactly as the full diff is ([`MAX_DIFF_OUTPUT_BYTES`]). If the cap truncates
/// mid-record, that final partial path is dropped rather than returned as a real path that was
/// never actually reported - a short list is honest, a mangled path is not.
///
/// Performs blocking I/O (spawns a real `git diff` child process).
pub fn changed_paths_against_tree(
    worktree_path: &Path,
    tree_id: &str,
) -> Result<Vec<PathBuf>, Error> {
    validate_object_id(tree_id, "baseline tree id")?;

    let shadow_index = prepare_shadow_index(worktree_path, ShadowIndexContent::IntentToAdd)?;

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
        Some(shadow_index.path()),
    )?;

    // With `-z` every complete record is NUL-*terminated*, so a well-formed run always ends in a
    // trailing empty split. A truncated run's final split is instead a real, incomplete path.
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
/// The real keys this app builds are `(worktree path, agent kind, spawn timestamp)` tuples -
/// containing `/`, spaces, `.`, and potentially any byte a filesystem allows, none of which can
/// go into a ref name raw (`git check-ref-format`'s rules reject `..`, a trailing `.`, `~^:?*[`,
/// control characters, a `.lock` suffix, and more). Rather than trying to sanitize each of those
/// rules individually - a list that is easy to get subtly wrong, and where a *collision* between
/// two different keys sanitizing to the same string would silently mean two agents sharing one
/// baseline - this lowercase-hex-encodes the whole key. Hex is injective (distinct keys always
/// produce distinct refs) and is trivially a valid ref-name component, with no rule left to get
/// wrong.
///
/// The cost is an unreadable ref name. That is an acceptable trade for internal bookkeeping refs
/// no user is expected to read; the human-readable form of the same fact lives in this app's own
/// persisted review state, next to the tree id.
pub fn baseline_ref_name(key: &str) -> String {
    let mut encoded = String::with_capacity(REVIEW_REF_PREFIX.len() + key.len() * 2);
    encoded.push_str(REVIEW_REF_PREFIX);
    for byte in key.as_bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

/// `true` if `name` is a ref this module produced - i.e. it really sits under
/// [`REVIEW_REF_PREFIX`] and its remainder is a plain hex component with nothing else in it.
///
/// Every mutating function here checks this before handing `name` to `git update-ref`, so a
/// caller can never (through a bug, or a hand-edited persisted state file) point one of these
/// operations at `refs/heads/main` or at anything else outside this app's own namespace. That is
/// a real concern for [`delete_ref`] specifically, which is a destructive operation.
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

/// Points `ref_name` at the `tree_id` snapshot, so a `git gc` can't collect a baseline that is
/// still in use. Creates the ref if it doesn't exist, and moves it if it does (the "Mark
/// reviewed" case - advancing a baseline to a fresh snapshot).
///
/// A ref pointing directly at a *tree* (rather than a commit) is unusual but entirely legal -
/// git refs name arbitrary objects, and reachability for gc purposes follows from the object,
/// not from its type. Wrapping each snapshot in a throwaway commit just to have a commit-shaped
/// ref would add an object, an author identity, and a timestamp this doesn't need.
///
/// Refuses any `ref_name` outside [`REVIEW_REF_PREFIX`] - see [`is_review_ref`].
///
/// Performs blocking I/O (spawns a real `git` child process).
pub fn anchor_tree(worktree_path: &Path, ref_name: &str, tree_id: &str) -> Result<(), Error> {
    check_review_ref(ref_name)?;
    validate_object_id(tree_id, "baseline tree id")?;

    let args: Vec<OsString> = vec!["update-ref".into(), ref_name.into(), tree_id.into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Deletes `ref_name`, releasing its snapshot's objects to a future `git gc` - what an agent
/// closing does to its own baseline ref.
///
/// Deliberately **not** an error when the ref doesn't exist: `git update-ref -d` on a missing ref
/// already exits zero, and "make sure this is gone" is idempotent by nature - a caller cleaning
/// up after an agent shouldn't have to distinguish "deleted it" from "it was already gone".
///
/// Refuses any `ref_name` outside [`REVIEW_REF_PREFIX`] - see [`is_review_ref`]. This one really
/// matters: without it, a corrupted persisted state file could name `refs/heads/main` here and
/// have this delete a real branch.
///
/// Performs blocking I/O (spawns a real `git` child process).
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

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        String::from_utf8_lossy(&output.stdout).into_owned()
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

    /// The core promise of a baseline: nothing has changed since it was taken, so the review
    /// diff against it is genuinely empty - even though this worktree has a real, non-empty
    /// *git* diff (an uncommitted edit and an untracked file), which is exactly the distinction
    /// GitHub issue #225 is about.
    #[test]
    fn a_fresh_snapshot_reports_no_review_changes_even_when_the_git_diff_is_not_empty() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        fs::write(repo.path().join("untracked.txt"), "brand new\n").expect("write");

        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");
        let review = diff_against_tree(repo.path(), &tree, "since it started".to_string())
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

        // The same worktree, at the same instant, really does have changes to show on the *git*
        // side - proving the empty review diff above is a genuine "nothing new since you looked",
        // not just an empty worktree.
        let git_diff = crate::diff::diff_against_base(repo.path()).expect("diff_against_base");
        let git_files = git_diff.diff().expect("a real git diff").files.len();
        assert!(
            git_files > 0,
            "the git diff must be non-empty here, or this test isn't proving the two answers \
             can differ"
        );
    }

    /// The other half: a real edit made *after* the snapshot must show up in the review diff,
    /// with real hunks - the same parsed shape a git diff produces.
    #[test]
    fn changes_made_after_a_snapshot_show_up_in_the_review_diff_with_real_hunks() {
        let repo = init_repo();
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");

        fs::write(repo.path().join("file.txt"), "hello\nafter the snapshot\n").expect("write");

        let review = diff_against_tree(repo.path(), &tree, "since it started".to_string())
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

    /// An *untracked* file created after a snapshot is a real change the reviewer needs to see -
    /// and the case `git diff <object>` would silently miss without the shadow index.
    #[test]
    fn an_untracked_file_created_after_a_snapshot_is_reported_as_an_addition() {
        let repo = init_repo();
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");

        fs::write(repo.path().join("brand_new.txt"), "written by an agent\n").expect("write");

        let review =
            diff_against_tree(repo.path(), &tree, "since".to_string()).expect("diff_against_tree");
        let paths: Vec<PathBuf> = review.files.iter().map(|file| file.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("brand_new.txt")]);
        assert_eq!(review.files[0].status, crate::diff::FileChangeStatus::Added);
    }

    /// A file that already existed *untracked* at snapshot time must be captured with its real
    /// content, so a later edit to it is a modification rather than reappearing as a whole-file
    /// addition. This is what `ShadowIndexContent::FullContent` buys over `--intent-to-add`.
    #[test]
    fn a_file_untracked_at_snapshot_time_is_captured_with_its_real_content() {
        let repo = init_repo();
        fs::write(repo.path().join("notes.txt"), "line one\n").expect("write");

        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");
        fs::write(repo.path().join("notes.txt"), "line one\nline two\n").expect("write");

        let review =
            diff_against_tree(repo.path(), &tree, "since".to_string()).expect("diff_against_tree");
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

    /// The safety guarantee this whole approach rests on: snapshotting a worktree that a real
    /// agent might be working in must not perturb one single thing git reports about it. Checks
    /// real `git status --porcelain` output (staged, unstaged and untracked files all present)
    /// byte-for-byte across the call, plus `HEAD`, the index file's own bytes, and the stash.
    #[test]
    fn snapshotting_a_worktree_leaves_real_git_status_byte_identical() {
        let repo = init_repo();
        // A genuinely mixed state: one staged change, one unstaged change, one untracked file.
        fs::write(repo.path().join("staged.txt"), "staged content\n").expect("write");
        git(repo.path(), &["add", "staged.txt"]);
        fs::write(repo.path().join("file.txt"), "hello\nunstaged edit\n").expect("write");
        fs::write(repo.path().join("untracked.txt"), "untracked\n").expect("write");

        let status_before = git_stdout(repo.path(), &["status", "--porcelain"]);
        let head_before = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
        let index_before = fs::read(repo.path().join(".git").join("index")).expect("read index");
        let stash_before = git_stdout(repo.path(), &["stash", "list"]);
        assert!(
            status_before.contains("A  staged.txt")
                && status_before.contains(" M file.txt")
                && status_before.contains("?? untracked.txt"),
            "the test fixture must really have staged, unstaged and untracked entries - got:\n\
             {status_before}"
        );

        snapshot_worktree_tree(repo.path()).expect("snapshot");

        assert_eq!(
            git_stdout(repo.path(), &["status", "--porcelain"]),
            status_before,
            "snapshotting must not change one byte of what git reports about the worktree"
        );
        assert_eq!(git_stdout(repo.path(), &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            fs::read(repo.path().join(".git").join("index")).expect("read index"),
            index_before,
            "the real index file must be untouched - every mutation goes to the shadow copy"
        );
        assert_eq!(git_stdout(repo.path(), &["stash", "list"]), stash_before);
    }

    /// Two snapshots of the *same* content must produce the same tree id (git trees are content
    /// addressed), and a snapshot after a real change must produce a different one - the
    /// property "Mark reviewed" relies on to actually advance the baseline.
    #[test]
    fn a_snapshot_id_tracks_real_content_not_the_time_it_was_taken() {
        let repo = init_repo();
        let first = snapshot_worktree_tree(repo.path()).expect("snapshot");
        let unchanged = snapshot_worktree_tree(repo.path()).expect("snapshot");
        assert_eq!(
            first, unchanged,
            "an unchanged worktree must snapshot to the same tree"
        );

        fs::write(repo.path().join("file.txt"), "hello\nchanged\n").expect("write");
        let after = snapshot_worktree_tree(repo.path()).expect("snapshot");
        assert_ne!(
            first, after,
            "a real change must produce a genuinely different baseline"
        );
    }

    /// `changed_paths_against_tree` must agree exactly with the full diff's own file list - it
    /// exists purely as a cheaper route to the same answer, so any disagreement is a bug.
    #[test]
    fn changed_paths_agrees_with_the_full_review_diffs_file_list() {
        let repo = init_repo();
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");

        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        fs::write(repo.path().join("added.txt"), "new\n").expect("write");

        let mut paths = changed_paths_against_tree(repo.path(), &tree).expect("changed_paths");
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("added.txt"), PathBuf::from("file.txt")]
        );

        let mut full: Vec<PathBuf> = diff_against_tree(repo.path(), &tree, "since".to_string())
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
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");
        assert!(changed_paths_against_tree(repo.path(), &tree)
            .expect("changed_paths")
            .is_empty());
    }

    /// A real, anchored baseline must survive an aggressive `git gc` - the whole reason
    /// `anchor_tree` exists rather than trusting a bare tree id to stay reachable.
    #[test]
    fn an_anchored_baseline_survives_a_real_aggressive_gc() {
        let repo = init_repo();
        fs::write(repo.path().join("only_here.txt"), "unreferenced content\n").expect("write");
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");

        let ref_name = baseline_ref_name("worktree-a|Claude|1700000000");
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");

        git(repo.path(), &["gc", "--prune=now", "--aggressive"]);

        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name]).trim(),
            tree,
            "the anchored ref must still resolve to the same tree after a real gc"
        );
        // And the tree's real content is still readable - not just the ref surviving.
        assert!(git_stdout(repo.path(), &["cat-file", "-p", &tree]).contains("only_here.txt"));
    }

    #[test]
    fn anchoring_again_moves_an_existing_baseline_ref_rather_than_failing() {
        let repo = init_repo();
        let first = snapshot_worktree_tree(repo.path()).expect("snapshot");
        let ref_name = baseline_ref_name("key");
        anchor_tree(repo.path(), &ref_name, &first).expect("anchor");

        fs::write(repo.path().join("file.txt"), "hello\nmarked reviewed\n").expect("write");
        let second = snapshot_worktree_tree(repo.path()).expect("snapshot");
        anchor_tree(repo.path(), &ref_name, &second).expect("re-anchor");

        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name]).trim(),
            second,
            "'Mark reviewed' advances the same ref onto the new snapshot"
        );
    }

    #[test]
    fn deleting_a_baseline_ref_removes_it_and_is_idempotent() {
        let repo = init_repo();
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");
        let ref_name = baseline_ref_name("key");
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");

        delete_ref(repo.path(), &ref_name).expect("delete");
        assert!(
            git_stdout(repo.path(), &["rev-parse", "--verify", &ref_name])
                .trim()
                .is_empty(),
            "the ref must really be gone"
        );

        delete_ref(repo.path(), &ref_name)
            .expect("deleting an already-deleted baseline ref must not be an error");
    }

    /// The guard that matters most: a corrupted or hand-edited persisted state file must never
    /// be able to talk `delete_ref` into deleting a real branch.
    #[test]
    fn a_ref_outside_the_review_namespace_is_refused_and_the_branch_survives() {
        let repo = init_repo();
        let head_before = git_stdout(repo.path(), &["rev-parse", "refs/heads/main"]);

        assert!(delete_ref(repo.path(), "refs/heads/main").is_err());
        assert!(anchor_tree(repo.path(), "refs/heads/main", &"0".repeat(40)).is_err());
        // Nor a path that merely *looks* like it starts inside the namespace.
        assert!(delete_ref(repo.path(), "refs/jerry/review/../../heads/main").is_err());

        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", "refs/heads/main"]),
            head_before,
            "the real branch must be completely untouched"
        );
    }

    /// Ref names must be injective in the key: two agents whose keys differ only in characters a
    /// naive sanitizer would collapse (a space vs. an underscore, `/` vs. `-`) must never end up
    /// sharing one baseline ref.
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

    /// A real `git check-ref-format` run over a key stuffed with every character class git's own
    /// ref rules reject - the encoding must produce something git genuinely accepts.
    #[test]
    fn an_encoded_ref_name_is_accepted_by_real_git_check_ref_format() {
        let repo = init_repo();
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

        // And it really works as a ref, end to end.
        let tree = snapshot_worktree_tree(repo.path()).expect("snapshot");
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name]).trim(),
            tree
        );
    }

    #[test]
    fn a_tree_id_that_is_not_a_hex_object_id_is_refused_before_git_ever_runs() {
        let repo = init_repo();
        assert!(diff_against_tree(repo.path(), "--upload-pack=evil", "x".to_string()).is_err());
        assert!(changed_paths_against_tree(repo.path(), "").is_err());
        assert!(diff_against_tree(repo.path(), "HEAD", "x".to_string()).is_err());
    }

    /// A snapshot really is per-worktree: two worktrees of the same repository snapshot their own
    /// content, and diffing one against the other's baseline reports the real difference.
    #[test]
    fn snapshots_are_scoped_to_the_worktree_they_were_taken_in() {
        let repo = init_repo();
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

        let main_tree = snapshot_worktree_tree(repo.path()).expect("snapshot main");
        let linked_paths =
            changed_paths_against_tree(&other, &main_tree).expect("changed_paths in linked");
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
