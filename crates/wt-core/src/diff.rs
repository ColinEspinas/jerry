//! Read-only diff of a worktree's `HEAD`, uncommitted changes included, against the merge-base
//! with the default branch.
//!
//! The default branch is detected in order: `refs/remotes/origin/HEAD`, a local `main`, a local
//! `master`, then the main worktree's checked-out branch. Detection uses `gix`; the diff text
//! comes from `git` - see `docs/architecture/decisions.md` §5.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::{Error, GitExit};
use crate::{check_success, format_args, git_command, open_repo, run_git};

/// Cap on how many bytes of `git diff` stdout are buffered; beyond it the diff is truncated.
pub(crate) const MAX_DIFF_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

const MAX_FILES: usize = 300;

const MAX_HUNK_LINES_PER_FILE: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// The line's text, without its leading `+`/`-`/` ` marker or trailing newline.
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The hunk header line as `git diff` printed it, e.g. `@@ -1,3 +1,4 @@ fn foo() {`.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// The file's current path (or its last path before deletion).
    pub path: PathBuf,
    /// The file's path before the change, if different (only set for renames).
    pub old_path: Option<PathBuf>,
    pub status: FileChangeStatus,
    /// `true` if `git diff` reported this as binary; `hunks` is then always empty.
    pub is_binary: bool,
    pub hunks: Vec<DiffHunk>,
    /// `true` if this file's hunk lines were cut short by [`MAX_HUNK_LINES_PER_FILE`].
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeDiff {
    pub base_branch: String,
    pub base_commit: String,
    pub files: Vec<DiffFile>,
    /// `true` if output exceeded [`MAX_DIFF_OUTPUT_BYTES`] or [`MAX_FILES`], so content is missing.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffBase {
    Diff(WorktreeDiff),
    /// No usable base *branch*, so `uncommitted` holds a `git diff HEAD` instead. `branch` names
    /// the detected default branch when the worktree is simply already on it.
    NoBase {
        branch: Option<String>,
        uncommitted: WorktreeDiff,
    },
    /// Nothing to diff at all: `HEAD` is unborn, so there is no commit to compare against.
    NoBaseFound,
}

impl DiffBase {
    /// The diff content this outcome carries, `None` only for [`DiffBase::NoBaseFound`].
    ///
    /// Callers that want to show a diff regardless of whether a base branch was found should
    /// read through this rather than matching [`DiffBase::Diff`] alone.
    pub fn diff(&self) -> Option<&WorktreeDiff> {
        match self {
            DiffBase::Diff(diff)
            | DiffBase::NoBase {
                uncommitted: diff, ..
            } => Some(diff),
            DiffBase::NoBaseFound => None,
        }
    }
}

/// Resolved once and shared, so every scope a caller draws agrees about where `HEAD` and the base
/// are rather than each re-deriving them and disagreeing mid-refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseResolution {
    /// The worktree's own checked-out branch, `None` when `HEAD` is detached.
    pub(crate) worktree_branch: Option<String>,
    /// The worktree's `HEAD` commit, as a hex sha. Always a born commit.
    pub(crate) head_sha: String,
    /// The detected default branch, whether or not a merge-base with it exists.
    pub(crate) detected_base_branch: Option<String>,
    /// `Some((base branch, merge-base sha))` only when there is a usable base to diff against.
    pub(crate) base: Option<(String, String)>,
}

impl BaseResolution {
    /// The branch name a diff from this resolution is *labelled* with; nothing is diffed against it.
    fn label_branch(&self) -> String {
        self.detected_base_branch
            .clone()
            .unwrap_or_else(|| self.worktree_branch.clone().unwrap_or_default())
    }
}

/// Resolves `worktree_path`'s `HEAD` and its base. `Ok(None)` means `HEAD` is unborn.
pub(crate) fn resolve_base(worktree_path: &Path) -> Result<Option<BaseResolution>, Error> {
    let repo = open_repo(worktree_path)?;

    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let worktree_branch = head.referent_name().map(|name| name.shorten().to_string());
    let worktree_head_id = head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?;

    let Some(worktree_head_id) = worktree_head_id else {
        return Ok(None);
    };
    let head_sha = worktree_head_id.detach().to_string();

    let Some((base_branch, base_commit_id)) = detect_default_base(&repo)? else {
        return Ok(Some(BaseResolution {
            worktree_branch,
            head_sha,
            detected_base_branch: None,
            base: None,
        }));
    };

    if worktree_branch.as_deref() == Some(base_branch.as_str()) {
        return Ok(Some(BaseResolution {
            worktree_branch,
            head_sha,
            detected_base_branch: Some(base_branch),
            base: None,
        }));
    }

    let merge_base = match repo.merge_base(worktree_head_id.detach(), base_commit_id) {
        Ok(id) => Some(id.detach().to_string()),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => None,
        Err(source) => return Err(Error::MergeBase(Box::new(source))),
    };

    Ok(Some(BaseResolution {
        worktree_branch,
        head_sha,
        detected_base_branch: Some(base_branch.clone()),
        base: merge_base.map(|sha| (base_branch, sha)),
    }))
}

pub fn diff_against_base(worktree_path: &Path) -> Result<DiffBase, Error> {
    let Some(resolved) = resolve_base(worktree_path)? else {
        return Ok(DiffBase::NoBaseFound);
    };

    let Some((base_branch, merge_base_sha)) = resolved.base.clone() else {
        let uncommitted = compute_diff(
            worktree_path,
            &resolved.head_sha,
            ShadowIndexContent::IntentToAdd,
            resolved.label_branch(),
        )?;
        return Ok(DiffBase::NoBase {
            branch: resolved.detected_base_branch,
            uncommitted,
        });
    };

    let diff = compute_diff(
        worktree_path,
        &merge_base_sha,
        ShadowIndexContent::IntentToAdd,
        base_branch,
    )?;
    Ok(DiffBase::Diff(diff))
}

/// The working tree against its own `HEAD` - what is dirty in the checkout, untracked files
/// included.
///
/// A different question from [`diff_against_base`], not a filtered view of it: this compares
/// against `HEAD`, so work already committed on the branch does not appear.
///
/// `Ok(None)` means `HEAD` is unborn; "nothing changed" is `Ok(Some(_))` with empty `files`.
pub fn diff_against_head(worktree_path: &Path) -> Result<Option<WorktreeDiff>, Error> {
    let Some(resolved) = resolve_base(worktree_path)? else {
        return Ok(None);
    };
    let label = resolved
        .worktree_branch
        .clone()
        .unwrap_or_else(|| "HEAD".to_string());
    Ok(Some(compute_diff(
        worktree_path,
        &resolved.head_sha,
        ShadowIndexContent::IntentToAdd,
        label,
    )?))
}

/// Rejects an object id that is not non-empty ASCII hex, so it can never reach `git` as a flag
/// or as revision syntax.
///
/// Deliberately does not pin a length: the repository's hash algorithm is its own business.
pub(crate) fn validate_object_id(id: &str, what: &str) -> Result<(), Error> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::WorktreeIo(std::io::Error::other(format!(
            "{what} was not a hex object id"
        ))));
    }
    Ok(())
}

/// Runs `git diff <object_id>` against the working tree and folds the result into a
/// [`WorktreeDiff`]. `label_branch` is purely descriptive.
///
/// `object_id` may be a commit or a tree: `git diff <object>` resolves both against the working
/// tree the same way, which is what lets [`crate::review::diff_against_tree`] reuse this.
pub(crate) fn compute_diff(
    worktree_path: &Path,
    object_id: &str,
    shadow_content: ShadowIndexContent,
    label_branch: String,
) -> Result<WorktreeDiff, Error> {
    let commit_sha = object_id;
    validate_object_id(commit_sha, "commit id")?;

    let shadow_index = prepare_shadow_index(worktree_path, shadow_content)?;

    let args: Vec<OsString> = vec![
        // Pinned, not left to the caller's config: `diff.mnemonicPrefix` would turn the `a/`/`b/`
        // prefixes `strip_diff_prefix` parses into `i/`/`w/`/`c/`, and `core.quotePath` defaults
        // to octal-escaping non-ASCII filenames. `-c` must precede the subcommand.
        "-c".into(),
        "diff.mnemonicPrefix=false".into(),
        "-c".into(),
        "diff.noprefix=false".into(),
        "-c".into(),
        "core.quotePath=false".into(),
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "-M".into(),
        commit_sha.into(),
    ];
    let (output, output_truncated) = capture_git_stdout(
        worktree_path,
        &args,
        MAX_DIFF_OUTPUT_BYTES,
        Some(&shadow_index),
    )?;
    let text = String::from_utf8_lossy(&output);
    let (files, files_truncated) = parse_git_diff(&text);

    Ok(WorktreeDiff {
        base_branch: label_branch,
        base_commit: commit_sha.to_string(),
        files,
        truncated: output_truncated || files_truncated,
    })
}

