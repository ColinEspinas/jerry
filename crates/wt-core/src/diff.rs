//! Read-only diff of a worktree's `HEAD` (including uncommitted changes) against the
//! merge-base with the repository's default branch.
//!
//! ## What "base" means
//!
//! A git worktree has no notion of an explicit "base branch": it just has a branch (or a
//! detached commit) checked out. The useful comparison for reviewing what changed before
//! merging is against the point where the worktree's branch diverged from the repository's
//! default branch - i.e. the merge-base between the worktree's `HEAD` and the default
//! branch's tip.
//!
//! The default branch itself is detected, in order:
//! 1. `refs/remotes/origin/HEAD`, if it exists and is a symbolic ref (mirrors
//!    `git symbolic-ref refs/remotes/origin/HEAD`).
//! 2. A local `main` branch, if one exists.
//! 3. A local `master` branch, if one exists.
//! 4. The main worktree's own currently checked-out branch, as a last resort (so a
//!    repository with neither an `origin` remote nor a `main`/`master` branch still gets a
//!    sensible base).
//!
//! If none of these yield a branch, or the selected worktree's branch *is* the detected
//! default branch, or no merge-base exists between the two histories, [`diff_against_base`]
//! returns [`DiffBase::NoBase`] rather than fabricating a base branch to diff against - but it
//! still computes a real `git diff HEAD` of uncommitted changes for that case (GitHub issue
//! #108), so `DiffBase::NoBase` is not "nothing to show". [`DiffBase::NoBaseFound`] is reserved
//! for the one case where even that fallback is impossible: `HEAD` itself is unborn.
//!
//! ## What the diff includes
//!
//! This runs `git diff <merge-base>` (not `git diff <merge-base>..HEAD`), with the worktree
//! itself as the current directory. `git diff <commit>` compares `<commit>`'s tree against
//! the *working tree* (index and unstaged changes included), which is deliberate: an agent
//! working in this worktree may not have committed anything yet, so limiting the diff to
//! committed history would hide exactly the changes a reviewer most wants to see. One gap:
//! `git diff <commit>` (with no `--cached`) only ever considers paths already present in
//! `<commit>`'s tree or in the index, so a genuinely untracked file is invisible to it.
//! [`diff_against_base`] works around this with [`prepare_shadow_index`]: a throwaway,
//! `--intent-to-add`-augmented copy of the index, passed to `git diff` via a
//! `GIT_INDEX_FILE` override so untracked files show up as additions too, without ever
//! touching the real index. See that function's docs for the mechanics.
//!
//! ## Explicit git config, not caller defaults
//!
//! The `git diff` invocation pins `diff.mnemonicPrefix=false`, `diff.noprefix=false`, and
//! `core.quotePath=false` via `-c` (before the `diff` subcommand, where git requires global
//! config overrides to appear). Without this, the path-prefix parsing below
//! ([`strip_diff_prefix`]) would silently mislabel every file under a caller's
//! `diff.mnemonicPrefix=true` config (prefixes become `i/`/`w/`/`c/` instead of `a/`/`b/`),
//! and non-ASCII filenames would render octal-escaped under `core.quotePath`'s default (on)
//! setting.
//!
//! ## gix vs. the `git` CLI
//!
//! Per this crate's convention, reads go through `gix` where practical: base-branch
//! detection and merge-base computation use [`gix::Repository::find_reference`] and
//! [`gix::Repository::merge_base`].
//!
//! Producing the diff *text* is different: `gix-diff` operates at the level of tree and blob
//! objects, not the working tree, and has no built-in formatter that reproduces `git diff`'s
//! unified-diff text (hunk headers, context lines, rename/binary detection, and blending in
//! uncommitted working-tree state). Reimplementing that on top of `gix-diff`'s lower-level
//! primitives would mean re-deriving `git diff`'s own output format from scratch. `git diff`
//! already handles all of this correctly, so this module shells out to it for the diff text
//! specifically, the same way [`super::remove_worktree`]'s dirty-check shells out to `git
//! status`.
//!
//! Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::{Error, GitExit};
use crate::{check_success, format_args, git_command, open_repo, run_git};

/// Cap on how many bytes of `git diff` stdout are read into memory. A diff larger than this
/// (thousands of changed lines, or a huge generated file slipping past `.gitignore`) is
/// truncated rather than buffered without bound or left to hang the read loop; see
/// [`WorktreeDiff::truncated`].
pub(crate) const MAX_DIFF_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Cap on how many changed files are kept from a single diff. Mirrors the "cap the loaded
/// data, independent of what's rendered" approach `file_tree::build_file_tree` uses.
const MAX_FILES: usize = 300;

/// Cap on how many hunk lines are kept per file. A single enormous file (e.g. a generated
/// lockfile) shouldn't be allowed to blow up memory or rendering on its own.
const MAX_HUNK_LINES_PER_FILE: usize = 2000;

/// One line within a diff hunk.
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

/// One `@@ ... @@` hunk within a file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The hunk header line as `git diff` printed it, e.g. `@@ -1,3 +1,4 @@ fn foo() {`.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// How a single file differs from the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

/// One changed file within a [`WorktreeDiff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// The file's current path (or its last path before deletion).
    pub path: PathBuf,
    /// The file's path before the change, if different (only set for renames).
    pub old_path: Option<PathBuf>,
    pub status: FileChangeStatus,
    /// `true` if `git diff` reported this as a binary file; `hunks` is always empty in that
    /// case (binary content is never diffed line-by-line here).
    pub is_binary: bool,
    pub hunks: Vec<DiffHunk>,
    /// `true` if this file's hunk lines were cut short by [`MAX_HUNK_LINES_PER_FILE`].
    pub truncated: bool,
}

/// A real, computed diff of a worktree against its base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeDiff {
    /// The short name of the detected default branch this was diffed against.
    pub base_branch: String,
    /// The full commit id of the merge-base between the worktree's `HEAD` and the default
    /// branch (i.e. exactly what `git diff <base_commit>` was run against).
    pub base_commit: String,
    pub files: Vec<DiffFile>,
    /// `true` if the raw `git diff` output was too large to read in full
    /// ([`MAX_DIFF_OUTPUT_BYTES`]) or if more than [`MAX_FILES`] files changed; some files or
    /// hunk lines may be missing as a result.
    pub truncated: bool,
}

/// The outcome of trying to compute a worktree's diff against its base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffBase {
    /// A usable base branch was found; here is the diff against it (possibly with zero changed
    /// files, if the worktree exactly matches its base).
    Diff(WorktreeDiff),
    /// No meaningful base *branch* to diff against - either this worktree's own branch *is*
    /// the detected default branch (`branch: Some(name)`), or none could be detected, or the
    /// two histories share no common ancestor (`branch: None` for either of those). `HEAD` is
    /// still a real, born commit though, so `uncommitted` is a genuine `git diff HEAD` (staged
    /// and unstaged local edits) rather than a fabricated "nothing to show" - see GitHub issue
    /// #108: a worktree on its default branch (or with no detectable base) still deserves to
    /// show real, reviewable changes if it has any.
    NoBase {
        branch: Option<String>,
        uncommitted: WorktreeDiff,
    },
    /// Truly nothing to diff, even uncommitted changes: `HEAD` itself is unborn (a brand new
    /// repository with no commits yet), so there is no commit to diff the working tree against.
    NoBaseFound,
}

