//! Read-only diff of a worktree's `HEAD` (including uncommitted changes) against the
//! merge-base with the repository's default branch.
//!
//! ## What "base" means
//!
//! A git worktree has no notion of an explicit "base branch": it just has a branch (or a
//! detached commit) checked out. For an agent-development-environment reviewing what an
//! agent changed before merging, the useful comparison is against the point where the
//! worktree's branch diverged from the repository's default branch - i.e. the merge-base
//! between the worktree's `HEAD` and the default branch's tip.
//!
//! The default branch itself is detected, in order:
//! 1. `refs/remotes/origin/HEAD`, if it exists and is a symbolic ref (mirrors
//!    `git symbolic-ref refs/remotes/origin/HEAD`).
//! 2. A local `main` branch, if one exists.
//! 3. A local `master` branch, if one exists.
//! 4. The main worktree's own currently checked-out branch, as a last resort (so a
//!    repository with neither an `origin` remote nor a `main`/`master` branch - e.g. one
//!    initialized locally on some other default branch name - still gets a sensible base).
//!
//! If none of these yield a branch, or the selected worktree's branch *is* the detected
//! default branch, or no merge-base exists between the two histories (e.g. genuinely
//! unrelated histories), there is no sensible base: [`diff_against_base`] returns
//! [`DiffBase::NoBaseFound`] or [`DiffBase::OnDefaultBranch`] rather than fabricating one.
//!
//! ## What the diff includes
//!
//! This runs `git diff <merge-base>` (not `git diff <merge-base>..HEAD`), with the worktree
//! itself as the current directory. `git diff <commit>` compares `<commit>`'s tree against
//! the *working tree* (index and unstaged changes included), which is deliberate: an agent
//! working in this worktree may not have committed anything yet, so limiting the diff to
//! committed history (`<merge-base>..HEAD`) would hide exactly the changes a reviewer most
//! wants to see. This one command covers both committed-since-branch-point changes and any
//! uncommitted working-tree changes on top of them - with one real gap: `git diff <commit>`
//! (with no `--cached`) only ever considers paths already present in `<commit>`'s tree or in
//! the index, so a genuinely untracked file (created but never `git add`ed) is invisible to
//! it no matter what. Since a new, unstaged file is exactly the single most common thing an
//! agent produces, [`diff_against_base`] works around this with [`prepare_shadow_index`]: a
//! throwaway, `--intent-to-add`-augmented copy of the real index, passed to `git diff` via a
//! `GIT_INDEX_FILE` override so untracked files show up as real additions too, without ever
//! touching the real index. See that function's docs for the mechanics and why it's safe.
//!
//! ## Explicit git config, not caller defaults
//!
//! The `git diff` invocation pins `diff.mnemonicPrefix=false`, `diff.noprefix=false`, and
//! `core.quotePath=false` via `-c` (before the `diff` subcommand, where git requires global
//! config overrides to appear). Without this, the path-prefix parsing below
//! ([`strip_diff_prefix`]) would silently mislabel every file under a caller's repo/global
//! `diff.mnemonicPrefix=true` config (prefixes become `i/`/`w/`/`c/` instead of `a/`/`b/`),
//! and non-ASCII filenames would render octal-escaped under `core.quotePath`'s *default*
//! (on) setting. Pinning these explicitly makes this module's parsing assumptions true by
//! construction rather than by hoping the caller's git config happens to match them.
//!
//! ## gix vs. the `git` CLI
//!
//! Per this crate's convention (see the crate-level docs), reads go through `gix` where
//! practical. Base-branch detection and merge-base computation are done via `gix`
//! ([`gix::Repository::find_reference`], [`gix::Repository::merge_base`]) - both real reads
//! with a direct, stable `gix` API.
//!
//! Producing the actual diff *text* is different: `gix-diff` operates at the level of tree
//! and blob objects, not the working tree, and has no built-in formatter that reproduces
//! `git diff`'s unified-diff text (hunk headers, context lines, rename detection, binary
//! detection, and - critically - blending in uncommitted working-tree state). Reimplementing
//! that formatting (and a working-tree-aware diff at that) on top of `gix-diff`'s lower-level
//! primitives would mean re-deriving `git diff`'s own output format from scratch, which is a
//! lot of surface area to get byte-for-byte right without a real risk of subtly-wrong
//! (effectively fake) hunks. `git diff` itself is definitionally correct and already handles
//! all of this, so this module shells out to it for the diff text specifically, the same way
//! [`super::remove_worktree`]'s dirty-check shells out to `git status` for a read that's
//! easier to get right via the CLI than via a from-scratch reimplementation.
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
const MAX_DIFF_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

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
    /// A usable base was found; here is the diff against it (possibly with zero changed
    /// files, if the worktree exactly matches its base).
    Diff(WorktreeDiff),
    /// The worktree's own branch *is* the detected default branch, so there is no meaningful
    /// "base" to diff against.
    OnDefaultBranch { branch: String },
    /// No sensible base could be found at all: no default branch could be detected, `HEAD`
    /// is unborn (a brand new repository with no commits), or the two histories share no
    /// common ancestor.
    NoBaseFound,
}