const MAX_BRANCH_COMMITS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCommit {
    pub id: String,
    /// git's own `%h` abbreviation of [`Self::id`], never a hand-truncated prefix.
    pub short_id: String,
    pub subject: String,
    pub author_name: String,
    pub author_time_unix: i64,
    /// Lines this commit added, per its own `--numstat`.
    ///
    /// Zero for a merge commit and for binary files: git reports no line counts for either, so
    /// this is a genuine absence rather than a computed zero.
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCommits {
    /// The base branch the range was taken against; `None` leaves [`Self::commits`] empty rather
    /// than falling back to the whole of history.
    pub base_branch: Option<String>,
    pub base_commit: Option<String>,
    /// Newest first, exactly as `git log` lists them.
    pub commits: Vec<BranchCommit>,
    /// The *net* diffstat of the range, not the sum of [`Self::commits`]' own stats - a line
    /// added then later removed counts in the sum but not here.
    pub added: u32,
    pub removed: u32,
    pub truncated: bool,
}

/// Reads the commits on `worktree_path`'s branch since its merge-base, plus the range's net
/// diffstat. With no usable base this returns an empty range rather than all of history.
pub fn commits_since_base(worktree_path: &Path) -> Result<BranchCommits, Error> {
    let empty = BranchCommits {
        base_branch: None,
        base_commit: None,
        commits: Vec::new(),
        added: 0,
        removed: 0,
        truncated: false,
    };

    let Some(resolved) = resolve_base(worktree_path)? else {
        return Ok(empty);
    };
    let Some((base_branch, merge_base_sha)) = resolved.base else {
        return Ok(empty);
    };
    validate_object_id(&merge_base_sha, "merge-base id")?;
    validate_object_id(&resolved.head_sha, "commit id")?;

    let range = format!("{merge_base_sha}..{}", resolved.head_sha);

    // RS/US (`%x1e`/`%x1f`) delimit records and fields so a subject containing tabs or newlines
    // cannot be misparsed as a field boundary or as the start of a numstat line.
    let log_args: Vec<OsString> = vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "log".into(),
        "--no-color".into(),
        "--numstat".into(),
        format!("--max-count={}", MAX_BRANCH_COMMITS + 1).into(),
        "--format=%x1e%H%x1f%h%x1f%an%x1f%at%x1f%s".into(),
        range.clone().into(),
    ];
    let (log_output, log_truncated) =
        capture_git_stdout(worktree_path, &log_args, MAX_DIFF_OUTPUT_BYTES, None)?;
    let log_text = String::from_utf8_lossy(&log_output);
    let mut commits = parse_branch_commits(&log_text);
    let over_cap = commits.len() > MAX_BRANCH_COMMITS;
    commits.truncate(MAX_BRANCH_COMMITS);

    let numstat_args: Vec<OsString> = vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--numstat".into(),
        range.into(),
    ];
    let (numstat_output, numstat_truncated) =
        capture_git_stdout(worktree_path, &numstat_args, MAX_DIFF_OUTPUT_BYTES, None)?;
    let (added, removed) = sum_numstat(&String::from_utf8_lossy(&numstat_output));

    Ok(BranchCommits {
        base_branch: Some(base_branch),
        base_commit: Some(merge_base_sha),
        commits,
        added,
        removed,
        truncated: over_cap || log_truncated || numstat_truncated,
    })
}

fn parse_branch_commits(text: &str) -> Vec<BranchCommit> {
    let mut commits = Vec::new();
    // Skipping empty records, rather than `skip(1)`, keeps a real record if git ever stops
    // emitting the leading separator.
    for record in text.split('\u{1e}') {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let mut lines = record.lines();
        let Some(header) = lines.next() else {
            continue;
        };
        let mut fields = header.split('\u{1f}');
        let (Some(id), Some(short_id), Some(author_name), Some(author_time), Some(subject)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let (added, removed) = sum_numstat(&lines.collect::<Vec<_>>().join("\n"));
        commits.push(BranchCommit {
            id: id.to_string(),
            short_id: short_id.to_string(),
            subject: subject.to_string(),
            author_name: author_name.to_string(),
            // The timestamp only drives an age label, so an unparseable one loses less than
            // dropping the whole commit would.
            author_time_unix: author_time.parse().unwrap_or(0),
            added,
            removed,
        });
    }
    commits
}

/// Sums `git --numstat` lines (`<added>\t<removed>\t<path>`), saturating.
///
/// A binary file's `-\t-\t<path>` contributes nothing: git reports no line counts for one.
fn sum_numstat(text: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(add), Some(del)) = (fields.next(), fields.next()) else {
            continue;
        };
        if fields.next().is_none() {
            continue;
        }
        if let Ok(add) = add.parse::<u32>() {
            added = added.saturating_add(add);
        }
        if let Ok(del) = del.parse::<u32>() {
            removed = removed.saturating_add(del);
        }
    }
    (added, removed)
}

/// Merge-state of a worktree's `HEAD` against the detected default base branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeMergeStatus {
    /// The short name of the detected default branch this was checked against.
    pub base_branch: String,
    /// `true` if `HEAD` is an ancestor of (or equal to) the base branch's tip - what
    /// `git branch --merged <base>` reports.
    pub merged: bool,
    /// The `HEAD` commit's committer timestamp, in seconds since the Unix epoch. `None` means
    /// the commit could not be decoded; it only affects a display label, not `merged`.
    pub head_committer_unix_seconds: Option<i64>,
}

/// Whether `worktree_path`'s `HEAD` is already merged into the detected default branch.
/// `Ok(None)` if no base could be detected or `HEAD` is unborn.
pub fn merge_status_against_base(
    worktree_path: &Path,
) -> Result<Option<WorktreeMergeStatus>, Error> {
    let repo = open_repo(worktree_path)?;

    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let head_id = head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?;
    let Some(head_id) = head_id else {
        return Ok(None);
    };
    let head_id = head_id.detach();

    let Some((base_branch, base_commit_id)) = detect_default_base(&repo)? else {
        return Ok(None);
    };

    let merged = if head_id == base_commit_id {
        true
    } else {
        match repo.merge_base(head_id, base_commit_id) {
            Ok(merge_base_id) => merge_base_id.detach() == head_id,
            Err(gix::repository::merge_base::Error::NotFound { .. }) => false,
            Err(source) => return Err(Error::MergeBase(Box::new(source))),
        }
    };

    let head_committer_unix_seconds = repo
        .find_commit(head_id)
        .ok()
        .and_then(|commit| commit.time().ok())
        .map(|time| time.seconds);

    Ok(Some(WorktreeMergeStatus {
        base_branch,
        merged,
        head_committer_unix_seconds,
    }))
}

/// Ahead/behind commit counts for a worktree's `HEAD` against the detected default base branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AheadBehind {
    /// Commits reachable from `HEAD` but not from the base branch's tip.
    pub ahead: usize,
    /// Commits reachable from the base branch's tip but not from `HEAD`.
    pub behind: usize,
}

/// Ahead/behind counts for `worktree_path` against its detected default base branch. `Ok(None)`
/// if no base could be detected, `HEAD` is unborn, or the histories are unrelated; `{0, 0}` when
/// the worktree is already on the base branch.
pub fn ahead_behind_against_base(worktree_path: &Path) -> Result<Option<AheadBehind>, Error> {
    let repo = open_repo(worktree_path)?;

    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let worktree_branch = head.referent_name().map(|name| name.shorten().to_string());
    let worktree_head_id = head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?;
    let Some(worktree_head_id) = worktree_head_id else {
        return Ok(None);
    };

    let Some((base_branch, base_commit_id)) = detect_default_base(&repo)? else {
        return Ok(None);
    };

    if worktree_branch.as_deref() == Some(base_branch.as_str()) {
        return Ok(Some(AheadBehind::default()));
    }

    match repo.merge_base(worktree_head_id.detach(), base_commit_id) {
        Ok(_) => {}
        Err(gix::repository::merge_base::Error::NotFound { .. }) => return Ok(None),
        Err(source) => return Err(Error::MergeBase(Box::new(source))),
    }

    let base_commit_sha = base_commit_id.to_string();
    if base_commit_sha.is_empty() || !base_commit_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::WorktreeIo(std::io::Error::other(
            "detected base commit id was not a hex object id",
        )));
    }

    // The sha, never `base_branch`'s short name: when the base came from
    // `refs/remotes/origin/HEAD`, a bare `main...HEAD` resolves to a local `refs/heads/main`
    // first, which may be stale and would under-report how far behind the worktree is.
    // `...` computes the merge-base itself and reports `<behind>\t<ahead>`.
    let args: Vec<OsString> = vec![
        "rev-list".into(),
        "--left-right".into(),
        "--count".into(),
        format!("{base_commit_sha}...HEAD").into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ahead_behind_counts(&text))
}

/// Parses `git rev-list --left-right --count`'s `<behind> <ahead>` stdout.
///
/// `None`, rather than `{0, 0}`, when a field is missing or unparseable: a confident but wrong
/// "up to date" is worse than an omitted value.
fn parse_ahead_behind_counts(text: &str) -> Option<AheadBehind> {
    let mut parts = text.split_whitespace();
    let behind = parts.next().and_then(|part| part.parse::<usize>().ok())?;
    let ahead = parts.next().and_then(|part| part.parse::<usize>().ok())?;
    Some(AheadBehind { ahead, behind })
}

/// Detects the repository's default branch and its tip, per the module docs' order.
/// `Ok(None)` rather than `Err` when none is detectable: that is an expected outcome.
pub(crate) fn detect_default_base(
    repo: &gix::Repository,
) -> Result<Option<(String, gix::ObjectId)>, Error> {
    if let Ok(mut origin_head) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let gix::refs::TargetRef::Symbolic(name) = origin_head.target() {
            let full = name.as_bstr().to_string();
            if let Some(short) = full.strip_prefix("refs/remotes/origin/") {
                let short = short.to_string();
                if let Ok(id) = origin_head.peel_to_id_in_place() {
                    return Ok(Some((short, id.detach())));
                }
            }
        }
    }

    for candidate in ["main", "master"] {
        let full_name = format!("refs/heads/{candidate}");
        if let Ok(mut reference) = repo.find_reference(full_name.as_str()) {
            if let Ok(id) = reference.peel_to_id_in_place() {
                return Ok(Some((candidate.to_string(), id.detach())));
            }
        }
    }

    // Last resort: the main worktree's checked-out branch. A failure here (corrupt common dir)
    // is treated as "nothing found", per this function's `Ok(None)` contract.
    let Ok(main_repo) = repo.main_repo() else {
        return Ok(None);
    };
    let mut main_head = main_repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let Some(name) = main_head.referent_name() else {
        return Ok(None);
    };
    let short = name.shorten().to_string();
    let id = main_head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?;
    Ok(id.map(|id| (short, id.detach())))
}