impl DiffBase {
    /// The real diff content this outcome carries, if any - `Some` for both [`DiffBase::Diff`]
    /// (a real base branch) and [`DiffBase::NoBase`] (no real base branch, but real uncommitted
    /// changes against `HEAD` instead), `None` only for [`DiffBase::NoBaseFound`]. Every
    /// consumer that wants to *show* a diff regardless of why - the Changes sidebar, tab diff
    /// stats, the rail's per-worktree summary - should read through this rather than matching
    /// `Diff` alone, or it would silently drop GitHub issue #108's fallback.
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

/// The comparison points one worktree offers, resolved once through `gix`.
///
/// Extracted so the three scopes the Changes panel draws (GitHub issue #285: *Uncommitted* =
/// working tree vs `HEAD`, *Commits* = the branch's own commits, *Against main* = the merge-base
/// diff) all agree about where `HEAD` is and where the base is, instead of each re-deriving it
/// from its own `gix` walk and being able to disagree about the answer mid-refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseResolution {
    /// The worktree's own checked-out branch, `None` when `HEAD` is detached.
    pub(crate) worktree_branch: Option<String>,
    /// The worktree's `HEAD` commit, as a hex sha. Always a real, born commit - a resolution is
    /// only produced at all once `HEAD` peels to something.
    pub(crate) head_sha: String,
    /// The detected default branch, whether or not a merge-base with it exists (see the module
    /// docs' "What 'base' means" section for the detection order).
    pub(crate) detected_base_branch: Option<String>,
    /// `Some((base branch, merge-base sha))` only when there is a genuinely usable base to diff
    /// against: a default branch was detected, it is not this worktree's own branch, and the two
    /// histories share a common ancestor.
    pub(crate) base: Option<(String, String)>,
}

impl BaseResolution {
    /// The branch name a diff produced from this resolution is *labelled* with - the real base
    /// branch where there is one, otherwise the worktree's own branch (possibly empty for a
    /// detached `HEAD`). Purely descriptive; nothing is diffed against it.
    fn label_branch(&self) -> String {
        self.detected_base_branch
            .clone()
            .unwrap_or_else(|| self.worktree_branch.clone().unwrap_or_default())
    }
}

/// Resolves `worktree_path`'s `HEAD` and its base, per this module's own base-detection order.
/// `Ok(None)` means `HEAD` is unborn - a freshly initialized repository with no commits at all,
/// which has nothing to compare against, not even its own working tree.
///
/// Performs blocking I/O (`gix` reads only).
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

    // The worktree's own branch *is* the default branch: there is a detected base, but nothing
    // meaningful to diff against it.
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
        // A real default branch exists, but shares no common ancestor with this worktree's
        // history - so there is no point to diff from.
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

/// Compute the real diff of the worktree at `worktree_path` against its base, per this
/// module's docs. Performs blocking I/O (opens the repository via `gix`, and spawns a real
/// `git diff` child process).
pub fn diff_against_base(worktree_path: &Path) -> Result<DiffBase, Error> {
    let Some(resolved) = resolve_base(worktree_path)? else {
        // Unborn HEAD: a freshly initialized repository with no commits yet has nothing to
        // diff against any base - not even its own uncommitted changes, since there is no
        // commit to diff the working tree against.
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

/// The **Uncommitted** scope (GitHub issue #285): the worktree's working tree against its own
/// `HEAD` - "what is dirty in the checkout".
///
/// Deliberately a *different question* from [`diff_against_base`], not a filtered view of it.
/// `diff_against_base` compares against the merge-base with the default branch, so its file list
/// mixes work that is already committed on this branch with work that is not
/// (`design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §1's *Against main* scope: "what
/// would land"). This compares against `HEAD`, so a file whose only difference from `main` is
/// already inside a commit on this branch does not appear here at all. The two agree only on a
/// branch with no commits of its own.
///
/// Untracked files are included, via the same `--intent-to-add` shadow index
/// ([`prepare_shadow_index`]) `diff_against_base` uses - an agent's brand-new file is exactly the
/// kind of dirty the checkout has.
///
/// `Ok(None)` means `HEAD` is unborn: there is no commit to diff the working tree against, which
/// is the same single case [`DiffBase::NoBaseFound`] is reserved for. It is never used to mean
/// "nothing changed" - that is `Ok(Some(diff))` with an empty `files`.
///
/// Performs blocking I/O (opens the repository via `gix`, and spawns a real `git diff` child
/// process).
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

/// Defensive validation for an object id about to be handed to a spawned `git` process as a
/// bare argument: it must be a non-empty, all-ASCII-hex string, so it can never be misparsed as
/// a flag or as revision syntax. Every in-crate caller produces these from a real `gix` object
/// id (or from `git write-tree`'s own stdout), but re-checking costs nothing and means a future
/// change to how one is derived can't silently turn into a git-argument injection.
///
/// Deliberately does **not** pin a length (40 for SHA-1, 64 for SHA-256): a repository's hash
/// algorithm is the repository's business, and hardcoding today's two lengths here would be a
/// latent failure for a future one without buying anything the hex-only check doesn't already.
///
/// `pub(crate)` so [`crate::review`]'s own tree-id arguments go through this exact check rather
/// than a second, drifting copy of it.
pub(crate) fn validate_object_id(id: &str, what: &str) -> Result<(), Error> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::WorktreeIo(std::io::Error::other(format!(
            "{what} was not a hex object id"
        ))));
    }
    Ok(())
}

/// Runs `git diff <object_id>` against the worktree's real working tree (see the module docs'
/// "What the diff includes" section) and folds the result into a [`WorktreeDiff`]. `label_branch`
/// is purely descriptive - the real base branch name for [`DiffBase::Diff`], or whatever branch
/// name is available (possibly none) for the [`DiffBase::NoBase`] uncommitted-vs-`HEAD` fallback.
///
/// `object_id` is a commit id for every caller in this module, but `git diff <object>` resolves a
/// **tree** id exactly the same way - it diffs that tree against the working tree either way - so
/// [`crate::review::diff_against_tree`] reuses this function verbatim against a
/// `git write-tree` snapshot rather than duplicating the invocation, the shadow index, the pinned
/// config, and the parser. That is why this is `pub(crate)` rather than private.
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
        // Global config overrides (`-c key=value`) must precede the subcommand for `git` to
        // accept them - see the module docs' "Explicit git config" section for why these are
        // pinned rather than left to the caller's own git config.
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

/// Cap on how many commits the **Commits** scope loads, mirroring [`MAX_FILES`]' "cap the loaded
/// data, independent of what's rendered" convention. A branch with more commits than this than its
/// base is truncated, not silently hung on.
const MAX_BRANCH_COMMITS: usize = 300;

/// One commit written down on this branch since its base - a row of the **Commits** section
/// (GitHub issue #285).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCommit {
    /// The full hex commit id.
    pub id: String,
    /// git's own abbreviation of [`Self::id`] (`%h`), honouring the repository's real
    /// `core.abbrev`/uniqueness rules - never a hand-truncated prefix.
    pub short_id: String,
    pub subject: String,
    pub author_name: String,
    pub author_time_unix: i64,
    /// Lines this commit added, summed over its own `--numstat`.
    ///
    /// **Zero for a merge commit**, and honestly so: `git log --numstat` with no `-m`/`-c` reports
    /// no file lines for a merge, git's own "a merge has no single meaningful diff" behaviour -
    /// exactly what [`crate::graph::commit_changed_files`] already documents for the same reason.
    /// A binary file also contributes nothing, since git prints `-` rather than a line count for
    /// one; that is a real absence of a line count, not a zero this function invented.
    pub added: u32,
    pub removed: u32,
}

/// The **Commits** scope (GitHub issue #285): what is already written down on this branch, and
/// the net diffstat of writing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCommits {
    /// The short name of the base branch the range was taken against, or `None` when there is no
    /// usable base (see [`DiffBase::NoBase`]'s own cases) - in which case [`Self::commits`] is
    /// empty rather than falling back to the whole of history.
    pub base_branch: Option<String>,
    /// The merge-base commit the range starts at, when there is one.
    pub base_commit: Option<String>,
    /// Newest first, exactly as `git log` lists them.
    pub commits: Vec<BranchCommit>,
    /// The **net** diffstat of `<merge-base>..HEAD` - one `git diff --numstat`, not the sum of
    /// [`Self::commits`]' own stats.
    ///
    /// The two are genuinely different numbers and the net one is the honest answer to "what is
    /// written down": a line a commit added and a later commit removed is in the sum twice and in
    /// the net answer not at all, and a merge commit contributes nothing to the sum at all (see
    /// [`BranchCommit::added`]). This is the same quantity `Against main` reports, minus whatever
    /// is still uncommitted.
    pub added: u32,
    pub removed: u32,
    /// `true` if more than [`MAX_BRANCH_COMMITS`] commits were on the branch, so [`Self::commits`]
    /// is a prefix of the real range.
    pub truncated: bool,
}