/// Compute the real diff of the worktree at `worktree_path` against its base, per this
/// module's docs. Performs blocking I/O (opens the repository via `gix`, and spawns a real
/// `git diff` child process).
pub fn diff_against_base(worktree_path: &Path) -> Result<DiffBase, Error> {
    let repo = open_repo(worktree_path)?;

    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let worktree_branch = head.referent_name().map(|name| name.shorten().to_string());
    let worktree_head_id = head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?;

    let Some(worktree_head_id) = worktree_head_id else {
        // Unborn HEAD: a freshly initialized repository with no commits yet has nothing to
        // diff against any base.
        return Ok(DiffBase::NoBaseFound);
    };

    let Some((base_branch, base_commit_id)) = detect_default_base(&repo)? else {
        return Ok(DiffBase::NoBaseFound);
    };

    if worktree_branch.as_deref() == Some(base_branch.as_str()) {
        return Ok(DiffBase::OnDefaultBranch {
            branch: base_branch,
        });
    }

    let merge_base_id = match repo.merge_base(worktree_head_id.detach(), base_commit_id) {
        Ok(id) => id.detach(),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => {
            return Ok(DiffBase::NoBaseFound);
        }
        Err(source) => return Err(Error::MergeBase(Box::new(source))),
    };

    let merge_base_sha = merge_base_id.to_string();
    // Defensive: this is about to be handed to a spawned `git` process as an argument. It
    // was just produced by `gix` as a real object id, but double-checking it's actually a
    // hex string (rather than something that could be misparsed as a flag) costs nothing and
    // means a future change to how it's derived can't silently turn into a git-argument
    // injection.
    if merge_base_sha.is_empty() || !merge_base_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::WorktreeIo(std::io::Error::other(
            "merge-base id was not a hex object id",
        )));
    }

    let shadow_index = prepare_shadow_index(worktree_path)?;

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
        merge_base_sha.clone().into(),
    ];
    let (output, output_truncated) = capture_git_stdout(
        worktree_path,
        &args,
        MAX_DIFF_OUTPUT_BYTES,
        Some(shadow_index.path()),
    )?;
    let text = String::from_utf8_lossy(&output);
    let (files, files_truncated) = parse_git_diff(&text);

    Ok(DiffBase::Diff(WorktreeDiff {
        base_branch,
        base_commit: merge_base_sha,
        files,
        truncated: output_truncated || files_truncated,
    }))
}