/// Cap on how many bytes of a spawned child's stderr are buffered.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Runs `git` in `dir`, capturing up to `max_bytes` of stdout; `index_override` sets
/// `GIT_INDEX_FILE` (see [`prepare_shadow_index`]).
///
/// Past `max_bytes` the child is killed rather than waited on, and the returned flag is `true`.
pub(crate) fn capture_git_stdout(
    dir: &Path,
    args: &[OsString],
    max_bytes: usize,
    index_override: Option<&Path>,
) -> Result<(Vec<u8>, bool), Error> {
    let mut command = git_command(dir, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(index_path) = index_override {
        command.env("GIT_INDEX_FILE", index_path);
    }
    let mut child = command.spawn().map_err(|source| Error::GitSpawn {
        args: format_args(args),
        source,
    })?;

    let (buf, truncated, stderr_text) = match read_streams_concurrently(&mut child, max_bytes) {
        Ok(v) => v,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };

    if truncated {
        let _ = child.kill();
        let _ = child.wait();
        return Ok((buf, true));
    }

    let status = child.wait().map_err(|source| Error::GitSpawn {
        args: format_args(args),
        source,
    })?;
    if !status.success() {
        return Err(Error::GitCommand {
            args: format_args(args),
            exit: GitExit::from_status(&status),
            stderr: stderr_text,
        });
    }
    Ok((buf, false))
}

/// Drains `child`'s stdout and stderr concurrently, capped at `max_stdout_bytes` and
/// [`MAX_STDERR_BYTES`]. Returns `(stdout, stdout_truncated, stderr_text)` without waiting on
/// `child`; the caller must do that, killing it first if truncated.
///
/// Concurrently, because reading stdout to EOF first deadlocks: pipe buffers are bounded, so a
/// child that fills stderr (one warning line per changed file, say) blocks writing it while this
/// thread blocks reading stdout.
fn read_streams_concurrently(
    child: &mut std::process::Child,
    max_stdout_bytes: usize,
) -> Result<(Vec<u8>, bool, String), Error> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::WorktreeIo(std::io::Error::other("child stdout was not piped")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::WorktreeIo(std::io::Error::other("child stderr was not piped")))?;

    let stderr_handle = std::thread::Builder::new()
        .spawn(move || {
            let mut stderr = stderr;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let take = n.min(MAX_STDERR_BYTES.saturating_sub(buf.len()));
                        buf.extend_from_slice(&chunk[..take]);
                        if buf.len() >= MAX_STDERR_BYTES {
                            // Keep draining past the cap, without accumulating, so the child
                            // can never block on a full stderr pipe.
                            while matches!(stderr.read(&mut chunk), Ok(n) if n > 0) {}
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
        .map_err(Error::WorktreeIo)?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max_stdout_bytes {
                    let remaining = max_stdout_bytes.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = stderr_handle.join();
                return Err(Error::WorktreeIo(err));
            }
        }
    }
    drop(stdout);

    let stderr_text = stderr_handle.join().unwrap_or_default();
    Ok((buf, truncated, stderr_text))
}

/// Which flavour of `git add` [`prepare_shadow_index`] runs into the throwaway index copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowIndexContent {
    /// `git add --intent-to-add -A` - enough for `git diff <object>` to see an untracked path,
    /// without writing any blob.
    IntentToAdd,
    /// `git add -A` - stages content, so `git write-tree` can name every blob.
    ///
    /// Unbounded in the size of the untracked set; [`crate::review::snapshot_worktree_tree`]
    /// measures that set first (see [`crate::review::MAX_UNTRACKED_SNAPSHOT_BYTES`]).
    FullContent,
    /// `git add -u` - tracked paths only, so the work is bounded by what is already committed.
    ///
    /// `git write-tree` refuses an index holding intent-to-add entries and this flavour does not
    /// materialize them, so a worktree with a user's own `git add -N` fails to snapshot here.
    /// That surfaces as an ordinary error, not a wrong answer.
    TrackedOnly,
}

/// Filename prefix marking a shadow index as this app's, should a killed process leave one behind.
const SHADOW_INDEX_PREFIX: &str = ".jerry-shadow-index-";

/// Creates the throwaway index file, beside `real_index_path` in the worktree's git directory.
///
/// Falls back to the OS temp directory when that directory cannot host it (a read-only mount
/// being the real case). If both refuse, the git directory's error is the one that propagates.
fn shadow_index_file(real_index_path: &Path) -> Result<tempfile::NamedTempFile, Error> {
    let builder = || {
        tempfile::Builder::new()
            .prefix(SHADOW_INDEX_PREFIX)
            .tempfile()
    };
    let Some(git_dir) = real_index_path.parent() else {
        return builder().map_err(Error::WorktreeIo);
    };
    match tempfile::Builder::new()
        .prefix(SHADOW_INDEX_PREFIX)
        .tempfile_in(git_dir)
    {
        Ok(file) => Ok(file),
        Err(git_dir_err) => builder().map_err(|_| Error::WorktreeIo(git_dir_err)),
    }
}

/// Builds a throwaway copy of the worktree's index with untracked files added, so
/// `git diff <object>` - which only considers paths already in the index or the target tree -
/// reports new files as additions rather than omitting them.
///
/// The real index is only read; every mutation goes to the copy through the caller's
/// `GIT_INDEX_FILE` override, so repository state is never perturbed.
///
/// The copy lives beside the real index rather than in `std::env::temp_dir()`: `git add` writes
/// an index by renaming a `.lock` file over it, so it needs a directory it can write to on the
/// repository's own filesystem. A `TMPDIR` on another mount fails that with
/// `fatal: unable to write new index file`.
pub(crate) fn prepare_shadow_index(
    worktree_path: &Path,
    content: ShadowIndexContent,
) -> Result<tempfile::TempPath, Error> {
    let index_path_args: Vec<OsString> =
        vec!["rev-parse".into(), "--git-path".into(), "index".into()];
    let output = run_git(worktree_path, &index_path_args)?;
    check_success(&index_path_args, &output)?;
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let real_index_path = {
        let candidate = PathBuf::from(&printed);
        if candidate.is_absolute() {
            candidate
        } else {
            worktree_path.join(candidate)
        }
    };

    let shadow = shadow_index_file(&real_index_path)?;
    // Decided here, acted on only after the handle is closed below.
    let real_index_missing;
    match std::fs::read(&real_index_path) {
        Ok(real_index_bytes) => {
            real_index_missing = false;
            use std::io::Write as _;
            (&shadow)
                .write_all(&real_index_bytes)
                .map_err(Error::WorktreeIo)?;
            (&shadow).flush().map_err(Error::WorktreeIo)?;
            // The mtime matters as much as the bytes: git's racy-index rule only distrusts an
            // entry whose cached mtime is not strictly older than the index file's own. A fresh
            // temp file carries *now*, which moves every entry out of that suspect window and
            // makes a same-length same-second edit vanish from the diff. Best-effort - a
            // filesystem that refuses the update leaves the copy no worse off.
            if let Ok(mtime) = std::fs::metadata(&real_index_path).and_then(|meta| meta.modified())
            {
                let _ = shadow
                    .as_file()
                    .set_times(std::fs::FileTimes::new().set_modified(mtime));
            }
        }
        Err(_) => {
            // No bytes to seed the copy with (an index never written, or one an agent's own
            // `git add` is rewriting right now). The empty placeholder `shadow_index_file`
            // created must be deleted rather than left: git rejects a 0-byte `GIT_INDEX_FILE`
            // with `index file smaller than expected`, but treats a missing one as empty.
            real_index_missing = true;
        }
    }

    // Closing the handle before `git add` runs is load-bearing, not tidiness: on Windows,
    // renaming the `.lock` over a destination that still has any open handle fails outright
    // (fixed upstream only in git 2.48), so holding the `NamedTempFile` across the child would
    // be a deterministic `fatal: unable to write new index file`. `TempPath` keeps the same
    // drop-based cleanup without the handle.
    let shadow = shadow.into_temp_path();
    if real_index_missing {
        // Safe only now the handle is closed: otherwise Windows leaves the path in a
        // delete-pending state that `git add` could not recreate.
        std::fs::remove_file(&shadow).map_err(Error::WorktreeIo)?;
    }

    let mut add_args: Vec<OsString> = vec!["add".into()];
    match content {
        ShadowIndexContent::IntentToAdd => {
            add_args.push("--intent-to-add".into());
            add_args.push("-A".into());
        }
        ShadowIndexContent::FullContent => add_args.push("-A".into()),
        ShadowIndexContent::TrackedOnly => add_args.push("-u".into()),
    }
    add_args.extend([OsString::from("--"), ".".into()]);

    // Defence-in-depth against something else on the machine (antivirus, a sync client) holding
    // the path open for a moment. Scoped to that one failure text so a broken repository or a
    // permissions error still fails on the first attempt.
    retry_transient_index_write_failure(|| {
        let output = git_command(worktree_path, &add_args)
            .env("GIT_INDEX_FILE", &shadow)
            .output()
            .map_err(|source| Error::GitSpawn {
                args: format_args(&add_args),
                source,
            })?;
        check_success(&add_args, &output)
    })?;

    Ok(shadow)
}

/// Total attempts, including the first, before the error is returned.
const MAX_INDEX_WRITE_ATTEMPTS: u32 = 3;

/// Retries `attempt_git` up to [`MAX_INDEX_WRITE_ATTEMPTS`] times with a growing backoff, but
/// only for [`is_transient_index_write_failure`]. Anything else returns on the first attempt.
///
/// Takes a closure so the policy stays testable without reproducing the Windows-only rename race
/// it exists for.
fn retry_transient_index_write_failure(
    mut attempt_git: impl FnMut() -> Result<(), Error>,
) -> Result<(), Error> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match attempt_git() {
            Ok(()) => return Ok(()),
            Err(Error::GitCommand { ref stderr, .. })
                if attempt < MAX_INDEX_WRITE_ATTEMPTS
                    && is_transient_index_write_failure(stderr) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50 * u64::from(attempt)));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Whether `stderr` names the one retryable `git add` failure. Matched by lowercased substring:
/// git's wording has varied by case across versions.
fn is_transient_index_write_failure(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("unable to write new index file")
}

/// Strips a leading `a/`/`b/` diff prefix and treats `/dev/null` as "no file". The prefixes are
/// exactly these by construction: [`compute_diff`] pins the git config that decides them.
fn strip_diff_prefix(path: &str) -> Option<PathBuf> {
    if path == "/dev/null" {
        return None;
    }
    let stripped = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/"));
    Some(PathBuf::from(stripped.unwrap_or(path)))
}