/// Reads the commits on `worktree_path`'s branch since its merge-base with the detected default
/// branch, plus the net diffstat of that range - the **Commits** section's data (GitHub issue
/// #285: "what is written down").
///
/// With no usable base (an unborn `HEAD`, no detectable default branch, the worktree sitting on
/// the default branch itself, or unrelated histories) this returns an **empty** range rather than
/// falling back to all of history: "the commits this branch added" has no answer without a point
/// to have added them from, and listing every commit in the repository would be a different,
/// wrong answer rather than a degraded one.
///
/// Performs blocking I/O: opens the repository via `gix` and spawns two real `git` child
/// processes.
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

    // One `git log` for both the commit list and each commit's own numstat. `%x1e` (RS) starts
    // every record and `%x1f` (US) separates the fields inside its header line, so a subject
    // containing tabs, newlines or any other punctuation cannot be misparsed as a field boundary
    // or as the start of a numstat line.
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

    // The section header's own diffstat: the *net* change across the whole range, in one call.
    // `..` (two dots) with a real merge-base on the left is the same range `git log` above walked.
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

/// Parses `git log --numstat --format=%x1e%H%x1f%h%x1f%an%x1f%at%x1f%s` output - see
/// [`commits_since_base`] for why those separators.
fn parse_branch_commits(text: &str) -> Vec<BranchCommit> {
    let mut commits = Vec::new();
    // `split('\u{1e}')` yields one leading empty piece before the first record; `filter` drops it
    // rather than a `skip(1)` that would silently eat a real record if git ever stopped emitting
    // the leading separator.
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
        // A header that doesn't carry a real hex id is not a commit record - skipped rather than
        // recorded with a fabricated id that a later `git show` would fail on.
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let (added, removed) = sum_numstat(&lines.collect::<Vec<_>>().join("\n"));
        commits.push(BranchCommit {
            id: id.to_string(),
            short_id: short_id.to_string(),
            subject: subject.to_string(),
            author_name: author_name.to_string(),
            // An unparseable `%at` becomes `0` rather than dropping the commit: the timestamp only
            // drives a relative-age label, and losing the commit entirely would be the bigger lie.
            author_time_unix: author_time.parse().unwrap_or(0),
            added,
            removed,
        });
    }
    commits
}

/// Sums `git --numstat` lines (`<added>\t<removed>\t<path>`).
///
/// A binary file's `-\t-\t<path>` contributes nothing, which is git's own way of saying it has no
/// line counts - deliberately not counted as a zero-line change *or* skipped silently into some
/// other bucket. Saturating, so a pathological diff cannot wrap a total into a smaller number.
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

/// Merge-state of a worktree's `HEAD` against the repository's detected default base branch
/// (see the module docs' "What 'base' means" section for the detection order) - powers the
/// session rail's "by project" worktree rows
/// (`design_handoff_jerry_ade/README.md`: a worktree whose branch is fully merged into the
/// default branch, with no running session, is offered as `merged HH:MM · prunable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeMergeStatus {
    /// The short name of the detected default branch this was checked against.
    pub base_branch: String,
    /// `true` if the worktree's `HEAD` is an ancestor of (or equal to) the base branch's
    /// tip - i.e. every commit reachable from `HEAD` is already reachable from the base
    /// branch, the same condition `git branch --merged <base>` reports. Computed via
    /// `gix::Repository::merge_base`: `HEAD` is merged iff its own id *is* the merge-base of
    /// (`HEAD`, base tip).
    pub merged: bool,
    /// The worktree `HEAD` commit's committer timestamp, as seconds since the Unix epoch
    /// (UTC) - read via `gix`'s `Commit::time()`. `None` only if the commit object itself
    /// could not be decoded; treated as "unknown" rather than a hard error, since it only
    /// affects a display label, not the `merged` verdict itself.
    pub head_committer_unix_seconds: Option<i64>,
}

/// Compute [`WorktreeMergeStatus`] for the worktree at `worktree_path`: whether its `HEAD`
/// has already been fully merged into the repository's detected default branch, per this
/// module's own base-detection order. Returns `Ok(None)` if no sensible base could be
/// detected or `HEAD` is unborn, rather than guessing.
///
/// Performs blocking I/O (`gix` reads only - no `git` child process is spawned here, unlike
/// [`diff_against_base`]).
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
        // Unborn HEAD: nothing has been committed yet, so there is nothing to compare.
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
            // No common ancestor at all: definitely not merged.
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

/// Real ahead/behind commit counts for a worktree's `HEAD` against the repository's detected
/// default base branch - the status bar's `↑2 ↓0` indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AheadBehind {
    /// Commits reachable from `HEAD` but not from the base branch's tip.
    pub ahead: usize,
    /// Commits reachable from the base branch's tip but not from `HEAD`.
    pub behind: usize,
}

/// Computes real [`AheadBehind`] counts for the worktree at `worktree_path` against its
/// detected default base branch, per this module's own base-detection order (see the module
/// docs). Returns `Ok(None)` for the same "nothing sensible to compare" cases
/// [`merge_status_against_base`] does: no base could be detected, or `HEAD` is unborn.
/// [`AheadBehind`] is trivially `{0, 0}` when the worktree's own branch *is* the detected base.
///
/// Shells out to `git rev-list --left-right --count <base_commit>...HEAD` (the `...` symmetric
/// difference already computes the merge-base internally, so there's no need to pass one
/// explicitly) - `<base_commit>...HEAD` reports `<count only reachable from base>\t<count only
/// reachable from HEAD>`, i.e. `<behind>\t<ahead>`. Before running it, this confirms via `gix`
/// that a real common ancestor exists (mirrors [`diff_against_base`]'s own `NoBaseFound`
/// handling for unrelated histories) - without that check, two branches with no shared history
/// would silently degrade into "every commit reachable from either side", not a real ahead/
/// behind count.
///
/// The `git` invocation is given `base_commit_id`'s real hex sha - *not* `base_branch`'s short
/// name - exactly as [`diff_against_base`] already does for its own `git diff` invocation. A
/// bare short name is not safe here: when `detect_default_base` finds its base via
/// `refs/remotes/origin/HEAD` (a common, normal case), the short name it returns (e.g. `main`)
/// is ambiguous in a repository that *also* has a local branch of the same name - git's own
/// disambiguation rules resolve a bare `main...HEAD` against the local `refs/heads/main` first,
/// not the remote-tracking `refs/remotes/origin/main` that was actually detected as the real
/// base. A local `main` that's gone stale relative to `origin/main` would then silently compare
/// against the wrong, stale commit and could under-report (or entirely miss) how far behind the
/// worktree really is. The hex commit id has no such ambiguity.
///
/// Performs blocking I/O: opens the repository via `gix` and spawns a real `git` child process.
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
        // Unborn HEAD: nothing has been committed yet, so there is nothing to compare.
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
    // Same defensive hex-only check `diff_against_base` applies to its own merge-base sha
    // before handing it to a spawned `git` argument - see that function's own comment for why
    // this costs nothing and guards against a future change silently turning into a
    // git-argument injection.
    if base_commit_sha.is_empty() || !base_commit_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::WorktreeIo(std::io::Error::other(
            "detected base commit id was not a hex object id",
        )));
    }

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

