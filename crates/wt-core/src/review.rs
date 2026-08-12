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

use sha2::{Digest, Sha256};

use crate::diff::{
    capture_git_stdout, compute_diff, prepare_shadow_index, validate_object_id, ShadowIndexContent,
    WorktreeDiff, MAX_DIFF_OUTPUT_BYTES,
};
use crate::error::Error;
use crate::{check_success, format_args, git_command, run_git};

/// Cap on the total bytes of **untracked** content a single snapshot will hash into the object
/// database before falling back to a tracked-only baseline ([`UntrackedCoverage::Excluded`]).
///
/// This cap is not hypothetical. `snapshot_worktree_tree` originally ran an unconditional
/// `git add -A`, and this project's own checkout at the time carried a 19 GB, 21,471-file
/// untracked build directory that was *not* gitignored. Every agent spawn - including the one
/// every window performs at startup - would have hashed and written all of it into the user's
/// real `.git/objects`, taking minutes and leaving the objects behind as loose files until a
/// `git gc --prune` (a two-week default grace period).
///
/// 128 MiB is chosen as comfortably above any plausible *legitimate* untracked set (scratch
/// files, a few generated artifacts) while bounding the pathological case to a few seconds of
/// hashing. Large build outputs are normally gitignored, and a gitignored file is never counted
/// here at all - a worktree tripping this cap has genuinely put hundreds of megabytes of
/// un-ignored content in front of git.
pub const MAX_UNTRACKED_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;

/// Companion cap to [`MAX_UNTRACKED_SNAPSHOT_BYTES`] on the *number* of untracked files.
///
/// Needed separately because per-file cost is not only bytes: each path costs a `stat`, an index
/// entry, and a loose object. 21,471 tiny files would slip under a pure byte cap while still
/// being exactly the case that motivated this. Also lets [`measure_untracked`] stop counting
/// early rather than stat-ing an unbounded list just to discover it is unbounded.
pub const MAX_UNTRACKED_SNAPSHOT_FILES: usize = 5_000;

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

/// Whether a baseline's snapshot actually captured the worktree's untracked files.
///
/// Not a detail: it changes what a review *means*, so it travels with the baseline and is shown
/// to the user rather than being silently assumed. It must also be passed back to
/// [`diff_against_tree`]/[`changed_paths_against_tree`], because measuring against a baseline
/// with one coverage using the other's rules produces a systematically wrong answer - a
/// tracked-only baseline compared with untracked files included would report every pre-existing
/// untracked file as something the agent had just added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackedCoverage {
    /// The normal case: untracked, non-ignored files were captured with their real content, so a
    /// file the agent creates afterwards shows up as a real addition.
    Included,
    /// The worktree's untracked set exceeded [`MAX_UNTRACKED_SNAPSHOT_BYTES`] or
    /// [`MAX_UNTRACKED_SNAPSHOT_FILES`], so the baseline covers tracked files only.
    ///
    /// Reviews against such a baseline are honest but narrower: they report changes to files git
    /// already tracks, and say nothing about untracked ones in either direction. Callers are
    /// expected to surface this to the user (this app's Review tab header appends `tracked files
    /// only`) rather than presenting it as a complete answer.
    Excluded,
}

impl UntrackedCoverage {
    /// The shadow-index flavour that matches this coverage - the single place the mapping lives,
    /// so a snapshot and every later diff against it can never disagree about whether untracked
    /// files are in scope.
    fn shadow_index_content(self, for_snapshot: bool) -> ShadowIndexContent {
        match (self, for_snapshot) {
            (UntrackedCoverage::Included, true) => ShadowIndexContent::FullContent,
            // A diff never needs real untracked *content* - `git diff <object>` reads the working
            // tree directly - so the cheap intent-to-add stub is enough to make the path visible.
            (UntrackedCoverage::Included, false) => ShadowIndexContent::IntentToAdd,
            (UntrackedCoverage::Excluded, _) => ShadowIndexContent::TrackedOnly,
        }
    }
}