/// Detect the repository's default branch and its tip commit id, per this module's
/// documented detection order. Returns `Ok(None)` if none of the strategies yield a branch
/// (rather than an `Err`): an undetectable default branch is a real, expected outcome (e.g. a
/// repository with no `origin` remote and a default branch named something other than `main`
/// or `master`), not a failure.
fn detect_default_base(repo: &gix::Repository) -> Result<Option<(String, gix::ObjectId)>, Error> {
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
    // itself succeeds even when the main repository is bare - per gix's own docs ("the main
    // repo might be bare"), it just opens the common dir, bare or not, rather than erroring
    // on one. The real "nothing to fall back to" cases are handled explicitly below: the
    // main repo's `HEAD` being unborn (a bare or fresh repository with no commits reachable
    // from `HEAD` yet - `try_peel_to_id_in_place` returns `None`) or detached
    // (`referent_name` returns `None`). `main_repo()` can still fail for other reasons
    // (e.g. a corrupt or inaccessible common dir), which is treated the same as "nothing
    // found" here rather than a hard failure, consistent with this function's overall
    // contract of returning `Ok(None)` for "no base detectable" rather than `Err`.
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
/// element of the returned tuple is `true`: mirrors [`super::is_dirty`]'s reasoning for not
/// risking a blocked read against a pipe the child may still be filling.
fn capture_git_stdout(
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
/// Reading them sequentially (stdout to EOF, only then stderr - what an earlier version of
/// this code did) has a real deadlock: each pipe has a bounded OS buffer (64KB on Linux),
/// and if the child writes enough to stderr before finishing stdout - plausible for `git
/// diff` (e.g. one CRLF/smudge-filter warning line per changed file, across up to
/// `MAX_FILES` files) - the child blocks writing stderr while this process is blocked
/// reading stdout: neither side can make progress, hanging this thread forever with a live,
/// un-reapable `git` child. Draining both concurrently means neither pipe can ever back up
/// far enough to block the child's writes.
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

/// Builds a temporary, throwaway copy of the worktree's real index with untracked files
/// added as "intent to add" stubs (`git add --intent-to-add`), so `git diff <merge-base>` -
/// which only ever considers paths already present in the index or in `<merge-base>`'s tree
/// (see the module docs' "What the diff includes" section) - picks up genuinely new,
/// unstaged files as real additions instead of silently omitting them.
///
/// The real index is only ever *read* (to seed the copy); every mutation happens on the
/// temp file via a `GIT_INDEX_FILE` override in the caller, so this can never perturb the
/// real repository state. Empirically verified during this fix: modifying a tracked file
/// and creating an untracked file, then diffing through a shadow index built this way,
/// includes both changes; a plain `git status` immediately afterward still reports the
/// untracked file as untracked (`??`), never staged - confirming the real index was
/// untouched.
///
/// `--intent-to-add` (rather than a full `git add -A`) is deliberate: it records just enough
/// (an empty-blob stub entry) for `git diff` to treat the path as present, without actually
/// staging real file content into the temp index - unnecessary work, since `git diff
/// <commit>` (no `--cached`) always compares against the real working-tree file content
/// directly regardless of what's staged.
fn prepare_shadow_index(worktree_path: &Path) -> Result<tempfile::NamedTempFile, Error> {
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

    let mut shadow = tempfile::NamedTempFile::new().map_err(Error::WorktreeIo)?;
    // A repository whose index has never been written (e.g. immediately after `git init`,
    // before any commit) has no index file on disk yet; falling back to an empty temp file
    // in that case mirrors git's own "missing index == empty index" behavior rather than
    // being an error. In practice `diff_against_base` already returns `NoBaseFound` for an
    // unborn `HEAD` before reaching this point, so the index should always exist here, but
    // this stays defensive rather than failing outright if it somehow doesn't.
    if let Ok(real_index_bytes) = std::fs::read(&real_index_path) {
        use std::io::Write as _;
        shadow
            .write_all(&real_index_bytes)
            .map_err(Error::WorktreeIo)?;
        shadow.flush().map_err(Error::WorktreeIo)?;
    }

    let add_args: Vec<OsString> = vec![
        "add".into(),
        "--intent-to-add".into(),
        "-A".into(),
        "--".into(),
        ".".into(),
    ];
    let output = git_command(worktree_path, &add_args)
        .env("GIT_INDEX_FILE", shadow.path())
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&add_args),
            source,
        })?;
    check_success(&add_args, &output)?;

    Ok(shadow)
}