/// Parses `git rev-list --left-right --count <base>...HEAD`'s stdout (`<behind> <ahead>`,
/// whitespace-separated) into a real [`AheadBehind`]. `None` - not a fabricated `{0, 0}` via
/// `.unwrap_or(0)` - if either field is missing or isn't a real number, matching this codebase's
/// own established "no entry rather than a fabricated value" convention (e.g. `rail.rs`'s
/// clean/merge note handling) for exactly this class of situation: a confident-looking but wrong
/// "up to date" is worse than an honestly-omitted value.
fn parse_ahead_behind_counts(text: &str) -> Option<AheadBehind> {
    let mut parts = text.split_whitespace();
    let behind = parts.next().and_then(|part| part.parse::<usize>().ok())?;
    let ahead = parts.next().and_then(|part| part.parse::<usize>().ok())?;
    Some(AheadBehind { ahead, behind })
}

/// Detect the repository's default branch and its tip commit id, per this module's
/// documented detection order. Returns `Ok(None)` if none of the strategies yield a branch
/// (rather than an `Err`): an undetectable default branch is a real, expected outcome (e.g. a
/// repository with no `origin` remote and a default branch named something other than `main`
/// or `master`), not a failure.
///
/// `pub(crate)` (not private): `crate::merge` reuses this exact detection logic to find the
/// real base branch a session's worktree merges into, rather than reimplementing it - see
/// that module's docs.
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

    // Last resort: the main worktree's own currently checked-out branch. `main_repo()`
    // succeeds even when the main repository is bare (per gix's own docs, "the main repo
    // might be bare") - it just opens the common dir either way. The "nothing to fall back
    // to" cases are handled explicitly below: the main repo's `HEAD` being unborn
    // (`try_peel_to_id_in_place` returns `None`) or detached (`referent_name` returns
    // `None`). `main_repo()` can still fail for other reasons (e.g. a corrupt common dir),
    // treated the same as "nothing found" here, consistent with this function's `Ok(None)`
    // contract.
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

/// Cap on how many bytes of a spawned child's stderr are read into memory - see
/// [`read_streams_concurrently`]'s docs for why stderr needs its own cap and its own
/// concurrent reader thread, not just stdout's own cap.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Run `git` with `args` in `dir`, capturing up to `max_bytes` of stdout. If `index_override`
/// is `Some`, the child runs with `GIT_INDEX_FILE` pointed at that path instead of the
/// worktree's real index - see [`prepare_shadow_index`]. If more stdout is available than
/// `max_bytes`, the child is killed (rather than waited on to completion) and the second
/// element of the returned tuple is `true` - mirrors [`super::is_dirty`]'s reasoning for not
/// risking a blocked read against a pipe the child may still be filling.
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

/// Reads `child`'s stdout (capped at `max_stdout_bytes`) and stderr (capped at
/// [`MAX_STDERR_BYTES`]) *concurrently* - stderr is drained on a dedicated thread while this
/// thread drains stdout - rather than reading one pipe to completion before starting on the
/// other.
///
/// Reading them sequentially (stdout to EOF, only then stderr) can deadlock: each pipe has a
/// bounded OS buffer (64KB on Linux), and if the child writes enough to stderr before
/// finishing stdout - plausible for `git diff` (e.g. one CRLF/smudge-filter warning line per
/// changed file, across up to `MAX_FILES` files) - the child blocks writing stderr while
/// this process is blocked reading stdout, and neither side can make progress. Draining both
/// concurrently means neither pipe can ever back up far enough to block the child's writes.
///
/// Returns `(stdout, stdout_truncated, stderr_text)`; does not wait on `child` - the caller
/// is responsible for that (and for killing it first if truncated).
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
                            // Past the cap: keep draining so the child can never block on a
                            // full stderr pipe, but stop accumulating what's read - mirrors
                            // stdout's own cap-then-keep-not-blocking approach.
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
///
/// `pub(crate)` because [`crate::review::snapshot_worktree_tree`] needs the second flavour: a
/// `git write-tree` snapshot has to name real blobs, and an intent-to-add stub has no blob to
/// name (`write-tree` refuses outright with `fatal: ... has intent-to-add entries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowIndexContent {
    /// `git add --intent-to-add -A` - records just enough for `git diff <object>` to treat an
    /// untracked path as present, without writing any blob into the object database. The
    /// long-standing default for [`compute_diff`]; see this function's own docs for why that is
    /// sufficient there.
    IntentToAdd,
    /// `git add -A` - stages real content, so every untracked/modified file's blob genuinely
    /// lands in the object database and `git write-tree` can name it. Writes real objects (the
    /// only way to snapshot a working tree at all), but still only ever into the object database:
    /// the real index, working tree, `HEAD` and stash are as untouched as with `IntentToAdd`,
    /// because every mutation still goes through the same `GIT_INDEX_FILE` override.
    ///
    /// **Unbounded in the size of the untracked set**, which is why
    /// [`crate::review::snapshot_worktree_tree`] measures that set before ever asking for this -
    /// see [`crate::review::MAX_UNTRACKED_SNAPSHOT_BYTES`] and the real 19 GB worktree that
    /// motivated the cap.
    FullContent,
    /// `git add -u` - stages real content for **tracked** paths only (including deletions), and
    /// never touches untracked files at all.
    ///
    /// The bounded fallback for a worktree whose untracked set is too large to hash into the
    /// object database. Bounded by construction: every path it can write a blob for is already
    /// in git's history, so the work is proportional to what the user has actually committed
    /// rather than to whatever build output happens to be sitting in the directory.
    ///
    /// Known limitation: `git write-tree` refuses an index containing intent-to-add entries, and
    /// unlike [`Self::FullContent`]'s `add -A` this flavour does not materialize them. A worktree
    /// where the user has run a real `git add -N` therefore fails to snapshot through this path.
    /// That surfaces as an ordinary error (the caller logs it and leaves the review surface
    /// unavailable), not as a wrong answer.
    TrackedOnly,
}

/// Filename prefix every shadow index carries, so a file left behind by a hard-killed process is
/// recognisable as this app's and not mistaken for something git itself owns. Leading dot for the
/// same reason git's own transient files use one.
const SHADOW_INDEX_PREFIX: &str = ".jerry-shadow-index-";

/// Creates the throwaway index file [`prepare_shadow_index`] hands to `GIT_INDEX_FILE`, in the
/// directory holding `real_index_path` - this worktree's own git directory (`<repo>/.git` for a
/// main checkout, `<common-dir>/worktrees/<name>` for a linked one, since
/// `git rev-parse --git-path index` already resolves that distinction for us). See
/// [`prepare_shadow_index`]'s own "Where the shadow index file lives" docs for why this is not
/// `std::env::temp_dir()`.
///
/// Falls back to the OS temp directory in the two cases where the git directory genuinely can't
/// host the file: `real_index_path` has no parent at all (not reachable through any real
/// `rev-parse` output, guarded rather than unwrapped), or creating a file there fails - a
/// repository checked out on a read-only mount being the real instance of the latter, which still
/// diffs fine through a temp-dir shadow index because an `--intent-to-add` pass writes nothing
/// into the repository itself. The fallback is deliberately *not* silent about failing too: if
/// both locations refuse, the git directory's own error is what propagates, since that's the one
/// describing the repository the caller actually asked about.
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