/// Strips the surrounding quotes git puts around a path, but not the backslash escapes inside
/// them: a path containing a quote or control character keeps its escaped form rather than being
/// rendered as an invalid path.
fn unquote_path(raw: &str) -> &str {
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

#[derive(Default)]
struct FileBuilder {
    header_path: Option<PathBuf>,
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
    added: bool,
    deleted: bool,
    renamed: bool,
    is_binary: bool,
    hunks: Vec<DiffHunk>,
    truncated: bool,
    hunk_line_budget: usize,
}

impl FileBuilder {
    fn new() -> Self {
        Self {
            hunk_line_budget: MAX_HUNK_LINES_PER_FILE,
            ..Default::default()
        }
    }

    fn finish(self) -> Option<DiffFile> {
        let path = self
            .new_path
            .or(self.old_path.clone())
            .or(self.header_path)?;
        let status = if self.renamed {
            FileChangeStatus::Renamed
        } else if self.added {
            FileChangeStatus::Added
        } else if self.deleted {
            FileChangeStatus::Deleted
        } else {
            FileChangeStatus::Modified
        };
        let old_path = if self.renamed {
            self.old_path.filter(|old| old != &path)
        } else {
            None
        };
        Some(DiffFile {
            path,
            old_path,
            status,
            is_binary: self.is_binary,
            hunks: self.hunks,
            truncated: self.truncated,
        })
    }
}

/// Parses `git diff`'s unified-diff stdout into files/hunks/lines, plus whether [`MAX_FILES`]
/// cut off any trailing files.
///
/// Header prefixes (`--- `, `+++ `, `@@ `, `rename from `, ...) are only ever matched *outside* a
/// hunk body. Body lines are unescaped file content behind a single marker char, so a removed
/// line reading `-- a comment` is textually identical to a `--- <path>` header; matching prefixes
/// unconditionally misfiles the change under a bogus path. The end of a body is therefore taken
/// from the hunk header's own declared line counts ([`parse_hunk_counts`]), counted down here.
fn parse_git_diff(text: &str) -> (Vec<DiffFile>, bool) {
    let mut files = Vec::new();
    let mut files_truncated = false;
    let mut current: Option<FileBuilder> = None;
    let mut in_hunk = false;
    let mut hunk_old_remaining = 0usize;
    let mut hunk_new_remaining = 0usize;

    let flush = |current: &mut Option<FileBuilder>, files: &mut Vec<DiffFile>| {
        if let Some(builder) = current.take() {
            if let Some(file) = builder.finish() {
                files.push(file);
            }
        }
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut current, &mut files);
            if files.len() >= MAX_FILES {
                files_truncated = true;
                current = None;
                in_hunk = false;
                continue;
            }
            in_hunk = false;
            let mut builder = FileBuilder::new();
            builder.header_path = parse_diff_git_header(rest);
            current = Some(builder);
            continue;
        }

        let Some(builder) = current.as_mut() else {
            continue;
        };
        if files.len() > MAX_FILES {
            continue;
        }

        if in_hunk {
            let kind = match line.chars().next() {
                Some('+') => Some(DiffLineKind::Added),
                Some('-') => Some(DiffLineKind::Removed),
                Some(' ') => Some(DiffLineKind::Context),
                _ => None,
            };
            if let Some(kind) = kind {
                match kind {
                    DiffLineKind::Added => {
                        hunk_new_remaining = hunk_new_remaining.saturating_sub(1);
                    }
                    DiffLineKind::Removed => {
                        hunk_old_remaining = hunk_old_remaining.saturating_sub(1);
                    }
                    DiffLineKind::Context => {
                        hunk_old_remaining = hunk_old_remaining.saturating_sub(1);
                        hunk_new_remaining = hunk_new_remaining.saturating_sub(1);
                    }
                }
                if builder.hunk_line_budget > 0 {
                    if let Some(hunk) = builder.hunks.last_mut() {
                        hunk.lines.push(DiffLine {
                            kind,
                            content: line[1..].to_string(),
                        });
                    }
                    builder.hunk_line_budget -= 1;
                } else {
                    builder.truncated = true;
                }
            }
            // `\ No newline at end of file` and anything else unrecognized is skipped without
            // counting against either budget.
            if hunk_old_remaining == 0 && hunk_new_remaining == 0 {
                in_hunk = false;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("rename from ") {
            builder.old_path = strip_diff_prefix_or_raw(rest);
            builder.renamed = true;
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            builder.new_path = strip_diff_prefix_or_raw(rest);
            builder.renamed = true;
        } else if line.starts_with("new file mode") {
            builder.added = true;
        } else if line.starts_with("deleted file mode") {
            builder.deleted = true;
        } else if let Some(rest) = line.strip_prefix("Binary files ") {
            builder.is_binary = true;
            if let Some(differ) = rest.strip_suffix(" differ") {
                if let Some((old, new)) = differ.split_once(" and ") {
                    builder.old_path = strip_diff_prefix(unquote_path(old));
                    builder.new_path = strip_diff_prefix(unquote_path(new));
                }
            }
        } else if let Some(rest) = line.strip_prefix("--- ") {
            builder.old_path = strip_diff_prefix(unquote_path(rest));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            builder.new_path = strip_diff_prefix(unquote_path(rest));
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let (old_count, new_count) = parse_hunk_counts(header).unwrap_or((0, 0));
            hunk_old_remaining = old_count;
            hunk_new_remaining = new_count;
            builder.hunks.push(DiffHunk {
                header: format!("@@ {header}"),
                lines: Vec::new(),
            });
            // A degenerate zero-line hunk would otherwise leave us in body mode forever.
            in_hunk = !(old_count == 0 && new_count == 0);
        }
    }
    flush(&mut current, &mut files);

    (files, files_truncated)
}

/// Parses a hunk header, minus its leading `"@@ "`, into the `(old, new)` body-line counts it
/// declares. A range with no explicit `,<count>` means one line.
fn parse_hunk_counts(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;
    let old_count = parse_range_count(old_part)?;
    let new_count = parse_range_count(new_part)?;
    Some((old_count, new_count))
}

fn parse_range_count(range: &str) -> Option<usize> {
    match range.split_once(',') {
        Some((_start, count)) => count.parse().ok(),
        None => range.parse::<usize>().ok().map(|_start| 1),
    }
}

fn strip_diff_prefix_or_raw(raw: &str) -> Option<PathBuf> {
    strip_diff_prefix(unquote_path(raw))
}

/// Fallback path extraction from a `diff --git a/<path> b/<path>` line, for a file with no hunk,
/// rename or binary marker to take one from (a pure mode change).
///
/// Splitting on the last `" b/"` is correct whenever both sides match, which holds here because a
/// rename emits `rename from`/`rename to` instead. Ambiguous only if they differ *and* the path
/// itself contains `" b/"`.
fn parse_diff_git_header(rest: &str) -> Option<PathBuf> {
    let idx = rest.rfind(" b/")?;
    let b_path = &rest[idx + 3..];
    strip_diff_prefix(&format!("b/{b_path}"))
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
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "initial commit"]);
        dir
    }

    #[test]
    fn on_default_branch_with_nothing_uncommitted_yields_an_empty_diff() {
        let repo = init_repo();
        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::NoBase {
            branch,
            uncommitted,
        } = result
        else {
            panic!("expected DiffBase::NoBase, got {result:?}");
        };
        assert_eq!(branch, Some("main".to_string()));
        assert!(
            uncommitted.files.is_empty(),
            "a clean worktree really has nothing uncommitted to show"
        );
    }

    #[test]
    fn on_default_branch_with_real_uncommitted_changes_shows_them() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited on main\n").expect("write");
        fs::write(repo.path().join("new.txt"), "brand new\n").expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let via_diff_accessor = result.diff().cloned();

        let DiffBase::NoBase {
            branch,
            uncommitted,
        } = result
        else {
            panic!("expected DiffBase::NoBase, got {result:?}");
        };
        assert_eq!(branch, Some("main".to_string()));
        let paths: Vec<&Path> = uncommitted.files.iter().map(|f| f.path.as_path()).collect();
        assert!(paths.contains(&Path::new("file.txt")));
        assert!(paths.contains(&Path::new("new.txt")));
        assert_eq!(via_diff_accessor, Some(uncommitted));
    }

    #[test]
    fn a_same_length_edit_racy_against_the_index_timestamp_is_still_reported() {
        use std::time::{Duration, SystemTime};

        let racy = SystemTime::UNIX_EPOCH
            + Duration::from_secs(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("clock after epoch")
                    .as_secs()
                    - 60,
            );
        let pin_mtime = |path: &Path| {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open for set_times");
            file.set_times(fs::FileTimes::new().set_modified(racy))
                .expect("set_times");
        };

        let repo = TempDir::new().expect("tempdir");
        let path = repo.path();
        git(path, &["init", "-b", "main"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test User"]);
        fs::write(path.join("a.rs"), "fn a() -> i32 {\n    1\n}\n").expect("write a.rs");
        pin_mtime(&path.join("a.rs"));
        git(path, &["add", "."]);
        git(path, &["commit", "-m", "initial"]);
        git(path, &["checkout", "-b", "feature"]);

        // The edit: byte-for-byte the same length as the committed content, and pinned back to
        // the same second the index entry already records.
        fs::write(path.join("a.rs"), "fn a() -> i32 {\n    2\n}\n").expect("rewrite a.rs");
        pin_mtime(&path.join("a.rs"));
        let index = fs::OpenOptions::new()
            .write(true)
            .open(path.join(".git/index"))
            .expect("open index");
        index
            .set_times(fs::FileTimes::new().set_modified(racy))
            .expect("set index times");

        let result = diff_against_base(path).expect("diff_against_base");
        let paths: Vec<PathBuf> = result
            .diff()
            .map(|diff| diff.files.iter().map(|file| file.path.clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            paths,
            vec![PathBuf::from("a.rs")],
            "a same-length edit whose mtime is racy against the index must still be diffed - \
             the shadow index has to preserve the real index's own mtime so git's racy-index \
             protection still applies to it"
        );
    }

    #[test]
    fn unborn_head_yields_no_base_found() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        let result = diff_against_base(dir.path()).expect("diff_against_base");
        assert_eq!(result, DiffBase::NoBaseFound);
    }

    #[test]
    fn feature_branch_diffs_modified_added_and_deleted_files() {
        let repo = init_repo();
        fs::write(repo.path().join("keep.txt"), "keep\n").expect("write");
        git(repo.path(), &["add", "keep.txt"]);
        git(repo.path(), &["commit", "-m", "add keep.txt"]);

        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("file.txt"), "hello\nworld\n").expect("write");
        fs::write(repo.path().join("new.txt"), "new file\n").expect("write");
        git(repo.path(), &["add", "new.txt"]);
        fs::remove_file(repo.path().join("keep.txt")).expect("remove");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert_eq!(diff.base_branch, "main");
        assert!(!diff.truncated);

        let modified = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .expect("file.txt should be in the diff");
        assert_eq!(modified.status, FileChangeStatus::Modified);
        assert!(!modified.hunks.is_empty());
        assert!(modified
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == DiffLineKind::Added && l.content == "world"));

        let added = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("new.txt"))
            .expect("new.txt should be in the diff");
        assert_eq!(added.status, FileChangeStatus::Added);
        assert!(added
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == DiffLineKind::Added && l.content == "new file"));

        let deleted = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("keep.txt"))
            .expect("keep.txt should be in the diff");
        assert_eq!(deleted.status, FileChangeStatus::Deleted);
    }

    #[test]
    fn committed_changes_since_branch_point_are_included() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("file.txt"), "hello\ncommitted change\n").expect("write");
        git(repo.path(), &["commit", "-am", "a real commit on feature"]);

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        let modified = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .expect("file.txt should be in the diff");
        assert!(modified
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == DiffLineKind::Added && l.content == "committed change"));
    }

    #[test]
    fn clean_feature_branch_has_no_changes() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert!(diff.files.is_empty());
    }

    #[test]
    fn rename_is_detected() {
        let repo = init_repo();
        let content = "line\n".repeat(50);
        fs::write(repo.path().join("file.txt"), &content).expect("write");
        git(repo.path(), &["commit", "-am", "pad file.txt"]);

        git(repo.path(), &["checkout", "-b", "feature"]);
        git(repo.path(), &["mv", "file.txt", "renamed.txt"]);
        git(repo.path(), &["commit", "-m", "rename file.txt"]);

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        let renamed = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("renamed.txt"))
            .expect("renamed.txt should be in the diff");
        assert_eq!(renamed.status, FileChangeStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some(Path::new("file.txt")));
    }

    #[test]
    fn binary_file_is_flagged_without_hunks() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("data.bin"), [0u8, 159, 146, 0, 1, 2]).expect("write");
        git(repo.path(), &["add", "data.bin"]);
        git(repo.path(), &["commit", "-m", "add binary file"]);

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        let binary = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("data.bin"))
            .expect("data.bin should be in the diff");
        assert!(binary.is_binary);
        assert!(binary.hunks.is_empty());
    }

    #[test]
    fn detached_head_still_diffs_against_default_branch() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "hello\nchanged\n").expect("write");
        git(repo.path(), &["commit", "-am", "a change"]);
        let head_sha = {
            let output = Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(repo.path(), &["checkout", "--detach", &head_sha]);

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        assert!(matches!(result, DiffBase::Diff(_)));
    }

    #[test]
    fn deleted_sql_style_comment_line_is_not_misparsed_as_file_header() {
        // A removed line starting `-- ` renders as `--- <text>`, identical to a `--- <path>`
        // header line.
        let repo = init_repo();
        let content = "line one\n-- a real sql comment\nline three\nline four\n";
        fs::write(repo.path().join("file.txt"), content).expect("write");
        git(
            repo.path(),
            &["commit", "-am", "add sql-style comment line"],
        );

        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(
            repo.path().join("file.txt"),
            "line one\nline three\nline four\n",
        )
        .expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        let modified = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .expect("file.txt should be in the diff, under its real name");
        let lines: Vec<&DiffLine> = modified.hunks.iter().flat_map(|h| &h.lines).collect();
        assert!(
            lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Removed && l.content == "-- a real sql comment"),
            "the comment line should be parsed as a real removed line, not dropped as a \
             fake file header: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Removed && l.content == "line three"),
            "line three is unchanged and shouldn't appear as removed at all: {lines:?}"
        );
        assert!(!modified.truncated);
    }

    #[test]
    fn added_line_looking_like_a_file_header_does_not_misattribute_the_file() {
        // An added line starting `++ ` renders as `+++ <text>`, identical to a `+++ <path>`
        // header line - which would misattribute the change to a path parsed out of content.
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(
            repo.path().join("file.txt"),
            "hello\n++ b/evil.txt looks like a header but is not\n",
        )
        .expect("write");
        git(
            repo.path(),
            &["commit", "-am", "add a line that looks like a diff header"],
        );

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert_eq!(
            diff.files.len(),
            1,
            "the line's content must not spawn a second, bogus file entry: {:?}",
            diff.files
        );
        let file = &diff.files[0];
        assert_eq!(file.path, Path::new("file.txt"));
        assert!(diff.files.iter().all(|f| f.path != Path::new("evil.txt")));
        let lines: Vec<&DiffLine> = file.hunks.iter().flat_map(|h| &h.lines).collect();
        assert!(
            lines.iter().any(|l| l.kind == DiffLineKind::Added
                && l.content == "++ b/evil.txt looks like a header but is not"),
            "the line should be parsed as real added content: {lines:?}"
        );
    }

    #[test]
    fn detects_default_branch_via_origin_head() {
        // Deliberately named neither `main` nor `master`, so this can only pass by following
        // `refs/remotes/origin/HEAD` rather than the local-branch fallbacks.
        let origin = TempDir::new().expect("tempdir");
        git(origin.path(), &["init", "--bare", "-b", "trunk"]);

        let seed = TempDir::new().expect("tempdir");
        git(seed.path(), &["init", "-b", "trunk"]);
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        fs::write(seed.path().join("file.txt"), "hello\n").expect("write");
        git(seed.path(), &["add", "file.txt"]);
        git(seed.path(), &["commit", "-m", "initial commit"]);
        let origin_url = origin.path().to_str().expect("utf8 path").to_string();
        git(seed.path(), &["remote", "add", "origin", &origin_url]);
        git(seed.path(), &["push", "origin", "trunk"]);

        let dir = TempDir::new().expect("tempdir");
        git(
            dir.path(),
            &[
                "clone",
                &origin_url,
                dir.path().to_str().expect("utf8 path"),
            ],
        );
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        git(dir.path(), &["checkout", "-b", "feature"]);
        fs::write(dir.path().join("file.txt"), "hello\nfrom feature\n").expect("write");

        let result = diff_against_base(dir.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert_eq!(diff.base_branch, "trunk");
        let modified = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .expect("file.txt should be in the diff");
        assert!(modified
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.content == "from feature"));
    }

    #[test]
    fn untracked_file_appears_in_diff_as_an_addition() {
        // `git diff <merge-base>` alone never sees untracked files; the shadow index's
        // `--intent-to-add` pass is what surfaces them.
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("brand_new.txt"), "content nobody staged\n").expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        let added = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("brand_new.txt"))
            .expect("an untracked file must still show up in the diff as an addition");
        assert_eq!(added.status, FileChangeStatus::Added);
        assert!(added
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == DiffLineKind::Added && l.content == "content nobody staged"));

        let status = Command::new("git")
            .current_dir(repo.path())
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(
            status_text.contains("?? brand_new.txt"),
            "the real index must be untouched by the shadow index trick, got status: \
             {status_text}"
        );
    }

    #[test]
    fn a_missing_or_unreadable_real_index_does_not_fail_the_whole_diff() {
        // `GIT_INDEX_FILE` at a nonexistent path is a fresh empty index and `git add` succeeds;
        // at an existing 0-byte file it fails with `index file smaller than expected`. Leaving
        // the placeholder in place therefore breaks the whole Changes panel.
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("brand_new.txt"), "content nobody staged\n").expect("write");

        // The index unreadable exactly when `prepare_shadow_index` reads it: no index written
        // yet, or an agent's own concurrent `git add` rewriting it in the same window.
        let index_path_output = Command::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "--git-path", "index"])
            .output()
            .expect("git rev-parse --git-path index");
        let index_path = repo
            .path()
            .join(String::from_utf8_lossy(&index_path_output.stdout).trim());
        assert!(
            index_path.exists(),
            "sanity check: a real index must exist after a commit"
        );
        fs::remove_file(&index_path)
            .expect("remove the real index to simulate it being unreadable");

        let result = diff_against_base(repo.path())
            .expect("a missing real index must not fail the whole diff computation");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert!(
            diff.files
                .iter()
                .any(|f| f.path == Path::new("brand_new.txt")),
            "the diff must still compute (and still see the real untracked file) even when \
             the real index couldn't be read to seed the shadow copy"
        );
    }

    /// The shadow index must be created in the worktree's own git directory, never in
    /// `std::env::temp_dir()` - see [`prepare_shadow_index`].
    ///
    /// Asserted on the parent directory rather than by pointing `TMPDIR` somewhere hostile:
    /// `set_var` is process-global and these tests run in parallel threads that all use
    /// `tempfile`, so that would corrupt unrelated tests instead of proving anything.
    fn transient_failure() -> Error {
        Error::GitCommand {
            args: "add --intent-to-add -A -- .".into(),
            exit: GitExit::Code(128),
            stderr: "fatal: Unable to write new index file".into(),
        }
    }

    fn permanent_failure() -> Error {
        Error::GitCommand {
            args: "add --intent-to-add -A -- .".into(),
            exit: GitExit::Code(128),
            stderr: "fatal: not a git repository".into(),
        }
    }

    #[test]
    fn is_transient_index_write_failure_matches_regardless_of_case() {
        assert!(is_transient_index_write_failure(
            "fatal: Unable to write new index file"
        ));
        assert!(is_transient_index_write_failure(
            "fatal: unable to write new index file"
        ));
        assert!(!is_transient_index_write_failure(
            "fatal: not a git repository"
        ));
    }

    #[test]
    fn a_transient_failure_that_clears_on_retry_never_surfaces_to_the_caller() {
        let mut calls = 0;
        let result = retry_transient_index_write_failure(|| {
            calls += 1;
            if calls < 2 {
                Err(transient_failure())
            } else {
                Ok(())
            }
        });
        assert!(
            result.is_ok(),
            "the retry must have absorbed the transient failure"
        );
        assert_eq!(calls, 2, "must have retried exactly once before succeeding");
    }

    #[test]
    fn a_transient_failure_that_never_clears_gives_up_after_the_bound() {
        let mut calls = 0;
        let result = retry_transient_index_write_failure(|| {
            calls += 1;
            Err(transient_failure())
        });
        assert!(
            result.is_err(),
            "must give up and return the real error eventually"
        );
        assert_eq!(
            calls, MAX_INDEX_WRITE_ATTEMPTS,
            "must make exactly the documented number of attempts, no more and no less"
        );
    }

    #[test]
    fn a_non_transient_failure_is_never_retried() {
        let mut calls = 0;
        let result = retry_transient_index_write_failure(|| {
            calls += 1;
            Err(permanent_failure())
        });
        assert!(result.is_err());
        assert_eq!(
            calls, 1,
            "a non-transient failure must return on the very first attempt"
        );
    }

    #[test]
    fn prepare_shadow_index_closes_its_own_file_handle_before_returning() {
        let repo = init_repo();
        fs::write(repo.path().join("untracked.txt"), "nobody staged this\n").expect("write");

        let shadow = prepare_shadow_index(repo.path(), ShadowIndexContent::IntentToAdd)
            .expect("prepare_shadow_index");
        let shadow_path = fs::canonicalize(&*shadow).expect("canonicalize shadow path");

        let fd_dir = Path::new("/proc/self/fd");
        if !fd_dir.is_dir() {
            return;
        }
        for entry in fs::read_dir(fd_dir).expect("read /proc/self/fd") {
            let Ok(entry) = entry else { continue };
            let Ok(target) = fs::read_link(entry.path()) else {
                continue;
            };
            assert_ne!(
                target,
                shadow_path,
                "this process must hold no open file descriptor on the shadow index's own path \
                 by the time prepare_shadow_index returns - fd {:?} still points at it",
                entry.path()
            );
        }
    }

    #[test]
    fn the_shadow_index_lives_in_the_git_directory_not_the_os_temp_directory() {
        let repo = init_repo();
        fs::write(repo.path().join("untracked.txt"), "nobody staged this\n").expect("write");

        let shadow = prepare_shadow_index(repo.path(), ShadowIndexContent::IntentToAdd)
            .expect("prepare_shadow_index");

        let git_dir = repo.path().join(".git");
        let parent = shadow.parent().expect("shadow index has a parent");
        assert_eq!(
            fs::canonicalize(parent).expect("canonicalize shadow parent"),
            fs::canonicalize(&git_dir).expect("canonicalize git dir"),
            "the shadow index must be created inside the repository's own git directory"
        );
        assert_ne!(
            fs::canonicalize(parent).expect("canonicalize shadow parent"),
            fs::canonicalize(std::env::temp_dir()).expect("canonicalize temp dir"),
            "the shadow index must not depend on the OS-wide temp directory at all"
        );
        assert!(
            shadow
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .starts_with(SHADOW_INDEX_PREFIX),
            "a shadow index left behind by a killed process must be recognisably ours"
        );

        let path = shadow.to_path_buf();
        assert!(path.exists());
        drop(shadow);
        assert!(
            !path.exists(),
            "the shadow index must still be cleaned up on drop now that it lives under .git"
        );
    }

    #[test]
    fn a_linked_worktrees_shadow_index_lives_in_that_worktrees_own_git_directory() {
        let repo = init_repo();
        let holder = TempDir::new().expect("tempdir");
        let wt_path = holder.path().join("linked");
        drop(holder);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                wt_path.to_str().expect("utf8 path"),
            ],
        );
        fs::write(wt_path.join("untracked.txt"), "nobody staged this\n").expect("write");

        let shadow = prepare_shadow_index(&wt_path, ShadowIndexContent::IntentToAdd)
            .expect("prepare_shadow_index");
        let parent = shadow
            .parent()
            .expect("shadow index has a parent")
            .to_path_buf();
        assert!(
            parent.ends_with(Path::new("worktrees").join("linked")),
            "expected the linked worktree's own admin directory, got {}",
            parent.display()
        );
        drop(shadow);
        let _ = fs::remove_dir_all(&wt_path);
    }

    #[test]
    fn an_embedded_git_repository_in_the_worktree_does_not_fail_the_diff() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("brand_new.txt"), "content nobody staged\n").expect("write");

        // Needs a commit of its own: `git add` fails an embedded repo without one outright,
        // which is a different case.
        let embedded = repo.path().join("vendor").join("inner");
        fs::create_dir_all(&embedded).expect("create embedded repo dir");
        git(&embedded, &["init", "-b", "main"]);
        git(&embedded, &["config", "user.email", "test@example.com"]);
        git(&embedded, &["config", "user.name", "Test User"]);
        fs::write(embedded.join("inner.txt"), "inner\n").expect("write");
        git(&embedded, &["add", "inner.txt"]);
        git(&embedded, &["commit", "-m", "inner commit"]);

        let result = diff_against_base(repo.path())
            .expect("an embedded git repository must not fail the whole diff computation");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert!(
            diff.files
                .iter()
                .any(|f| f.path == Path::new("brand_new.txt")),
            "the real untracked file must still be reported alongside the embedded repository"
        );

        let status = Command::new("git")
            .current_dir(repo.path())
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(
            status_text.contains("?? vendor/"),
            "the embedded repository must still be untracked afterwards, got: {status_text}"
        );
    }

    #[test]
    fn untracked_and_committed_and_uncommitted_changes_all_appear_together() {
        // Combines all three kinds of change the diff is supposed to surface in one pass -
        // the exact workflow this feature exists for (check everything an agent did before
        // merge, in one view).
        let repo = init_repo();
        fs::write(repo.path().join("keep.txt"), "keep\n").expect("write");
        git(repo.path(), &["add", "keep.txt"]);
        git(repo.path(), &["commit", "-m", "add keep.txt"]);
        git(repo.path(), &["checkout", "-b", "feature"]);

        fs::write(repo.path().join("keep.txt"), "keep\ncommitted\n").expect("write");
        git(repo.path(), &["commit", "-am", "commit on feature"]);
        fs::write(repo.path().join("file.txt"), "hello\nstaged\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        fs::write(repo.path().join("new_untracked.txt"), "new\n").expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert!(diff
            .files
            .iter()
            .find(|f| f.path == Path::new("keep.txt"))
            .is_some_and(|f| f
                .hunks
                .iter()
                .flat_map(|h| &h.lines)
                .any(|l| l.kind == DiffLineKind::Added && l.content == "committed")));
        assert!(diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .is_some_and(|f| f
                .hunks
                .iter()
                .flat_map(|h| &h.lines)
                .any(|l| l.kind == DiffLineKind::Added && l.content == "staged")));
        assert!(diff
            .files
            .iter()
            .find(|f| f.path == Path::new("new_untracked.txt"))
            .is_some_and(|f| f.status == FileChangeStatus::Added));
    }

    #[test]
    fn mnemonic_prefix_config_does_not_break_path_parsing() {
        // `diff.mnemonicPrefix=true` (a real, fairly common user git config) changes git
        // diff's `a/`/`b/` prefixes to `i/`/`w/`/`c/`. `diff_against_base` must pin this
        // config explicitly (rather than relying on defaults) so path parsing is unaffected
        // by whatever the repo's own config happens to be.
        let repo = init_repo();
        git(repo.path(), &["config", "diff.mnemonicPrefix", "true"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("file.txt"), "hello\nchanged\n").expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        let modified = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .expect("file.txt should be in the diff under its real, unprefixed path");
        assert_eq!(modified.status, FileChangeStatus::Modified);
        // The bug this guards against: under `diff.mnemonicPrefix=true` without an explicit
        // override, this path would parse as `Path::new("i/file.txt")` or similar instead.
        assert_ne!(modified.path, Path::new("w/file.txt"));
    }

    #[test]
    fn falls_back_to_main_worktree_branch_when_no_origin_head_or_main_or_master() {
        // Isolates detection strategy 4 specifically: no `origin` remote at all, and no
        // local branch named `main`/`master` exists anywhere in the repo (only `trunk` and
        // `feature` do) - the only way this can pass is by actually falling back to the main
        // worktree's own checked-out branch.
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "trunk"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "initial commit"]);

        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("linked-wt");
        drop(linked_dir);
        git(
            dir.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );
        fs::write(linked_path.join("file.txt"), "hello\nfrom feature\n").expect("write");

        let result = diff_against_base(&linked_path).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert_eq!(diff.base_branch, "trunk");
    }

    #[test]
    fn unrelated_histories_yield_no_base_with_a_real_uncommitted_diff() {
        // Two branches in the same repo with genuinely no common ancestor (an orphan
        // branch) - `gix::Repository::merge_base` must report `NotFound`, which
        // `diff_against_base` surfaces as `DiffBase::NoBase` (GitHub issue #108: real
        // uncommitted changes are still worth showing, even with no comparable base branch)
        // rather than an `Err` or a fabricated `NoBaseFound`.
        let repo = init_repo();
        git(repo.path(), &["checkout", "--orphan", "unrelated"]);
        git(repo.path(), &["rm", "-rf", "--cached", "."]);
        fs::remove_file(repo.path().join("file.txt")).expect("remove");
        fs::write(
            repo.path().join("other.txt"),
            "a totally unrelated history\n",
        )
        .expect("write");
        git(repo.path(), &["add", "other.txt"]);
        git(repo.path(), &["commit", "-m", "unrelated root commit"]);
        fs::write(repo.path().join("other.txt"), "edited after commit\n").expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::NoBase {
            branch,
            uncommitted,
        } = result
        else {
            panic!("expected DiffBase::NoBase, got {result:?}");
        };
        assert_eq!(branch, Some("main".to_string()));
        assert_eq!(uncommitted.files.len(), 1);
        assert_eq!(uncommitted.files[0].path, Path::new("other.txt"));
    }

    #[test]
    fn stdout_and_stderr_are_drained_concurrently_without_deadlocking() {
        // A child that writes well past a typical OS pipe buffer (64KB on Linux) to stderr
        // *before* writing anything to stdout and exiting. If stdout and stderr were read
        // sequentially (stdout to EOF, only then stderr - what an earlier version of
        // `capture_git_stdout` did), this child would block forever writing to a full
        // stderr pipe while this process blocked reading an empty stdout, and this test
        // would hang. Uses `sh`, not `git`, so the scenario is reproduced deterministically
        // without needing to coax git itself into writing that much stderr.
        let mut child = Command::new("sh")
            .args([
                "-c",
                "head -c 200000 /dev/zero | tr '\\0' 'e' >&2; printf done",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning sh should succeed - this environment must have /bin/sh on PATH");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = read_streams_concurrently(&mut child, MAX_DIFF_OUTPUT_BYTES);
            let _ = child.wait();
            let _ = tx.send(result);
        });

        let result = rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "read_streams_concurrently should return within 10s if stdout/stderr are \
                 drained concurrently - hitting this timeout means the old sequential-read \
                 deadlock has regressed",
        );
        let (stdout, truncated, stderr_text) = result.expect("read_streams_concurrently");
        assert!(!truncated);
        assert_eq!(stdout, b"done");
        assert!(stderr_text.len() <= MAX_STDERR_BYTES);
    }

    #[test]
    fn falls_back_to_local_master_when_no_main_or_origin() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "master"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write");
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "-m", "initial commit"]);
        git(dir.path(), &["checkout", "-b", "feature"]);
        fs::write(dir.path().join("file.txt"), "hello\nchanged\n").expect("write");

        let result = diff_against_base(dir.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert_eq!(diff.base_branch, "master");
    }

    #[test]
    fn linked_worktree_diffs_against_main_repo_default_branch() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("linked-wt");
        drop(linked_dir);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );
        fs::write(
            linked_path.join("file.txt"),
            "hello\nfrom linked worktree\n",
        )
        .expect("write in linked worktree");

        let result = diff_against_base(&linked_path).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert_eq!(diff.base_branch, "main");
        let modified = diff
            .files
            .iter()
            .find(|f| f.path == Path::new("file.txt"))
            .expect("file.txt should be in the diff");
        assert!(modified
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.content == "from linked worktree"));
    }

    #[test]
    fn parse_git_diff_handles_multiple_files_and_hunks() {
        let text = "\
diff --git a/one.txt b/one.txt
index 1234567..89abcde 100644
--- a/one.txt
+++ b/one.txt
@@ -1,2 +1,3 @@
 context line
-removed line
+added line
+another added line
diff --git a/two.txt b/two.txt
new file mode 100644
index 0000000..fedcba9
--- /dev/null
+++ b/two.txt
@@ -0,0 +1,1 @@
+brand new content
";
        let (files, truncated) = parse_git_diff(text);
        assert!(!truncated);
        assert_eq!(files.len(), 2);

        assert_eq!(files[0].path, Path::new("one.txt"));
        assert_eq!(files[0].status, FileChangeStatus::Modified);
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(
            files[0].hunks[0].lines,
            vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    content: "context line".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Removed,
                    content: "removed line".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    content: "added line".to_string()
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    content: "another added line".to_string()
                },
            ]
        );

        assert_eq!(files[1].path, Path::new("two.txt"));
        assert_eq!(files[1].status, FileChangeStatus::Added);
    }

    #[test]
    fn parse_git_diff_caps_files_and_marks_truncated() {
        let mut text = String::new();
        for i in 0..(MAX_FILES + 5) {
            text.push_str(&format!(
                "diff --git a/f{i}.txt b/f{i}.txt\n--- a/f{i}.txt\n+++ b/f{i}.txt\n@@ -1 +1 @@\n-old\n+new\n"
            ));
        }
        let (files, truncated) = parse_git_diff(&text);
        assert!(truncated);
        assert_eq!(files.len(), MAX_FILES);
    }

    #[test]
    fn parse_git_diff_caps_hunk_lines_per_file() {
        let mut text = String::from(
            "diff --git a/big.txt b/big.txt\n--- a/big.txt\n+++ b/big.txt\n@@ -1 +1 @@\n",
        );
        for i in 0..(MAX_HUNK_LINES_PER_FILE + 10) {
            text.push_str(&format!("+line {i}\n"));
        }
        let (files, files_truncated) = parse_git_diff(&text);
        assert!(!files_truncated);
        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert_eq!(files[0].hunks[0].lines.len(), MAX_HUNK_LINES_PER_FILE);
    }

    #[test]
    fn merge_status_reports_merged_true_for_a_worktree_fully_contained_in_base() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        git(repo.path(), &["checkout", "main"]);

        // Real linked worktree, not just a branch switch in place - this is what the rail
        // actually inspects.
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("feature-wt");
        drop(linked_dir);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                linked_path.to_str().expect("utf8 path"),
                "feature",
            ],
        );

        let status = merge_status_against_base(&linked_path)
            .expect("merge_status_against_base")
            .expect("a base should be detected");
        assert_eq!(status.base_branch, "main");
        assert!(status.merged, "an unchanged branch off main is merged");
        assert!(status.head_committer_unix_seconds.is_some());
    }

    #[test]
    fn merge_status_reports_merged_false_for_a_worktree_with_unique_commits() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("feature.txt"), "feature work\n").expect("write");
        git(repo.path(), &["add", "feature.txt"]);
        git(repo.path(), &["commit", "-m", "unique feature commit"]);
        git(repo.path(), &["checkout", "main"]);

        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("feature-wt");
        drop(linked_dir);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                linked_path.to_str().expect("utf8 path"),
                "feature",
            ],
        );

        let status = merge_status_against_base(&linked_path)
            .expect("merge_status_against_base")
            .expect("a base should be detected");
        assert!(
            !status.merged,
            "a branch with a commit not on main must not be reported as merged"
        );
    }

    #[test]
    fn merge_status_is_none_when_no_base_is_detectable() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        let status = merge_status_against_base(dir.path()).expect("merge_status_against_base");
        assert_eq!(status, None);
    }

    #[test]
    fn merge_status_on_the_default_branch_itself_is_trivially_merged() {
        let repo = init_repo();
        let status = merge_status_against_base(repo.path())
            .expect("merge_status_against_base")
            .expect("main itself has a detectable base (itself)");
        assert_eq!(status.base_branch, "main");
        assert!(status.merged);
    }

    #[test]
    fn ahead_behind_on_default_branch_is_zero_and_zero() {
        let repo = init_repo();
        let result = ahead_behind_against_base(repo.path())
            .expect("ahead_behind_against_base")
            .expect("main itself has a detectable base (itself)");
        assert_eq!(
            result,
            AheadBehind {
                ahead: 0,
                behind: 0
            }
        );
    }

    #[test]
    fn ahead_behind_unborn_head_is_none() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        let result = ahead_behind_against_base(dir.path()).expect("ahead_behind_against_base");
        assert_eq!(result, None);
    }

    #[test]
    fn ahead_behind_counts_real_diverged_commits_on_each_side() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("feature_one.txt"), "one\n").expect("write");
        git(repo.path(), &["add", "feature_one.txt"]);
        git(repo.path(), &["commit", "-m", "feature commit one"]);
        fs::write(repo.path().join("feature_two.txt"), "two\n").expect("write");
        git(repo.path(), &["add", "feature_two.txt"]);
        git(repo.path(), &["commit", "-m", "feature commit two"]);

        git(repo.path(), &["checkout", "main"]);
        fs::write(repo.path().join("main_only.txt"), "main only\n").expect("write");
        git(repo.path(), &["add", "main_only.txt"]);
        git(repo.path(), &["commit", "-m", "a commit only main has"]);

        git(repo.path(), &["checkout", "feature"]);
        let result = ahead_behind_against_base(repo.path())
            .expect("ahead_behind_against_base")
            .expect("feature has a detectable base (main)");
        assert_eq!(
            result,
            AheadBehind {
                ahead: 2,
                behind: 1
            }
        );
    }

    #[test]
    fn ahead_behind_with_no_divergence_at_all_is_zero_and_zero() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        let result = ahead_behind_against_base(repo.path())
            .expect("ahead_behind_against_base")
            .expect("feature has a detectable base (main)");
        assert_eq!(
            result,
            AheadBehind {
                ahead: 0,
                behind: 0
            }
        );
    }

    #[test]
    fn ahead_behind_is_computed_against_the_detected_base_commit_not_a_stale_local_branch_of_the_same_name(
    ) {
        let origin = TempDir::new().expect("tempdir");
        git(origin.path(), &["init", "--bare", "-b", "main"]);

        let seed = TempDir::new().expect("tempdir");
        git(seed.path(), &["init", "-b", "main"]);
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        fs::write(seed.path().join("file.txt"), "hello\n").expect("write");
        git(seed.path(), &["add", "file.txt"]);
        git(seed.path(), &["commit", "-m", "initial commit"]);
        let origin_url = origin.path().to_str().expect("utf8 path").to_string();
        git(seed.path(), &["remote", "add", "origin", &origin_url]);
        git(seed.path(), &["push", "origin", "main"]);

        let dir = TempDir::new().expect("tempdir");
        git(
            dir.path(),
            &[
                "clone",
                &origin_url,
                dir.path().to_str().expect("utf8 path"),
            ],
        );
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        git(dir.path(), &["checkout", "-b", "feature"]);

        // Advance origin's `main` two real commits further (from the seed clone - the origin
        // itself is bare and can't be committed to directly).
        fs::write(seed.path().join("file.txt"), "hello\nfrom origin 1\n").expect("write");
        git(seed.path(), &["commit", "-am", "origin commit 1"]);
        fs::write(
            seed.path().join("file.txt"),
            "hello\nfrom origin 1\nfrom origin 2\n",
        )
        .expect("write");
        git(seed.path(), &["commit", "-am", "origin commit 2"]);
        git(seed.path(), &["push", "origin", "main"]);

        // Update the real worktree's remote-tracking ref (`refs/remotes/origin/main`, and thus
        // `refs/remotes/origin/HEAD`'s target) to the fresh tip - deliberately *not* touching
        // the local `refs/heads/main`, which stays 2 commits stale. This is exactly the
        // real-world shape the audit reproduced.
        git(dir.path(), &["fetch", "origin"]);

        let result = ahead_behind_against_base(dir.path()).expect("ahead_behind_against_base");
        let ahead_behind = result.expect("a real base should have been detected");
        assert_eq!(
            ahead_behind.behind, 2,
            "must be computed against the real detected base commit (origin/main's fresh, \
             fetched tip), not the stale local `main` branch of the same short name - got \
             {ahead_behind:?}"
        );
        assert_eq!(ahead_behind.ahead, 0);
    }

    #[test]
    fn unparsable_rev_list_output_parses_to_none_not_a_fabricated_zero() {
        assert_eq!(
            parse_ahead_behind_counts(""),
            None,
            "empty output must not fabricate {{0, 0}}"
        );
        assert_eq!(
            parse_ahead_behind_counts("not-a-number also-not-a-number"),
            None,
            "non-numeric output must not fabricate {{0, 0}}"
        );
        assert_eq!(
            parse_ahead_behind_counts("3"),
            None,
            "a missing second field must not fabricate an ahead of 0"
        );
        assert_eq!(
            parse_ahead_behind_counts("2\t5"),
            Some(AheadBehind {
                ahead: 5,
                behind: 2
            }),
            "well-formed output must still parse normally"
        );
    }
    /// A feature branch with one real commit of its own plus one still-uncommitted edit - the
    /// shape that makes the three Changes-panel scopes give three genuinely different answers
    /// (GitHub issue #285).
    fn repo_with_a_commit_and_a_dirty_file() -> TempDir {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("committed.txt"), "one\ntwo\n").expect("write committed");
        git(repo.path(), &["add", "committed.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "a real commit on the feature branch"],
        );
        fs::write(
            repo.path().join("file.txt"),
            "hello\nedited but not committed\n",
        )
        .expect("write dirty");
        repo
    }

    #[test]
    fn the_uncommitted_scope_excludes_what_is_already_committed_on_this_branch() {
        // The whole reason `diff_against_head` exists (GitHub issue #285): `diff_against_base`
        // lists `committed.txt` because it differs from `main`, but nothing about it is dirty.
        let repo = repo_with_a_commit_and_a_dirty_file();

        let against_base = diff_against_base(repo.path()).expect("diff_against_base");
        let base_paths: Vec<&Path> = against_base
            .diff()
            .expect("a real base diff")
            .files
            .iter()
            .map(|file| file.path.as_path())
            .collect();
        assert!(
            base_paths.contains(&Path::new("committed.txt")),
            "sanity: the against-main scope really does list the committed file - otherwise the \
             assertion below would pass for the wrong reason"
        );

        let uncommitted = diff_against_head(repo.path())
            .expect("diff_against_head")
            .expect("HEAD is born, so there is a real answer");
        let paths: Vec<&Path> = uncommitted
            .files
            .iter()
            .map(|file| file.path.as_path())
            .collect();
        assert_eq!(
            paths,
            vec![Path::new("file.txt")],
            "the uncommitted scope is the working tree against HEAD, so a file whose only \
             difference from main is already committed must not appear in it at all"
        );
    }

    #[test]
    fn the_uncommitted_scope_includes_a_brand_new_untracked_file() {
        // Untracked files are exactly the kind of dirty an agent produces, and `git diff HEAD`
        // cannot see them without the `--intent-to-add` shadow index.
        let repo = init_repo();
        fs::write(repo.path().join("agent_wrote_this.rs"), "fn main() {}\n").expect("write");

        let uncommitted = diff_against_head(repo.path())
            .expect("diff_against_head")
            .expect("born HEAD");
        let paths: Vec<&Path> = uncommitted
            .files
            .iter()
            .map(|file| file.path.as_path())
            .collect();
        assert!(paths.contains(&Path::new("agent_wrote_this.rs")));
    }

    #[test]
    fn the_uncommitted_scope_is_empty_not_absent_for_a_clean_worktree() {
        // `Ok(None)` means "HEAD is unborn", never "nothing changed" - a caller must be able to
        // tell a clean checkout from a repository with no commits at all.
        let repo = init_repo();
        let uncommitted = diff_against_head(repo.path())
            .expect("diff_against_head")
            .expect("a clean worktree still has a real, empty answer");
        assert!(uncommitted.files.is_empty());
    }

    #[test]
    fn an_unborn_head_has_no_uncommitted_scope_at_all() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        fs::write(dir.path().join("file.txt"), "hello\n").expect("write");
        assert_eq!(
            diff_against_head(dir.path()).expect("diff_against_head"),
            None,
            "there is no commit to diff the working tree against"
        );
    }

    #[test]
    fn the_commits_scope_lists_only_this_branch_s_own_commits_newest_first() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("first.txt"), "a\nb\n").expect("write");
        git(repo.path(), &["add", "first.txt"]);
        git(repo.path(), &["commit", "-m", "first on the branch"]);
        fs::write(repo.path().join("second.txt"), "c\n").expect("write");
        git(repo.path(), &["add", "second.txt"]);
        git(repo.path(), &["commit", "-m", "second on the branch"]);

        let commits = commits_since_base(repo.path()).expect("commits_since_base");
        assert_eq!(commits.base_branch, Some("main".to_string()));
        let subjects: Vec<&str> = commits
            .commits
            .iter()
            .map(|commit| commit.subject.as_str())
            .collect();
        assert_eq!(
            subjects,
            vec!["second on the branch", "first on the branch"],
            "newest first, and `initial commit` (which is on main too) must not be in the range"
        );
        assert_eq!(
            (commits.added, commits.removed),
            (3, 0),
            "two added lines in `first.txt` and one in `second.txt`, none removed"
        );
        assert_eq!(
            (commits.commits[0].added, commits.commits[0].removed),
            (1, 0)
        );
        assert_eq!(
            (commits.commits[1].added, commits.commits[1].removed),
            (2, 0)
        );
        assert!(commits.commits.iter().all(|commit| commit.id.len() > 7
            && commit.short_id.len() >= 4
            && commit.id.starts_with(&commit.short_id)));
        assert!(commits
            .commits
            .iter()
            .all(|commit| commit.author_time_unix > 0));
        assert!(!commits.truncated);
    }

    #[test]
    fn the_commits_scope_reports_the_net_range_diffstat_not_the_sum_of_its_commits() {
        // A line one commit adds and the next removes is in the per-commit sum twice and in the
        // net answer not at all. "What is written down" is the net answer.
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("churn.txt"), "one\ntwo\nthree\n").expect("write");
        git(repo.path(), &["add", "churn.txt"]);
        git(repo.path(), &["commit", "-m", "add three lines"]);
        fs::write(repo.path().join("churn.txt"), "one\n").expect("write");
        git(repo.path(), &["add", "churn.txt"]);
        git(repo.path(), &["commit", "-m", "take two back"]);

        let commits = commits_since_base(repo.path()).expect("commits_since_base");
        let summed = commits
            .commits
            .iter()
            .fold((0u32, 0u32), |(add, del), commit| {
                (add + commit.added, del + commit.removed)
            });
        assert_eq!(
            summed,
            (3, 2),
            "sanity: the per-commit sum really is 3 added / 2 removed"
        );
        assert_eq!(
            (commits.added, commits.removed),
            (1, 0),
            "but only one line is actually written down at the end of the range"
        );
    }

    #[test]
    fn a_worktree_on_its_own_default_branch_has_an_empty_commits_scope_not_all_of_history() {
        // "The commits this branch added" has no answer without a point to have added them from,
        // and listing every commit in the repository would be a different, wrong answer.
        let repo = init_repo();
        let commits = commits_since_base(repo.path()).expect("commits_since_base");
        assert!(commits.commits.is_empty());
        assert_eq!(commits.base_branch, None);
        assert_eq!((commits.added, commits.removed), (0, 0));
    }

    #[test]
    fn a_commit_subject_carrying_the_numstat_separator_is_still_parsed_as_one_commit() {
        // The record/field separators exist so a subject cannot be mistaken for a field boundary
        // or for the start of a numstat line.
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("tabbed.txt"), "x\n").expect("write");
        git(repo.path(), &["add", "tabbed.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "fix\t1\t2\tsrc/looks_like_numstat.rs"],
        );

        let commits = commits_since_base(repo.path()).expect("commits_since_base");
        assert_eq!(commits.commits.len(), 1);
        assert_eq!(
            commits.commits[0].subject,
            "fix\t1\t2\tsrc/looks_like_numstat.rs"
        );
        assert_eq!(
            (commits.commits[0].added, commits.commits[0].removed),
            (1, 0),
            "the subject's tab-separated numbers must not be counted as this commit's numstat"
        );
    }

    #[test]
    fn a_merge_commit_contributes_no_line_counts_rather_than_invented_ones() {
        let repo = init_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("feature.txt"), "f\n").expect("write");
        git(repo.path(), &["add", "feature.txt"]);
        git(repo.path(), &["commit", "-m", "feature work"]);
        git(repo.path(), &["checkout", "-b", "side", "main"]);
        fs::write(repo.path().join("side.txt"), "s\n").expect("write");
        git(repo.path(), &["add", "side.txt"]);
        git(repo.path(), &["commit", "-m", "side work"]);
        git(repo.path(), &["checkout", "feature"]);
        git(
            repo.path(),
            &["merge", "--no-ff", "-m", "merge side", "side"],
        );

        let commits = commits_since_base(repo.path()).expect("commits_since_base");
        let merge = commits
            .commits
            .iter()
            .find(|commit| commit.subject == "merge side")
            .expect("the merge commit is in the range");
        assert_eq!(
            (merge.added, merge.removed),
            (0, 0),
            "git reports no single meaningful diff for a merge, and this must not fabricate one"
        );
        assert_eq!(
            (commits.added, commits.removed),
            (2, 0),
            "the range's net diffstat still counts both branches' real lines"
        );
    }
}