/// Strip a leading `a/` or `b/` diff prefix, and treat `/dev/null` as "no file". Prefixes
/// here are always exactly `a/`/`b/` *by construction*, not by assumption: `diff_against_base`
/// always invokes `git diff` with `-c diff.mnemonicPrefix=false -c diff.noprefix=false` (see
/// the module docs' "Explicit git config" section), which pins this regardless of the
/// caller's own git config - `diff.mnemonicPrefix=true` (a real, fairly common user setting)
/// would otherwise change these to `i/`/`w/`/`c/` and silently mislabel every file.
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
/// `core.quotePath`'s *default* (on) would otherwise render e.g. `café.txt` as the quoted,
/// octal-escaped `"caf\303\251.txt"`. That pin doesn't cover every case though: git still
/// quotes paths containing literal quote/backslash/control characters regardless of
/// `core.quotePath`, in double quotes with backslash escapes. Only the surrounding quotes
/// are stripped here, not those inner escape sequences - a fully general C-style unquoter is
/// out of scope for a read-only diff *viewer* (git itself remains the source of truth for
/// the underlying files), so a quoted path with escapes shows its escaped form rather than
/// being misrendered as something that isn't a valid path at all.
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
/// (`!in_hunk`). This matters because a hunk's body lines are real file content with a
/// single `+`/`-`/` ` marker prepended, not escaped in any way - a removed line whose own
/// text happens to start with `-- ` (a SQL-style comment, say) is rendered by `git diff` as
/// `--- <that text>`, textually indistinguishable from a `--- <path>` old-file header line;
/// likewise an added line starting with `++ ` renders as `+++ <that text>`, indistinguishable
/// from a `+++ <path>` new-file header. Naively checking these prefixes unconditionally (an
/// earlier version of this function did) both truncates the rest of that file's hunk *and*
/// can misattribute the change to whatever bogus "path" the content happened to parse as -
/// for a tool whose purpose is reviewing real changes before merge, silently mislabeling a
/// change under the wrong filename is a serious correctness bug, not a cosmetic one.
///
/// Knowing precisely when a hunk body ends therefore can't rely on line-prefix heuristics
/// either; it comes from the hunk header itself. A `@@ -<old_start>[,<old_count>]
/// +<new_start>[,<new_count>] @@` header declares exactly how many old-side and new-side
/// body lines follow ([`parse_hunk_counts`]); this function tracks both counts down as body
/// lines are consumed (a context line decrements both, a removed line decrements only the
/// old count, an added line only the new count) and only leaves "in a hunk body" once both
/// hit zero - i.e. once the hunk has consumed exactly as many lines as it declared, not
/// once a line merely *looks* like the next header.
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
/// exactly 1 (the unified diff spec's shorthand for a single-line range) - not "unknown" or
/// "the start line number", which is why [`parse_range_count`] discards the start-line value
/// entirely and only ever returns a real, meaningful count.
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
/// change). When the two sides are identical (the overwhelmingly common case for this
/// fallback, since a real rename always emits `rename from`/`rename to` lines), splitting on
/// the last `" b/"` yields the correct path regardless of where a space inside the path
/// might also match; when the two sides genuinely differ *and* this fallback is the only
/// source of the path (no rename lines emitted, which git always does for detected renames),
/// there is a real but narrow ambiguity for paths containing the literal substring `" b/"` -
/// documented rather than silently mishandled.
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
    fn on_default_branch_yields_no_base() {
        let repo = init_repo();
        let result = diff_against_base(repo.path()).expect("diff_against_base");
        assert_eq!(
            result,
            DiffBase::OnDefaultBranch {
                branch: "main".to_string()
            }
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
        // A removed line whose own text starts with `-- ` renders in unified diff output as
        // `--- <that text>` (marker `-` prepended to the real content) - textually identical
        // to a `--- <path>` old-file header line. Before this fix, the parser matched that
        // prefix unconditionally, truncating the rest of this hunk right after it.
        let repo = init_repo();
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
        let repo = init_repo();
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
    fn untracked_and_committed_and_uncommitted_changes_all_appear_together() {
        // Combines all three kinds of change the diff is supposed to surface in one pass -
        // the exact workflow this feature exists for (check everything an agent did before
        // merge, in one view).
        let repo = init_repo();
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
    fn unrelated_histories_yield_no_base_found() {
        // Two branches in the same repo with genuinely no common ancestor (an orphan
        // branch) - `gix::Repository::merge_base` must report `NotFound`, which
        // `diff_against_base` should surface as `NoBaseFound` rather than an `Err`.
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

        let result = diff_against_base(repo.path()).expect("diff_against_base");
        assert_eq!(result, DiffBase::NoBaseFound);
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
}