/// A real captured baseline: the tree, and what it actually covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    /// The tree's hex object id.
    pub tree_id: String,
    pub untracked: UntrackedCoverage,
    /// How many untracked, non-ignored files were found at capture time - real and measured, and
    /// kept even when they were included, so a caller can explain *why* a baseline is
    /// tracked-only. Saturates at [`MAX_UNTRACKED_SNAPSHOT_FILES`] + 1 when counting stopped
    /// early (see [`measure_untracked`]).
    pub untracked_files: usize,
    /// Their total size in bytes, measured the same way. `0` when counting stopped on the file
    /// cap before sizes were summed.
    pub untracked_bytes: u64,
}

/// Measures the worktree's untracked, non-ignored set: how many files, and how many bytes.
///
/// `git ls-files -o --exclude-standard -z` is the real list git itself would stage under
/// `add -A`, so this measures exactly what a [`ShadowIndexContent::FullContent`] snapshot would
/// be asked to hash - gitignored content is excluded here for the same reason it is excluded
/// there.
///
/// Stops early once the file count passes [`MAX_UNTRACKED_SNAPSHOT_FILES`], returning
/// `(cap + 1, 0)`: past that point the answer is already "too many", and stat-ing the rest would
/// be doing the very unbounded work this exists to avoid. A truncated path list is treated the
/// same way, since a list too large to even read is by definition past the cap.
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
        // `symlink_metadata`, not `metadata`: a symlink is staged as the link itself, so its own
        // (tiny) size is what a snapshot would write - following it could both over-count wildly
        // and wander outside the worktree entirely.
        if let Ok(meta) = std::fs::symlink_metadata(worktree_path.join(&relative)) {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    Ok((files, bytes))
}