/// Builds a temporary, throwaway copy of the worktree's index with untracked files added,
/// either as "intent to add" stubs or with their real content ([`ShadowIndexContent`]), so
/// `git diff <merge-base>` - which only ever considers paths already present in the index or in
/// `<merge-base>`'s tree (see the module docs' "What the diff includes" section) - picks up new,
/// unstaged files as additions instead of silently omitting them, and so
/// [`crate::review::snapshot_worktree_tree`] can `git write-tree` a complete snapshot.
///
/// The real index is only ever *read* (to seed the copy); every mutation happens on the temp
/// file via a `GIT_INDEX_FILE` override in the caller, so this can never perturb the real
/// repository state - verified by checking that `git status` still reports the untracked
/// file as untracked (`??`), never staged, immediately after diffing through a shadow index.
///
/// [`ShadowIndexContent::IntentToAdd`] (rather than a full `git add -A`) is deliberate for
/// [`compute_diff`]: it records just enough (an empty-blob stub entry) for `git diff` to treat
/// the path as present, without staging actual file content into the temp index - unnecessary,
/// since `git diff <commit>` (no `--cached`) always compares against the real working-tree file
/// content directly regardless of what's staged. [`crate::review::snapshot_worktree_tree`] is the
/// one caller that genuinely does need the content, and asks for
/// [`ShadowIndexContent::FullContent`] instead - see that variant's own docs.
///
/// The copy also inherits the real index's **mtime**, not just its bytes - an index's cached
/// stat data only means anything relative to that index file's own timestamp (git's
/// racy-index rule). See the inline comment at the copy itself, and the
/// `a_same_length_edit_racy_against_the_index_timestamp_is_still_reported` test, for the real
/// bug (GitHub issue #163) that came of not doing this.
///
/// ## Where the shadow index file lives
///
/// Next to the **real** index, inside this worktree's own git directory
/// ([`shadow_index_file`]) - deliberately *not* `std::env::temp_dir()`. `git add` writes an
/// index by creating `<GIT_INDEX_FILE>.lock` beside the target and renaming it over the top,
/// so whichever directory this file sits in is the directory git has to be able to create,
/// write, fsync and rename within. Pointing that at the OS-wide temp directory made every
/// shadow-index-backed operation in this crate (`compute_diff`,
/// [`crate::review::snapshot_worktree_tree`], [`crate::review::changed_paths_against_tree`])
/// silently depend on a directory that has nothing to do with the repository being diffed, and
/// that a real environment can make unusable in ways the repository itself is not: a `TMPDIR`
/// pointed at a different (or cross-OS-mounted) filesystem, a sandbox with its own private
/// `/tmp`, a full or quota-exceeded temp filesystem, or a cleanup daemon deleting files by age
/// (which this function actively invites, since it back-dates the copy's mtime to the real
/// index's possibly weeks-old mtime just above). A user running against a real repository hit
/// exactly this class of failure: `git add --intent-to-add -A -- .` exiting 128 with
/// `fatal: unable to write new index file`, which is git failing to write/rename the index at
/// this very path. The git directory is guaranteed to be on the same filesystem as the
/// repository git is already reading and writing, so if git can operate on the repo at all it
/// can write here.
///
/// The file is still a real [`tempfile::NamedTempFile`] with unchanged drop-based cleanup -
/// only its parent directory changed - and `.git` is never part of the worktree scan, so the
/// `git add -A -- .` below cannot see (let alone stage) it.
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
    // Whether the real index couldn't be read at all - decided here, acted on only *after* the
    // handle below is closed. See this function's own "Why the handle must be closed" docs for
    // why acting on it immediately (as this used to) was itself part of the bug.
    let real_index_missing;
    match std::fs::read(&real_index_path) {
        Ok(real_index_bytes) => {
            real_index_missing = false;
            use std::io::Write as _;
            (&shadow)
                .write_all(&real_index_bytes)
                .map_err(Error::WorktreeIo)?;
            (&shadow).flush().map_err(Error::WorktreeIo)?;
            // GitHub issue #163. Copying the bytes is not enough: an index's cached stat data
            // is only meaningful *relative to that index file's own mtime*. Git compares a
            // working-tree file against its entry by whole-second mtime plus size (sub-second
            // precision is a `USE_NSEC` build option that is off by default and off in every
            // mainstream distro's git), so a same-length edit landing in the same second as
            // the cached stat is indistinguishable from "unchanged" by stat alone. Git's
            // defence is the racy-index rule: an entry whose cached mtime is not strictly
            // older than the index file's own mtime is suspect, so its content gets re-read
            // instead of trusted.
            //
            // A fresh `NamedTempFile` carries *now* as its mtime, which silently moves every
            // copied entry out of that suspect window - git then trusts stat data the real
            // index would have rechecked, and a genuine same-length edit disappears from the
            // diff entirely. Carrying the source index's mtime across keeps the copy's
            // racy-index verdict identical to the real index's, so `git add` below observes
            // the same suspect entries git itself would and writes the shadow out with their
            // stat data invalidated. Best-effort: a filesystem that refuses the timestamp
            // update leaves the copy no worse than it was before this call existed, and
            // failing the whole diff over it would be a far bigger regression than the narrow
            // race it guards.
            if let Ok(mtime) = std::fs::metadata(&real_index_path).and_then(|meta| meta.modified())
            {
                let _ = shadow
                    .as_file()
                    .set_times(std::fs::FileTimes::new().set_modified(mtime));
            }
        }
        Err(_) => {
            // A repository whose index has never been written (e.g. immediately after
            // `git init`, before any commit), or one whose index momentarily can't be read
            // (a real interleaving hazard: an agent CLI's own `git add`/`git commit`
            // rewriting the index at the exact moment this reads it), has no real bytes to
            // seed the shadow copy with. `shadow_index_file` above already created a
            // real, empty *file* at this path though - and an empty-but-*existing* file is
            // not what git treats as "no index yet": confirmed directly (`GIT_INDEX_FILE`
            // pointed at a 0-byte file makes real `git add` fail outright with `fatal: ...
            // index file smaller than expected`, exit 128), where a path that simply
            // doesn't exist at all is instead treated as a fresh empty index and succeeds.
            // The placeholder is deleted below, once the handle is closed - `git add` then
            // creates a real index there from scratch, exactly as it would for a brand-new
            // repository's very first `git add`.
            real_index_missing = true;
        }
    }

    // Why the handle must be closed before `git add` ever runs, not just *where the file lives*:
    // a real, reported failure (`fatal: unable to write new index file`, two unrelated
    // repositories, this app running natively on Windows) traced to this function itself, not
    // anything external. `git add` writes an index by creating `<GIT_INDEX_FILE>.lock` and
    // renaming it over the destination - and on Windows, that rename can fail outright whenever
    // the destination has *any* open handle, even one opened with `FILE_SHARE_DELETE` (which
    // `tempfile`'s default sharing mode already is): confirmed against git's own upstream fix,
    // "compat/mingw: implement POSIX-style atomic renames over open files" (first shipped git
    // 2.48, Jan 2025) - before that release, on any git version, filesystem, or Windows edition,
    // this was a deterministic failure, not a transient one, because `shadow` above - a
    // `NamedTempFile` - was kept alive (and so its handle open) for the *entire* `git add` child
    // process below. Converting it to a [`tempfile::TempPath`] here closes that handle while
    // keeping the same drop-based cleanup; every real caller of this function only ever uses the
    // returned value's path, never its file content, so this is a pure signature narrowing, not
    // a behavior change for anyone downstream.
    let shadow = shadow.into_temp_path();
    if real_index_missing {
        // Now genuinely safe: with the handle already closed above, this is a real, immediate
        // delete - not the delete-*pending* state Windows leaves a still-open file in, which
        // would have left this exact path unusable for the `git add` below to recreate.
        std::fs::remove_file(&shadow).map_err(Error::WorktreeIo)?;
    }

    let mut add_args: Vec<OsString> = vec!["add".into()];
    match content {
        ShadowIndexContent::IntentToAdd => {
            add_args.push("--intent-to-add".into());
            add_args.push("-A".into());
        }
        ShadowIndexContent::FullContent => add_args.push("-A".into()),
        // `-u` restricts the update to paths git already tracks, so no untracked file's content
        // can reach the object database through this flavour - see its own docs.
        ShadowIndexContent::TrackedOnly => add_args.push("-u".into()),
    }
    add_args.extend([OsString::from("--"), ".".into()]);

    // A real, bounded retry remains here as defense-in-depth, *not* the fix above: even with our
    // own handle closed, something else on the user's machine can still transiently hold this
    // path open for a moment (real-time antivirus, a cloud-sync client watching the `.git`
    // directory) on a git version/filesystem combination that still takes the handle-sensitive
    // rename path. Scoped narrowly to that one failure text so a genuinely broken repository or a
    // real permissions error still surfaces immediately, on the first attempt, with no delay.
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

/// How many total attempts [`retry_transient_index_write_failure`] makes before giving up and
/// returning the real error - the first attempt plus this many retries.
const MAX_INDEX_WRITE_ATTEMPTS: u32 = 3;

/// The retry *policy* behind the git-directory placement's own docs above: retries `attempt_git`
/// only when it fails with [`is_transient_index_write_failure`], up to
/// [`MAX_INDEX_WRITE_ATTEMPTS`] total tries, with a short growing backoff between them. Every
/// other failure - a genuinely broken repository, a permissions error that will not resolve
/// itself - returns immediately on the first attempt; retrying those would only delay a real
/// error the user needs to see.
///
/// A free function taking a closure, rather than inlined into [`prepare_shadow_index`], so this
/// policy is independently testable: the real failure this exists for is a Windows-only
/// antivirus/rename race that this Linux dev environment cannot genuinely reproduce (confirmed by
/// direct testing - see this crate's own investigation notes), so what's verified here is that
/// the retry *logic itself* is correct (retries transient failures, stops immediately on
/// anything else, gives up after the bound), independent of ever reproducing the real OS
/// condition that triggers it.
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

/// Whether `stderr` names the one class of `git add` failure [`retry_transient_index_write_failure`]
/// retries - see that function's own docs for why only this one. Matched by substring, not the
/// full message: git's exact wording has varied by case across versions ("Unable"/"unable"), and
/// a narrower exact-string match would silently stop retrying the very failure this exists for
/// the moment a git upgrade rewords it.
fn is_transient_index_write_failure(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("unable to write new index file")
}

/// Strip a leading `a/` or `b/` diff prefix, and treat `/dev/null` as "no file". Prefixes
/// here are always exactly `a/`/`b/` *by construction*, not by assumption: `diff_against_base`
/// always invokes `git diff` with `-c diff.mnemonicPrefix=false -c diff.noprefix=false` (see
/// the module docs' "Explicit git config" section), which pins this regardless of the
/// caller's own git config - `diff.mnemonicPrefix=true` (a fairly common user setting) would
/// otherwise change these to `i/`/`w/`/`c/` and silently mislabel every file.
fn strip_diff_prefix(path: &str) -> Option<PathBuf> {
    if path == "/dev/null" {
        return None;
    }
    let stripped = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/"));
    Some(PathBuf::from(stripped.unwrap_or(path)))
}

/// A path as `git diff` printed it (after a `--- `/`+++ `/`rename from `/`rename to `
/// prefix, or the `and `/` differ` markers of a `Binary files ...` line): unquoted in the
/// common case. `diff_against_base` pins `-c core.quotePath=false` (see the module docs),
/// which stops git from C-style-quoting paths just for containing non-ASCII characters -
/// `core.quotePath`'s default (on) would otherwise render e.g. `café.txt` as the quoted,
/// octal-escaped `"caf\303\251.txt"`. That pin doesn't cover every case though: git still
/// quotes paths containing literal quote/backslash/control characters regardless of
/// `core.quotePath`, with backslash escapes inside the quotes. Only the surrounding quotes
/// are stripped here, not those inner escape sequences - a fully general C-style unquoter is
/// out of scope for a read-only diff *viewer*, so a quoted path with escapes shows its
/// escaped form rather than being misrendered as an invalid path.
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

/// Parse `git diff`'s unified-diff-format stdout into structured files/hunks/lines.
///
/// Returns the parsed files plus whether the [`MAX_FILES`] cap cut off any trailing files.
/// A file that hits [`MAX_HUNK_LINES_PER_FILE`] has its own `truncated` flag set instead
/// (which also folds into the overall [`WorktreeDiff::truncated`] by the caller).
///
/// Header-line detection (`rename from `/`rename to `/`new file mode`/`deleted file
/// mode`/`Binary files `/`--- `/`+++ `/`@@ `) only ever runs *outside* a hunk body
/// (`!in_hunk`). This matters because a hunk's body lines are file content with a single
/// `+`/`-`/` ` marker prepended, not escaped at all - a removed line whose own text happens
/// to start with `-- ` (a SQL-style comment, say) renders as `--- <that text>`, textually
/// indistinguishable from a `--- <path>` old-file header line (and likewise for `++
/// `/`+++ `). Naively checking these prefixes unconditionally (an earlier version of this
/// function did) both truncates the rest of that file's hunk and can misattribute the
/// change to a bogus "path" - silently mislabeling a change under the wrong filename, a
/// serious correctness bug for a tool whose purpose is reviewing changes before merge.
///
/// Knowing when a hunk body ends therefore can't rely on line-prefix heuristics; it comes
/// from the hunk header itself. A `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>]
/// @@` header declares exactly how many old-side and new-side body lines follow
/// ([`parse_hunk_counts`]); this function counts both down as body lines are consumed (a
/// context line decrements both, a removed line only the old count, an added line only the
/// new count) and only leaves "in a hunk body" once both hit zero.
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
                // Keep scanning is pointless once the cap is hit and there's nothing left to
                // flush into; every subsequent line is for a file we won't keep.
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
            // Preamble or trailing content outside any `diff --git` block; nothing to do
            // with it.
            continue;
        };
        if files.len() > MAX_FILES {
            continue;
        }

        if in_hunk {
            // Hunk body content - see this function's docs for why header-line prefixes are
            // never checked here, only the hunk's own declared old/new line counts.
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
            // A line starting with `\` (e.g. `\ No newline at end of file`) or anything else
            // unrecognized inside a hunk is silently skipped: cosmetic, not a line of real
            // content, and not counted against either remaining budget.
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
            // A degenerate zero-line hunk (shouldn't happen in real `git diff` output, but
            // don't get stuck "in a hunk" forever if it does) stays out of hunk-body mode.
            in_hunk = !(old_count == 0 && new_count == 0);
        }
    }
    flush(&mut current, &mut files);

    (files, files_truncated)
}

/// Parses the part of a `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@...`
/// hunk header after the opening `"@@ "` (i.e. `header` starts with `-`) into
/// `(old_count, new_count)`: how many old-side and new-side body lines this hunk declares,
/// per the unified diff format. A range without an explicit `,<count>` means a count of
/// exactly 1 (the unified diff spec's shorthand for a single-line range), which is why
/// [`parse_range_count`] discards the start-line value and only returns the count.
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