/// Snapshots the worktree at `worktree_path` exactly as it stands right now - tracked
/// modifications, staged changes, and (subject to the caps below) untracked files - as a real
/// git tree object.
///
/// ## Bounded, not unconditional
///
/// The untracked set is **measured first** ([`measure_untracked`]). Only if it fits within
/// [`MAX_UNTRACKED_SNAPSHOT_BYTES`] and [`MAX_UNTRACKED_SNAPSHOT_FILES`] is it staged with real
/// content; otherwise the snapshot falls back to tracked paths only and says so via
/// [`WorktreeSnapshot::untracked`]. See [`MAX_UNTRACKED_SNAPSHOT_BYTES`]'s own docs for the real
/// 19 GB worktree this exists for - an unconditional `git add -A` here writes every untracked,
/// non-ignored byte into the user's real object database, on every single agent spawn.
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
    // `git write-tree` succeeding with output that isn't a hex object id would mean something is
    // very wrong; refusing it here (rather than storing it as a baseline that every later
    // `diff_against_tree` would reject one at a time) keeps the failure at the point it happened.
    validate_object_id(&tree_id, "written tree id")?;
    Ok(WorktreeSnapshot {
        tree_id,
        untracked,
        untracked_files,
        untracked_bytes,
    })
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
/// baseline - this hashes the whole key.
///
/// ## Why a digest rather than raw hex
///
/// The first version hex-encoded the key directly. Hex is injective and always ref-name-legal,
/// but it **doubles the length**, and a loose ref is a real file whose name must fit in
/// `NAME_MAX` (255 bytes on Linux, and less once git appends its own `.lock` suffix). That capped
/// the worktree-path portion of a key at roughly 107 bytes - well inside the range of an ordinary
/// `~/Developer/<org>/<repo>/.worktrees/<branch>` layout - and past it `git update-ref` failed
/// with "File name too long", which surfaced only as a logged warning and a silently, permanently
/// disabled Review door.
///
/// A SHA-256 digest is a fixed 64 characters regardless of key length, so the full ref is always
/// 82 bytes. Collisions remain a non-issue - and these keys are local filesystem paths under the
/// user's own control, never attacker-chosen input, so collision *resistance* is not the property
/// being relied on here; injectivity in practice is.
///
/// The cost is an unreadable ref name. That is an acceptable trade for internal bookkeeping refs
/// no user is expected to read; the human-readable form of the same fact lives in this app's own
/// persisted review state, next to the tree id.
pub fn baseline_ref_name(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut encoded = String::with_capacity(REVIEW_REF_PREFIX.len() + digest.len() * 2);
    encoded.push_str(REVIEW_REF_PREFIX);
    for byte in digest {
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

    /// `snapshot_worktree_tree`, asserting the ordinary case (untracked content really captured)
    /// and handing back just the tree id - the shape most tests here want.
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

    /// An *untracked* file created after a snapshot is a real change the reviewer needs to see -
    /// and the case `git diff <object>` would silently miss without the shadow index.
    #[test]
    fn an_untracked_file_created_after_a_snapshot_is_reported_as_an_addition() {
        let repo = init_repo();
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

    /// A file that already existed *untracked* at snapshot time must be captured with its real
    /// content, so a later edit to it is a modification rather than reappearing as a whole-file
    /// addition. This is what `ShadowIndexContent::FullContent` buys over `--intent-to-add`.
    #[test]
    fn a_file_untracked_at_snapshot_time_is_captured_with_its_real_content() {
        let repo = init_repo();
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

    /// `changed_paths_against_tree` must agree exactly with the full diff's own file list - it
    /// exists purely as a cheaper route to the same answer, so any disagreement is a bug.
    #[test]
    fn changed_paths_agrees_with_the_full_review_diffs_file_list() {
        let repo = init_repo();
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
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        let tree = snapshot_tree(repo.path());
        assert!(
            changed_paths_against_tree(repo.path(), &tree, UntrackedCoverage::Included)
                .expect("changed_paths")
                .is_empty()
        );
    }

    /// Counts the loose objects under `.git/objects` - what a snapshot writes, and the thing an
    /// unbounded `git add -A` would blow up.
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
                // The two-hex-char fan-out directories; `info`/`pack` are not loose objects.
                name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(|shard| {
                std::fs::read_dir(shard.path())
                    .map(|entries| entries.flatten().count())
                    .unwrap_or(0)
            })
            .sum()
    }

    /// **The regression test for the 19 GB bug.** A worktree with an untracked set past
    /// [`MAX_UNTRACKED_SNAPSHOT_FILES`] must not hash that content into the object database at
    /// all - it must fall back to a tracked-only baseline and say so.
    ///
    /// Checks the real consequence (loose objects actually written), not just the returned flag:
    /// the flag being right while the content still landed in `.git/objects` would be exactly the
    /// bug, still present, with a label on it.
    #[test]
    fn an_oversized_untracked_set_is_never_hashed_into_the_object_database() {
        let repo = init_repo();
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

        // And the baseline is still genuinely usable for what it does cover.
        let paths = changed_paths_against_tree(repo.path(), &snapshot.tree_id, snapshot.untracked)
            .expect("changed_paths");
        assert!(
            paths.is_empty(),
            "nothing tracked has changed since the snapshot, and the untracked set must not leak \
             in as spurious additions either - got {paths:?}"
        );

        // A real edit to a *tracked* file is still caught, which is what makes the fallback worth
        // having rather than refusing outright.
        fs::write(repo.path().join("file.txt"), "hello\nedited\n").expect("write");
        let paths = changed_paths_against_tree(repo.path(), &snapshot.tree_id, snapshot.untracked)
            .expect("changed_paths");
        assert_eq!(paths, vec![PathBuf::from("file.txt")]);
    }

    /// The cap must not fire for an ordinary worktree - a fallback that triggered constantly
    /// would quietly degrade every review.
    #[test]
    fn an_ordinary_untracked_set_is_still_captured_in_full() {
        let repo = init_repo();
        fs::write(repo.path().join("scratch.txt"), "a normal untracked file\n").expect("write");

        let snapshot = snapshot_worktree_tree(repo.path()).expect("snapshot");
        assert_eq!(snapshot.untracked, UntrackedCoverage::Included);
        assert_eq!(snapshot.untracked_files, 1);
    }

    /// Gitignored content must not count toward the cap at all - `git add -A` would never have
    /// staged it, so counting it would push ordinary worktrees onto the fallback for content git
    /// was never going to touch.
    #[test]
    fn gitignored_content_does_not_count_toward_the_untracked_cap() {
        let repo = init_repo();
        fs::write(repo.path().join(".gitignore"), "ignored/\n").expect("write");
        let ignored = repo.path().join("ignored");
        std::fs::create_dir_all(&ignored).expect("mkdir");
        // A handful is enough: what this pins is that ignored files are counted as *zero*, not
        // that some threshold isn't reached - so it deliberately doesn't recreate the sibling
        // test's several-thousand-file fixture, which is real filesystem work this crate's own
        // parallel test suite has to share a machine with.
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

    /// A real, anchored baseline must survive an aggressive `git gc` - the whole reason
    /// `anchor_tree` exists rather than trusting a bare tree id to stay reachable.
    #[test]
    fn an_anchored_baseline_survives_a_real_aggressive_gc() {
        let repo = init_repo();
        fs::write(repo.path().join("only_here.txt"), "unreferenced content\n").expect("write");
        let tree = snapshot_tree(repo.path());

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
        let first = snapshot_tree(repo.path());
        let ref_name = baseline_ref_name("key");
        anchor_tree(repo.path(), &ref_name, &first).expect("anchor");

        fs::write(repo.path().join("file.txt"), "hello\nmarked reviewed\n").expect("write");
        let second = snapshot_tree(repo.path());
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
        let tree = snapshot_tree(repo.path());
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

    /// **The regression test for the `NAME_MAX` bug.** A realistically deep worktree path used to
    /// hex-encode into a ref filename longer than the 255-byte limit, so `git update-ref` failed
    /// with "File name too long" and the review surface was silently, permanently disabled.
    ///
    /// Drives a real `anchor_tree` against a real repository with a long key, rather than only
    /// asserting a length - the failure was in git, not in this crate's arithmetic.
    #[test]
    fn a_long_worktree_path_still_produces_a_usable_ref_name() {
        let repo = init_repo();
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
        // The real limit is on the final path *component*, which is what git turns into a file.
        let component = ref_name.rsplit('/').next().expect("a final ref component");
        assert!(
            component.len() + ".lock".len() <= 255,
            "the ref's own filename, plus git's `.lock` suffix, must fit in NAME_MAX"
        );

        // And it really works against real git, which is where the original bug actually showed.
        let tree = snapshot_tree(repo.path());
        anchor_tree(repo.path(), &ref_name, &tree).expect("a long key must still anchor");
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name]).trim(),
            tree
        );
        delete_ref(repo.path(), &ref_name).expect("and must still be deletable");
    }

    /// Key length must not affect ref length at all - the property that makes the `NAME_MAX`
    /// failure structurally impossible rather than merely unlikely.
    #[test]
    fn ref_name_length_is_independent_of_key_length() {
        let short = baseline_ref_name("a");
        let long = baseline_ref_name(&"x".repeat(4096));
        assert_eq!(short.len(), long.len());
        assert_ne!(short, long, "but distinct keys still produce distinct refs");
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
        let tree = snapshot_tree(repo.path());
        anchor_tree(repo.path(), &ref_name, &tree).expect("anchor");
        assert_eq!(
            git_stdout(repo.path(), &["rev-parse", &ref_name]).trim(),
            tree
        );
    }

    #[test]
    fn a_tree_id_that_is_not_a_hex_object_id_is_refused_before_git_ever_runs() {
        let repo = init_repo();
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