/// Best-effort fallback path extraction from a `diff --git a/<path> b/<path>` header line
/// (`rest` is everything after `"diff --git "`), used only when a file has no hunks, no
/// rename, and no binary marker to derive a path from otherwise (e.g. a pure file-mode
/// change). When the two sides are identical (the common case here, since a rename always
/// emits `rename from`/`rename to` lines instead), splitting on the last `" b/"` yields the
/// correct path regardless of any space elsewhere in it; there's a narrow, documented
/// ambiguity if the two sides genuinely differ *and* the path itself contains `" b/"`.
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
    use test_support::{git, seed_repo};

    #[test]
    fn on_default_branch_with_nothing_uncommitted_yields_an_empty_diff() {
        let repo = seed_repo();
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

    /// GitHub issue #108: a worktree left on its own default branch used to report
    /// `DiffBase::OnDefaultBranch` unconditionally, hiding real uncommitted edits from the
    /// Changes sidebar. `diff_against_base` must fall back to a real `git diff HEAD` instead.
    #[test]
    fn on_default_branch_with_real_uncommitted_changes_shows_them() {
        let repo = seed_repo();
        fs::write(repo.path().join("file.txt"), "hello\nedited on main\n").expect("write");
        fs::write(repo.path().join("new.txt"), "brand new\n").expect("write");

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        // `DiffBase::diff()` - the one accessor every real UI consumer should read through -
        // must surface this the same way it surfaces a real `Diff`.
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

    /// GitHub issue #163: the shadow index must not silently disable git's own racy-index
    /// protection.
    ///
    /// Git (built without `USE_NSEC`, which is the default and what every mainstream distro
    /// ships) compares a working-tree file against its index entry using **whole-second**
    /// mtime plus size. So an edit that keeps a file's length identical and lands in the same
    /// second as the stat data git cached for it is indistinguishable from "unchanged" by stat
    /// alone. Git's defence is the *racy index* rule: an entry whose cached mtime is not
    /// strictly older than the index file's **own** mtime is treated as suspect, and its
    /// content is re-read rather than trusted.
    ///
    /// That rule is defined relative to the mtime of the index file git actually read - which
    /// is exactly what [`prepare_shadow_index`] used to destroy: it copied the real index's
    /// bytes into a brand-new temp file, giving the copy a *fresh* mtime while leaving every
    /// cached entry's mtime untouched. Entries that were legitimately racy against the real
    /// index looked comfortably old against the copy, so git trusted the stale stat data and
    /// reported the file as unmodified - a real, same-length edit vanishing from the Changes
    /// panel entirely.
    ///
    /// This test forces that exact alignment deterministically (rather than waiting for the
    /// sub-second race to happen on its own, which is how it was originally found - as an
    /// intermittent failure of `app`'s own
    /// `switching_the_open_diff_to_a_different_file_recomputes_the_highlight_cache`): the
    /// file's cached mtime, its on-disk mtime and the real index's mtime are all pinned to one
    /// whole second, and the rewrite keeps the byte length identical.
    #[test]
    fn a_same_length_edit_racy_against_the_index_timestamp_is_still_reported() {
        use std::time::{Duration, SystemTime};

        // A whole second, safely in the past so nothing else can land on it by accident.
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
        // Pinned *before* `git add`, so this is the mtime git records in the index entry.
        pin_mtime(&path.join("a.rs"));
        git(path, &["add", "."]);
        git(path, &["commit", "-m", "initial"]);
        git(path, &["checkout", "-b", "feature"]);

        // The edit: byte-for-byte the same length as the committed content, and pinned back to
        // the same second the index entry already records.
        fs::write(path.join("a.rs"), "fn a() -> i32 {\n    2\n}\n").expect("rewrite a.rs");
        pin_mtime(&path.join("a.rs"));
        // The real index sits in the racy window too - which is what makes git's own
        // protection apply here, and therefore what the shadow copy must not throw away.
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
        let repo = seed_repo();
        fs::write(repo.path().join("keep.txt"), "keep\n").expect("write");
        git(repo.path(), &["add", "keep.txt"]);
        git(repo.path(), &["commit", "-m", "add keep.txt"]);

        git(repo.path(), &["checkout", "-b", "feature"]);
        // Modify a tracked file, add a new one, delete another - all left uncommitted, to
        // also prove uncommitted changes are included (per this module's documented `git
        // diff <merge-base>` choice).
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        let result = diff_against_base(repo.path()).expect("diff_against_base");
        let DiffBase::Diff(diff) = result else {
            panic!("expected DiffBase::Diff, got {result:?}");
        };
        assert!(diff.files.is_empty());
    }

    #[test]
    fn rename_is_detected() {
        let repo = seed_repo();
        // `-M`'s similarity threshold needs enough content to recognize a rename rather than
        // a delete+add; a single short line is not always enough, so pad the file.
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        // A removed line whose own text starts with `-- ` renders in unified diff output as
        // `--- <that text>` (marker `-` prepended to the real content) - textually identical
        // to a `--- <path>` old-file header line. Before this fix, the parser matched that
        // prefix unconditionally, truncating the rest of this hunk right after it.
        let repo = seed_repo();
        let content = "line one\n-- a real sql comment\nline three\nline four\n";
        fs::write(repo.path().join("file.txt"), content).expect("write");
        git(
            repo.path(),
            &["commit", "-am", "add sql-style comment line"],
        );

        git(repo.path(), &["checkout", "-b", "feature"]);
        // Delete the comment line, keep everything else - a real, ordinary edit.
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
        // The critical regression check: content *after* the `--- `-looking line must still
        // be present, proving the hunk wasn't truncated at that point.
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
        // An added line whose own text starts with `++ ` renders as `+++ <that text>` -
        // textually identical to a `+++ <path>` new-file header line. Before this fix, the
        // parser matched that prefix unconditionally, overwriting `new_path` with a bogus
        // path parsed out of the line's *content* - misattributing the change to the wrong
        // file, exactly the "worst case" scenario the checker flagged.
        let repo = seed_repo();
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
        // Must NOT have been misattributed to "evil.txt" (or any path derived from the
        // line's content) - there must be exactly one changed file, and it must be the real
        // one.
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
        // A real bare repository standing in for a remote, with a "main" branch that isn't
        // named `main`/`master` in a way local detection alone could ever fall into by
        // accident - it's deliberately named something else so this test can only pass by
        // actually following `refs/remotes/origin/HEAD`, not by coincidentally matching the
        // local-branch fallback strategies.
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
        // `git clone` already sets up `refs/remotes/origin/HEAD` as a symbolic ref to the
        // remote's default branch; this is exactly the case strategy 1 targets.
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
        // `git diff <merge-base>` alone never sees genuinely untracked (never `git add`ed)
        // files, no matter what - the single most common thing an agent produces in a fresh
        // worktree. `prepare_shadow_index`'s `--intent-to-add` trick is what makes this
        // show up at all.
        let repo = seed_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("brand_new.txt"), "content nobody staged\n").expect("write");
        // Deliberately NOT `git add`ed.

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

        // Confirm the real index was never touched by this: a plain `git status` still
        // reports the file as untracked (`??`), never staged.
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
        // Reproduces a real, confirmed bug: `prepare_shadow_index` used to fall back to an
        // empty *existing* temp file when the real index couldn't be read, on the (wrong)
        // assumption that this "mirrors git's own missing index == empty index behavior."
        // Verified directly with real git commands that it does not: `GIT_INDEX_FILE`
        // pointed at a genuinely nonexistent path is treated as a fresh empty index (real
        // `git add` succeeds), but pointed at an existing 0-byte file it fails outright
        // (`fatal: ... index file smaller than expected`, exit 128) - a completely
        // different, fatal code path. `diff_against_base` surfaced that fatal to the whole
        // Changes panel as "failed to compute diff: ...".
        let repo = seed_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("brand_new.txt"), "content nobody staged\n").expect("write");

        // Simulate the real index being genuinely unreadable at the moment
        // `prepare_shadow_index` reads it - a fresh worktree with no index written yet, or
        // (the more likely real-world trigger) an agent CLI's own concurrent `git add`/
        // `git commit` rewriting it in the same window. `git rev-parse --git-path index`
        // matches exactly what `prepare_shadow_index` itself resolves.
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

    /// The shadow index must be created inside the worktree's own git directory, next to the
    /// real index - never in `std::env::temp_dir()`. See `prepare_shadow_index`'s own "Where the
    /// shadow index file lives" docs: `git add` creates `<GIT_INDEX_FILE>.lock` beside this file
    /// and renames it over the top, so this directory is exactly the one git must be able to
    /// write and rename within, and the OS temp directory is a real, environment-specific
    /// liability there (a `TMPDIR` on another mount, a sandboxed private `/tmp`, a full temp
    /// filesystem, an age-based cleanup daemon).
    ///
    /// This is a stronger assertion than "diffing still works with a hostile `TMPDIR`", and is
    /// the reason no test here mutates `TMPDIR`: `std::env::set_var` is process-global, and this
    /// test binary runs its tests in parallel threads that use `tempfile` (`TempDir::new` in
    /// every `init_repo` call) throughout, so pointing the whole process's temp directory
    /// somewhere unusable mid-run would corrupt unrelated tests rather than prove anything about
    /// this one. Asserting the real parent directory proves the property directly instead.
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

    /// The whole reason this retry exists: a transient antivirus-scan-holds-the-lock-file race on
    /// Windows resolves itself within milliseconds, so a second attempt moments later succeeds
    /// without the caller ever seeing an error - exactly the "the write is safe to redo" property
    /// the retry's own docs claim.
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

    /// A failure that never clears (e.g. a genuinely locked file, or antivirus that never lets go)
    /// must still surface as a real error once the bound is reached - this is a *bounded* retry,
    /// not an infinite one that could hang the whole diff computation.
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

    /// The whole point of scoping the retry to one specific failure text: a genuinely broken
    /// repository (or any other real `git add` failure) must never be retried or delayed - it is
    /// not going to resolve itself, and the user needs to see it immediately.
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

    /// The real, root-cause fix (not the retry, which is defense-in-depth only): `git add` must
    /// never see an open handle on the shadow index's path when it runs, because Windows can
    /// refuse to rename a lock file over a destination with any open handle at all (see
    /// `prepare_shadow_index`'s own "Why the handle must be closed" docs for the exact upstream
    /// git behavior this traces to, and the two real, unrelated repositories that hit it).
    /// `/proc/self/fd` is a real, direct way to check this invariant on Linux even though the
    /// failure mode itself is Windows-only: this process must hold no open file descriptor
    /// pointing at the shadow index's path by the time [`prepare_shadow_index`] returns - if one
    /// still exists, this test proves the regression this fix closes, regardless of which
    /// platform is running it.
    #[test]
    fn prepare_shadow_index_closes_its_own_file_handle_before_returning() {
        let repo = seed_repo();
        fs::write(repo.path().join("untracked.txt"), "nobody staged this\n").expect("write");

        let shadow = prepare_shadow_index(repo.path(), ShadowIndexContent::IntentToAdd)
            .expect("prepare_shadow_index");
        let shadow_path = fs::canonicalize(&*shadow).expect("canonicalize shadow path");

        let fd_dir = Path::new("/proc/self/fd");
        if !fd_dir.is_dir() {
            // Not Linux (or no procfs) - the invariant this checks is real everywhere, but this
            // particular check has no portable equivalent; the git-directory-placement test
            // above and the retry-policy tests still cover this function on every platform.
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
        let repo = seed_repo();
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

        // Dropping it really deletes it - the `NamedTempFile` cleanup contract is unchanged by
        // the new parent directory, so a `.git` directory doesn't slowly fill with these.
        let path = shadow.to_path_buf();
        assert!(path.exists());
        drop(shadow);
        assert!(
            !path.exists(),
            "the shadow index must still be cleaned up on drop now that it lives under .git"
        );
    }

    /// A linked worktree has its *own* private git directory
    /// (`<common-dir>/worktrees/<name>`), which is where its own index lives - the shadow index
    /// must land there, not in the main checkout's `.git`, so the two never contend and the
    /// same-filesystem guarantee holds for a worktree created anywhere on disk.
    #[test]
    fn a_linked_worktrees_shadow_index_lives_in_that_worktrees_own_git_directory() {
        let repo = seed_repo();
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

    /// The real, reported failure's own shape: a repository with a genuine embedded git
    /// repository (its own `.git` directory, with its own commit) sitting untracked inside the
    /// worktree - which is exactly this app's own dogfooding checkout, where `vendor/zed` is a
    /// real vendored clone. `git add -A` prints a loud multi-line "adding embedded git
    /// repository" warning on stderr for it, and `compute_diff` must still succeed: that warning
    /// is stderr noise on a *successful* command, not a failure, and nothing in the diff pipeline
    /// may treat it as one.
    #[test]
    fn an_embedded_git_repository_in_the_worktree_does_not_fail_the_diff() {
        let repo = seed_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(repo.path().join("brand_new.txt"), "content nobody staged\n").expect("write");

        // A real nested repository with a real commit of its own - an embedded repo with *no*
        // commit checked out is a different case entirely (`git add` fails it outright with
        // "does not have a commit checked out"), and is not what the report showed.
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

        // And the real index is still untouched - the embedded repo is still untracked.
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
        let repo = seed_repo();
        fs::write(repo.path().join("keep.txt"), "keep\n").expect("write");
        git(repo.path(), &["add", "keep.txt"]);
        git(repo.path(), &["commit", "-m", "add keep.txt"]);
        git(repo.path(), &["checkout", "-b", "feature"]);

        // Committed since branch point.
        fs::write(repo.path().join("keep.txt"), "keep\ncommitted\n").expect("write");
        git(repo.path(), &["commit", "-am", "commit on feature"]);
        // Uncommitted (staged) change.
        fs::write(repo.path().join("file.txt"), "hello\nstaged\n").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        // Untracked, never staged.
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        git(repo.path(), &["checkout", "-b", "feature"]);
        // No new commits on `feature`: it's exactly `main`, so it's trivially "merged".
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
        let repo = seed_repo();
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
        // Unborn HEAD: nothing committed yet.
        let status = merge_status_against_base(dir.path()).expect("merge_status_against_base");
        assert_eq!(status, None);
    }

    #[test]
    fn merge_status_on_the_default_branch_itself_is_trivially_merged() {
        let repo = seed_repo();
        let status = merge_status_against_base(repo.path())
            .expect("merge_status_against_base")
            .expect("main itself has a detectable base (itself)");
        assert_eq!(status.base_branch, "main");
        assert!(status.merged);
    }

    /// Both real "nothing has diverged" states: the default branch itself (whose base is
    /// itself), and a branch just cut from it.
    #[test]
    fn a_branch_that_has_not_diverged_is_zero_ahead_and_zero_behind() {
        for branch in [None, Some("feature")] {
            let repo = seed_repo();
            if let Some(branch) = branch {
                git(repo.path(), &["checkout", "-b", branch]);
            }
            let result = ahead_behind_against_base(repo.path())
                .expect("ahead_behind_against_base")
                .unwrap_or_else(|| panic!("{branch:?} must have a detectable base"));
            assert_eq!(
                result,
                AheadBehind {
                    ahead: 0,
                    behind: 0
                },
                "{branch:?} has no commits of its own and none it is missing"
            );
        }
    }

    #[test]
    fn ahead_behind_unborn_head_is_none() {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        let result = ahead_behind_against_base(dir.path()).expect("ahead_behind_against_base");
        assert_eq!(result, None);
    }

    /// Real, independently diverged histories on each side: two commits ahead on `feature`
    /// after branching, then one commit added to `main` afterward - the exact shape the status
    /// bar's `↑2 ↓0`-style indicator needs to be able to show a non-zero `behind` too, not just
    /// `ahead`.
    #[test]
    fn ahead_behind_counts_real_diverged_commits_on_each_side() {
        let repo = seed_repo();
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

    /// The audit's exact reproduction of the wrong-ref bug: the detected base comes from
    /// `refs/remotes/origin/HEAD` (short name `main`), but a *local* `main` branch of the same
    /// short name also exists and has gone stale relative to `origin/main`. Before this fix,
    /// `ahead_behind_against_base` handed git the bare short name `main`, which git's own
    /// disambiguation rules resolve against the stale local branch (`refs/heads/main`) rather
    /// than the fresh remote-tracking commit that was actually detected as the real base -
    /// silently reporting `↓0` (up to date) when the real, correct answer is `↓2`.
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
        // A real feature branch, checked out from `main` at the clone-time commit.
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

    /// If `rev-list`'s output is ever not in the expected `<behind> <ahead>` shape, this must
    /// report an honest "unknown" (`None`) rather than fabricating a confident-looking
    /// `{ahead: 0, behind: 0}` via `.unwrap_or(0)`. Exercises the exact real parsing function
    /// [`ahead_behind_against_base`] itself calls on `rev-list`'s real stdout, not a
    /// reimplementation that could drift from it.
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
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
        let repo = seed_repo();
        let commits = commits_since_base(repo.path()).expect("commits_since_base");
        assert!(commits.commits.is_empty());
        assert_eq!(commits.base_branch, None);
        assert_eq!((commits.added, commits.removed), (0, 0));
    }

    #[test]
    fn a_commit_subject_carrying_the_numstat_separator_is_still_parsed_as_one_commit() {
        // The record/field separators exist so a subject cannot be mistaken for a field boundary
        // or for the start of a numstat line.
        let repo = seed_repo();
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
        let repo = seed_repo();
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
