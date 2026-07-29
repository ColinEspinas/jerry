//! `git merge` of a session's worktree branch into the repository's detected default
//! ("base") branch: attempting the merge, detecting conflicts from git's own conflict
//! markers, and resolving them (take-left/take-right/take-both) by writing content back to
//! disk and staging it.
//!
//! ## Worktree-checkout collision
//!
//! A git worktree has its own `HEAD`, but the *object database* (refs included) is shared
//! across every worktree of a repository. The same branch can never be checked out in two
//! worktrees at once - `git checkout <branch checked out elsewhere>` fails outright with
//! `fatal: '<branch>' is already used by worktree at '<path>'`. That rules out checking out
//! the base branch somewhere temporary (or into the session's own worktree) and merging
//! there.
//!
//! `git merge <branch-name>` merges a *ref*, not a directory, so it can instead be run from
//! any worktree that already has the *target* branch checked out. [`attempt_merge`] finds
//! the worktree with the detected base branch checked out (via [`crate::list_worktrees`])
//! and runs `git -C <that worktree> merge <session-branch-name>` there - never a `git
//! checkout`. If no worktree has the base branch checked out at all, this is refused with
//! [`Error::MergeBaseBranchNotCheckedOut`] rather than fabricating a checkout.
//!
//! ## `--no-commit --no-ff`
//!
//! `git merge --no-commit <branch>` still auto-commits a fast-forward merge on its own
//! (`git-merge(1)`: "fast-forward updates ... there is no way to stop those merges with
//! `--no-commit`"), which would defeat pausing before committing on *every* outcome so the
//! UI can show what happened first. Adding `--no-ff` forces a real merge commit even when a
//! fast-forward is possible, so combined with `--no-commit` nothing is ever auto-committed
//! (verified in the tests below across fast-forwardable, three-way, and conflicting merges).
//! The one exception is "already up to date": `git merge` exits `0` but never creates
//! `MERGE_HEAD` at all, which is why [`attempt_merge`] checks for `MERGE_HEAD` explicitly
//! rather than trusting exit status alone (see [`MergeOutcome::AlreadyUpToDate`]).
//!
//! `merge.conflictStyle=merge` is pinned via `-c` (same convention `crate::diff` uses for
//! `git diff`), so a conflicted file's markers are always the two-way
//! `<<<<<<</=======/>>>>>>>` form this module's parser understands, never `diff3`-style
//! (`|||||||` base section) - regardless of the caller's own git config.
//!
//! ## Not auto-committing
//!
//! Neither a clean merge nor a fully-resolved conflicted merge is auto-committed here;
//! [`complete_merge`] is a separate, explicit step for both, so the repository stays in a
//! staged-but-uncommitted state until the UI shows it and the user confirms.
//!
//! Performs blocking I/O everywhere in this module (shells out to `git`); see the
//! crate-level docs on offloading this to a background thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::diff::detect_default_base;
use crate::error::{Error, GitExit};
use crate::{check_success, format_args, git_command, is_dirty, list_worktrees, open_repo};

/// Where a merge attempt happened, and against what - returned alongside [`MergeOutcome`]
/// so a caller never has to re-derive "which worktree did this run in" or "what was the
/// base branch" from scratch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeStart {
    pub base_branch: String,
    pub base_worktree_path: PathBuf,
    pub session_branch: String,
}

/// The real result of one `git merge --no-commit --no-ff` attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The base branch already contains every commit on the session branch; `git merge`
    /// exited successfully but created no `MERGE_HEAD` and nothing changed on disk.
    AlreadyUpToDate,
    /// The merge completed with no conflicts and is staged, uncommitted, in the base
    /// worktree's index and working tree - [`complete_merge`] finishes it.
    Clean { files: Vec<PathBuf> },
    /// The merge produced real conflicts in one or more files. `clean_files` merged without
    /// any human input (git resolved them automatically because the edits don't overlap);
    /// `conflicted_files` each contain real `<<<<<<</=======/>>>>>>>` markers on disk and need
    /// [`load_conflicted_file`] + resolution before [`complete_merge`] can run.
    Conflicted {
        conflicted_files: Vec<PathBuf>,
        clean_files: Vec<PathBuf>,
    },
}

/// Attempt a real merge of the branch checked out in `session_worktree_path` into the
/// repository's detected default/base branch, per this module's docs. `repo_path` is used
/// only to open the repository and detect the base branch (any worktree path of the
/// repository works for that); the merge itself always runs in the worktree that has the
/// base branch checked out, which [`attempt_merge`] finds on its own.
///
/// Before running `git merge`, this refuses (returns [`Error::MergeTargetDirty`]) if the base
/// worktree has any uncommitted changes at all - `git merge` itself often refuses this too,
/// but checking first means a caller gets one consistent, structured error instead of having
/// to parse `git`'s own stderr to tell "refused, dirty" apart from other real failures.
///
/// Performs blocking I/O: opens the repository via `gix`, spawns `git worktree`/`git status`
/// reads, and spawns the real `git merge` child process.
pub fn attempt_merge(
    repo_path: &Path,
    session_worktree_path: &Path,
) -> Result<(MergeStart, MergeOutcome), Error> {
    let session_repo = open_repo(session_worktree_path)?;
    let session_head = session_repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let session_branch = session_head
        .referent_name()
        .map(|name| name.shorten().to_string());
    let Some(session_branch) = session_branch else {
        return Err(Error::MergeSourceDetached {
            path: session_worktree_path.to_path_buf(),
        });
    };

    let repo = open_repo(repo_path)?;
    let Some((base_branch, _base_commit_id)) = detect_default_base(&repo)? else {
        return Err(Error::MergeNoBaseBranch);
    };

    if session_branch == base_branch {
        return Err(Error::MergeSourceIsBaseBranch {
            branch: base_branch,
        });
    }

    let base_worktree_path =
        find_worktree_with_branch(repo_path, &base_branch)?.ok_or_else(|| {
            Error::MergeBaseBranchNotCheckedOut {
                branch: base_branch.clone(),
            }
        })?;

    if is_dirty(&base_worktree_path)? {
        return Err(Error::MergeTargetDirty {
            path: base_worktree_path,
        });
    }

    let start = MergeStart {
        base_branch,
        base_worktree_path: base_worktree_path.clone(),
        session_branch: session_branch.clone(),
    };

    let args: Vec<OsString> = vec![
        "-c".into(),
        "merge.conflictStyle=merge".into(),
        "merge".into(),
        "--no-commit".into(),
        "--no-ff".into(),
        "--".into(),
        session_branch.into(),
    ];
    let mut command = git_command(&base_worktree_path, &args);
    let output = command.output().map_err(|source| Error::GitSpawn {
        args: format_args(&args),
        source,
    })?;

    if output.status.success() {
        if !merge_head_exists(&base_worktree_path)? {
            return Ok((start, MergeOutcome::AlreadyUpToDate));
        }
        let files = touched_files(&base_worktree_path)?;
        return Ok((start, MergeOutcome::Clean { files }));
    }

    let conflicted_files = conflicted_files(&base_worktree_path)?;
    if conflicted_files.is_empty() {
        // A real, non-conflict failure (e.g. a merge was already in progress, or something
        // else genuinely went wrong) - surface git's own stderr rather than misreporting an
        // empty conflict set.
        return Err(Error::GitCommand {
            args: format_args(&args),
            exit: GitExit::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let touched = touched_files(&base_worktree_path)?;
    let clean_files = touched
        .into_iter()
        .filter(|f| !conflicted_files.contains(f))
        .collect();

    Ok((
        start,
        MergeOutcome::Conflicted {
            conflicted_files,
            clean_files,
        },
    ))
}

/// Abort an in-progress merge (real `git merge --abort`) in `base_worktree_path`, restoring
/// it to exactly the state it was in before [`attempt_merge`] ran: no `MERGE_HEAD`, no
/// conflict markers, no staged changes from the merge attempt.
///
/// Performs blocking I/O.
pub fn abort_merge(base_worktree_path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["merge".into(), "--abort".into()];
    let output = git_command(base_worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)
}

/// Finish an in-progress merge in `base_worktree_path` with `git commit --no-edit`, using
/// the merge message git already prepared in `MERGE_MSG`. Valid to call both when
/// [`attempt_merge`] returned [`MergeOutcome::Clean`] and when every file from a
/// [`MergeOutcome::Conflicted`] result has been resolved and staged via
/// [`write_resolved_file`]: both leave the repository in the same "index updated,
/// `MERGE_HEAD`/`MERGE_MSG` present, nothing committed yet" state.
///
/// Defense in depth: before running `git commit`, this re-checks git's own ground truth
/// directly rather than trusting a caller's UI-level "is this resolved" belief -
/// [`Error::MergeNotInProgress`] if `MERGE_HEAD` doesn't exist, and
/// [`Error::MergeFilesStillConflicted`] if `git diff --name-only --diff-filter=U` still
/// reports an unmerged path. This matters because a conflict-marker parser (like
/// [`ConflictedFile::is_resolved`]) only ever sees *text* conflicts - a modify/delete or
/// binary conflict leaves git's index unmerged with zero `<<<<<<<` markers to parse (see
/// [`classify_conflicted_file`]'s docs).
///
/// Performs blocking I/O.
pub fn complete_merge(base_worktree_path: &Path) -> Result<(), Error> {
    if !merge_head_exists(base_worktree_path)? {
        return Err(Error::MergeNotInProgress {
            path: base_worktree_path.to_path_buf(),
        });
    }
    let unmerged = conflicted_files(base_worktree_path)?;
    if !unmerged.is_empty() {
        return Err(Error::MergeFilesStillConflicted { paths: unmerged });
    }

    let args: Vec<OsString> = vec!["commit".into(), "--no-edit".into()];
    let output = git_command(base_worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)
}

/// Best-effort check for whether the repository's detected base branch's worktree currently
/// has a merge in progress (`MERGE_HEAD` present) - used to offer an `Abort merge` action
/// after some other failure left a caller's UI in an error state, without assuming a
/// worktree path that might not even be resolvable any more. Returns `Ok(None)`, not an
/// error, if the base branch can't be detected or isn't checked out anywhere.
///
/// Performs blocking I/O.
pub fn find_in_progress_merge(repo_path: &Path) -> Result<Option<PathBuf>, Error> {
    let repo = open_repo(repo_path)?;
    let Some((base_branch, _base_commit_id)) = detect_default_base(&repo)? else {
        return Ok(None);
    };
    let Some(base_worktree_path) = find_worktree_with_branch(repo_path, &base_branch)? else {
        return Ok(None);
    };
    if merge_head_exists(&base_worktree_path)? {
        Ok(Some(base_worktree_path))
    } else {
        Ok(None)
    }
}

/// Direct check for whether `worktree_path` currently has a merge in progress (`MERGE_HEAD`
/// resolves) - `pub` so a caller that already knows a specific worktree path can check
/// directly, without re-deriving it via [`find_in_progress_merge`].
///
/// Performs blocking I/O.
pub fn merge_head_exists(worktree_path: &Path) -> Result<bool, Error> {
    let args: Vec<OsString> = vec![
        "rev-parse".into(),
        "--verify".into(),
        "-q".into(),
        "MERGE_HEAD".into(),
    ];
    let output = git_command(worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    Ok(output.status.success())
}

/// Files with git-reported conflict markers still present (index has unmerged stages).
///
/// Pins `-c core.quotePath=false`, matching `crate::diff`'s reasoning for the same pin on
/// `git diff`: without it, a non-ASCII path (e.g. `café.txt`) comes back octal-escaped and
/// quoted (`"caf\303\251.txt"`), and [`parse_paths`] would take that literally -
/// `load_conflicted_file`/`write_resolved_file` would then silently look up/create a
/// wrongly-named file instead of the real one.
fn conflicted_files(worktree_path: &Path) -> Result<Vec<PathBuf>, Error> {
    let args: Vec<OsString> = vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "diff".into(),
        "--name-only".into(),
        "--diff-filter=U".into(),
    ];
    let output = git_command(worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)?;
    Ok(parse_paths(&output.stdout))
}

/// Every file the merge attempt touched relative to the pre-merge `HEAD`, conflicted or not.
/// Pins `-c core.quotePath=false` - see [`conflicted_files`]'s docs for why.
fn touched_files(worktree_path: &Path) -> Result<Vec<PathBuf>, Error> {
    let args: Vec<OsString> = vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "diff".into(),
        "--name-only".into(),
        "HEAD".into(),
    ];
    let output = git_command(worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)?;
    Ok(parse_paths(&output.stdout))
}

fn parse_paths(stdout: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Find the worktree (main or linked) of the repository at `repo_path` whose checked-out
/// branch is exactly `branch`, if any. A worktree that itself failed to describe (a corrupt
/// entry - see [`crate::WorktreeResult`]'s docs) is skipped rather than failing this lookup
/// outright, matching [`crate::list_worktrees`]'s own "one bad entry shouldn't hide the rest"
/// contract.
fn find_worktree_with_branch(repo_path: &Path, branch: &str) -> Result<Option<PathBuf>, Error> {
    let entries = list_worktrees(repo_path)?;
    for entry in entries.into_iter().flatten() {
        if entry.branch.as_deref() == Some(branch) {
            return Ok(Some(entry.path));
        }
    }
    Ok(None)
}

// --- Conflict marker parsing and resolution -------------------------------------------

/// One real conflict block parsed out of a file's `<<<<<<</=======/>>>>>>>` markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictHunk {
    /// The label git wrote after `<<<<<<< ` (typically `HEAD` - the base branch's own
    /// content, since [`attempt_merge`] always runs `git merge` from the base worktree).
    pub ours_label: String,
    pub ours: Vec<String>,
    /// The label git wrote after `>>>>>>> ` (the merged-in branch's name).
    pub theirs_label: String,
    pub theirs: Vec<String>,
}

/// One segment of a conflicted file: either ordinary (non-conflicted) lines, or one real
/// conflict block. A file's full real content is exactly the concatenation of every segment,
/// in order - see [`ConflictedFile::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictSegment {
    Common(Vec<String>),
    Conflict(ConflictHunk),
}

/// A conflicted file's content, parsed from its actual `<<<<<<</=======/>>>>>>>` markers on
/// disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedFile {
    pub relative_path: PathBuf,
    pub segments: Vec<ConflictSegment>,
    /// Whether the file on disk ended with a trailing newline - preserved on
    /// [`ConflictedFile::render`] so resolving conflicts never spuriously adds or removes
    /// one. `pub` (a plain fact, not an invariant-guarded field) so callers outside this
    /// crate (`app::merge`'s tests) can construct one directly.
    pub trailing_newline: bool,
}

impl ConflictedFile {
    /// How many conflict hunks in this file still need resolving.
    pub fn remaining_conflicts(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| matches!(s, ConflictSegment::Conflict(_)))
            .count()
    }

    pub fn is_resolved(&self) -> bool {
        self.remaining_conflicts() == 0
    }

    /// Reconstruct the file's text content from its (possibly partially resolved) segments.
    /// Still-conflicted hunks round-trip as conflict markers, so this is safe to call (e.g.
    /// for a live preview) even before every hunk is resolved - only [`write_resolved_file`]
    /// refuses an unresolved file.
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for segment in &self.segments {
            match segment {
                ConflictSegment::Common(seg_lines) => lines.extend(seg_lines.iter().cloned()),
                ConflictSegment::Conflict(hunk) => {
                    lines.push(format!("<<<<<<< {}", hunk.ours_label));
                    lines.extend(hunk.ours.iter().cloned());
                    lines.push("=======".to_string());
                    lines.extend(hunk.theirs.iter().cloned());
                    lines.push(format!(">>>>>>> {}", hunk.theirs_label));
                }
            }
        }
        let mut text = lines.join("\n");
        if self.trailing_newline && !text.is_empty() {
            text.push('\n');
        }
        text
    }
}

/// Read the real conflicted file at `worktree_path.join(relative_path)` from disk and parse
/// its real conflict markers.
pub fn load_conflicted_file(
    worktree_path: &Path,
    relative_path: &Path,
) -> Result<ConflictedFile, Error> {
    let full = worktree_path.join(relative_path);
    let bytes = std::fs::read(&full).map_err(Error::WorktreeIo)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::MergeConflictFileNotUtf8 {
        path: relative_path.to_path_buf(),
    })?;
    let trailing_newline = text.ends_with('\n');
    let segments = parse_conflict_segments(&text, relative_path)?;
    Ok(ConflictedFile {
        relative_path: relative_path.to_path_buf(),
        segments,
        trailing_newline,
    })
}

/// Which of a conflicted path's real `base`/`ours`/`theirs` index stages exist
/// (`git ls-files -u`) - the real ground truth for *what kind* of conflict a path has,
/// independent of whatever content (or lack of real conflict markers) happens to be sitting
/// in the working tree. A normal two-sided text conflict has all three; a modify/delete
/// conflict is always missing exactly one (whichever side deleted the file never gets a
/// stage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StagePresence {
    base: bool,
    ours: bool,
    theirs: bool,
}

impl StagePresence {
    fn is_two_sided(self) -> bool {
        self.base && self.ours && self.theirs
    }
}

/// Per-path stage presence for every currently-unmerged path in `worktree_path`, via one
/// `git ls-files -u` subprocess. [`classify_conflicted_file`] calls this fresh on every
/// invocation, so a caller classifying multiple conflicted paths pays one subprocess per
/// path rather than a single batched call - correctness isn't affected (each call re-reads
/// the same current index state), just worth knowing if it shows up as measured overhead on
/// a merge with many conflicted files. Pins `-c core.quotePath=false` - see
/// [`conflicted_files`]'s docs for why.
fn unmerged_stage_presence(worktree_path: &Path) -> Result<HashMap<PathBuf, StagePresence>, Error> {
    let args: Vec<OsString> = vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "ls-files".into(),
        "-u".into(),
    ];
    let output = git_command(worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
    check_success(&args, &output)?;

    let mut map: HashMap<PathBuf, StagePresence> = HashMap::new();
    // Each line is `<mode> <sha> <stage>\t<path>` (`git ls-files -u` output format).
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let Some(stage) = meta.split_whitespace().nth(2) else {
            continue;
        };
        let entry = map.entry(PathBuf::from(path)).or_default();
        match stage {
            "1" => entry.base = true,
            "2" => entry.ours = true,
            "3" => entry.theirs = true,
            _ => {}
        }
    }
    Ok(map)
}

/// Why a conflicted path could not be represented as resolvable `<<<<<<<` text markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmergeableReason {
    /// One side deleted the file entirely; the other modified it. `git ls-files -u` shows
    /// only two of the three stages (whichever side deleted it has none); the working tree
    /// is left holding the *other* side's content verbatim, with no conflict markers, and
    /// `git status` reports `DU`/`UD`.
    ModifyDelete,
    /// All three stages are present (a genuine two-sided conflict), but the working tree
    /// contains no `<<<<<<<` markers - git's binary-content heuristic (which can trigger
    /// even for valid UTF-8, e.g. a file containing an embedded NUL byte) leaves one side's
    /// content in the tree verbatim instead of attempting a textual merge.
    Binary,
}

/// One conflicted path, classified by [`classify_conflicted_file`] into either a resolvable
/// text conflict or one this module has no text-hunk resolution for. Deliberately *not* the
/// same as "no `ConflictSegment::Conflict` entries were parsed" - git's own index (via
/// [`unmerged_stage_presence`]) is the ground truth for whether a path is genuinely
/// unmerged, not just "the parser found no markers" (see [`UnmergeableReason`]'s docs: a
/// naive "zero parsed segments means resolved" check would wrongly treat either of those
/// cases as already resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictedPath {
    /// Parseable `<<<<<<<` markers - resolvable via [`resolve_hunk`] + [`write_resolved_file`].
    Text(ConflictedFile),
    /// A conflict this module has no text-hunk resolution for - see [`UnmergeableReason`].
    /// Never silently treated as resolved; there is deliberately no `is_resolved`-style
    /// method here that could default to `true`.
    Unmergeable {
        relative_path: PathBuf,
        reason: UnmergeableReason,
    },
}

impl ConflictedPath {
    pub fn relative_path(&self) -> &Path {
        match self {
            ConflictedPath::Text(file) => &file.relative_path,
            ConflictedPath::Unmergeable { relative_path, .. } => relative_path,
        }
    }
}

/// Classify one already-known-conflicted path (e.g. from
/// [`MergeOutcome::Conflicted::conflicted_files`]) into a [`ConflictedPath`] - the
/// ground-truth-checked replacement for calling [`load_conflicted_file`] directly, which has
/// no way to tell a binary or modify/delete conflict apart from "already resolved". Always
/// consults [`unmerged_stage_presence`] first, and only trusts a zero-marker parse as
/// evidence of `Binary` once the stage shape confirms a genuine two-sided conflict.
pub fn classify_conflicted_file(
    worktree_path: &Path,
    relative_path: &Path,
) -> Result<ConflictedPath, Error> {
    let stages = unmerged_stage_presence(worktree_path)?
        .get(relative_path)
        .copied()
        .unwrap_or_default();

    if !stages.is_two_sided() {
        return Ok(ConflictedPath::Unmergeable {
            relative_path: relative_path.to_path_buf(),
            reason: UnmergeableReason::ModifyDelete,
        });
    }

    let file = load_conflicted_file(worktree_path, relative_path)?;
    if file.remaining_conflicts() == 0 {
        // All three real stages exist (a genuine two-sided conflict), but the parser found
        // no real `<<<<<<<` markers on disk at all - git's own binary-content heuristic, not
        // an already-resolved file (a merge never auto-resolves a path git itself still lists
        // as unmerged).
        return Ok(ConflictedPath::Unmergeable {
            relative_path: relative_path.to_path_buf(),
            reason: UnmergeableReason::Binary,
        });
    }

    Ok(ConflictedPath::Text(file))
}

enum ParseState {
    Outside,
    Ours {
        ours_label: String,
        ours: Vec<String>,
    },
    Theirs {
        ours_label: String,
        ours: Vec<String>,
        theirs: Vec<String>,
    },
}

fn parse_conflict_segments(
    text: &str,
    relative_path: &Path,
) -> Result<Vec<ConflictSegment>, Error> {
    let mut segments = Vec::new();
    let mut common: Vec<String> = Vec::new();
    let mut state = ParseState::Outside;

    let malformed = || Error::MergeMalformedConflictMarkers {
        path: relative_path.to_path_buf(),
    };

    for line in text.lines() {
        state = match state {
            ParseState::Outside => {
                if let Some(label) = line
                    .strip_prefix("<<<<<<< ")
                    .or_else(|| (line == "<<<<<<<").then_some(""))
                {
                    if !common.is_empty() {
                        segments.push(ConflictSegment::Common(std::mem::take(&mut common)));
                    }
                    ParseState::Ours {
                        ours_label: label.to_string(),
                        ours: Vec::new(),
                    }
                } else {
                    common.push(line.to_string());
                    ParseState::Outside
                }
            }
            ParseState::Ours {
                ours_label,
                mut ours,
            } => {
                if line == "=======" {
                    ParseState::Theirs {
                        ours_label,
                        ours,
                        theirs: Vec::new(),
                    }
                } else if line.starts_with("|||||||") {
                    return Err(Error::MergeUnsupportedConflictStyle {
                        path: relative_path.to_path_buf(),
                    });
                } else if line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ") {
                    return Err(malformed());
                } else {
                    ours.push(line.to_string());
                    ParseState::Ours { ours_label, ours }
                }
            }
            ParseState::Theirs {
                ours_label,
                ours,
                mut theirs,
            } => {
                if let Some(label) = line
                    .strip_prefix(">>>>>>> ")
                    .or_else(|| (line == ">>>>>>>").then_some(""))
                {
                    segments.push(ConflictSegment::Conflict(ConflictHunk {
                        ours_label,
                        ours,
                        theirs_label: label.to_string(),
                        theirs,
                    }));
                    ParseState::Outside
                } else if line.starts_with("<<<<<<< ") || line == "=======" {
                    return Err(malformed());
                } else {
                    theirs.push(line.to_string());
                    ParseState::Theirs {
                        ours_label,
                        ours,
                        theirs,
                    }
                }
            }
        };
    }

    match state {
        ParseState::Outside => {
            if !common.is_empty() {
                segments.push(ConflictSegment::Common(common));
            }
            Ok(segments)
        }
        // An unterminated `<<<<<<<`/`=======` block at end of file: real, but malformed.
        ParseState::Ours { .. } | ParseState::Theirs { .. } => Err(malformed()),
    }
}

/// Which side of a conflict hunk to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Left,
    Right,
    Both,
}

/// Resolve the hunk at `hunk_index` (its position within [`ConflictedFile::segments`]) by
/// keeping `choice`'s content, turning that segment into an ordinary, non-conflicted one.
/// Real, in-memory only - call [`write_resolved_file`] afterward to persist it.
pub fn resolve_hunk(
    file: &mut ConflictedFile,
    hunk_index: usize,
    choice: ConflictChoice,
) -> Result<(), Error> {
    let path = file.relative_path.clone();
    let segment = file
        .segments
        .get_mut(hunk_index)
        .ok_or_else(|| Error::MergeNoSuchHunk {
            path: path.clone(),
            index: hunk_index,
        })?;
    let ConflictSegment::Conflict(hunk) = segment else {
        return Err(Error::MergeNoSuchHunk {
            path,
            index: hunk_index,
        });
    };
    let resolved = match choice {
        ConflictChoice::Left => hunk.ours.clone(),
        ConflictChoice::Right => hunk.theirs.clone(),
        ConflictChoice::Both => {
            let mut both = hunk.ours.clone();
            both.extend(hunk.theirs.iter().cloned());
            both
        }
    };
    *segment = ConflictSegment::Common(resolved);
    Ok(())
}

/// Write a fully-resolved [`ConflictedFile`]'s content back to disk at
/// `worktree_path.join(&file.relative_path)`, then `git add` it. Refuses
/// ([`Error::MergeFileNotFullyResolved`]) if the file still has unresolved conflict hunks -
/// this is the only path that ever writes a conflicted file back to disk, and it never
/// writes one that still contains conflict markers.
pub fn write_resolved_file(worktree_path: &Path, file: &ConflictedFile) -> Result<(), Error> {
    if !file.is_resolved() {
        return Err(Error::MergeFileNotFullyResolved {
            path: file.relative_path.clone(),
        });
    }
    let full = worktree_path.join(&file.relative_path);
    std::fs::write(&full, file.render()).map_err(Error::WorktreeIo)?;

    let args: Vec<OsString> = vec![
        "add".into(),
        "--".into(),
        file.relative_path.clone().into_os_string(),
    ];
    let output = git_command(worktree_path, &args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;
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

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    fn status(dir: &Path) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn log_subjects(dir: &Path) -> Vec<String> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["log", "--all", "--format=%s"])
            .output()
            .expect("git log");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", rev])
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn parent_count(dir: &Path, rev: &str) -> usize {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["cat-file", "-p", rev])
            .output()
            .expect("git cat-file");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .take_while(|line| !line.is_empty())
            .filter(|line| line.starts_with("parent "))
            .count()
    }

    /// Real linked worktree, checked out on a new branch - the same idiom `crate`'s own
    /// `lib.rs` tests use (a throwaway `TempDir` immediately dropped just to mint a fresh,
    /// guaranteed-nonexistent path for `git worktree add` to create).
    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        path
    }

    #[test]
    fn clean_fast_forwardable_merge_stays_uncommitted_until_complete_merge() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        let (start, outcome) =
            attempt_merge(repo.path(), &feature).expect("attempt_merge should succeed");
        assert_eq!(start.base_branch, "main");
        assert_eq!(start.session_branch, "feature");
        assert_eq!(
            fs::canonicalize(&start.base_worktree_path).expect("canonicalize"),
            fs::canonicalize(repo.path()).expect("canonicalize")
        );
        let MergeOutcome::Clean { files } = outcome else {
            panic!("expected a clean merge, got {outcome:?}");
        };
        assert_eq!(files, vec![PathBuf::from("new.txt")]);

        // Not auto-committed: still mid-merge.
        assert!(
            status(repo.path()).contains("new.txt"),
            "new.txt should be staged but not yet committed"
        );
        assert!(
            merge_head_exists(repo.path()).expect("merge_head_exists"),
            "MERGE_HEAD must exist while the merge is uncommitted"
        );

        complete_merge(repo.path()).expect("complete_merge");
        assert!(
            !merge_head_exists(repo.path()).expect("merge_head_exists"),
            "MERGE_HEAD must be gone after a real commit"
        );
        assert_eq!(
            status(repo.path()),
            "",
            "working tree must be clean after completing"
        );
        assert!(repo.path().join("new.txt").is_file());
        // `--no-ff` forces a real merge commit even though this was fast-forwardable.
        assert_eq!(parent_count(repo.path(), "HEAD"), 2);
    }

    #[test]
    fn clean_non_fast_forward_three_way_merge_produces_a_real_merge_commit() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        // Diverge: base gets its own commit on a different file.
        fs::write(repo.path().join("base_only.txt"), "base work\n").expect("write");
        git(repo.path(), &["add", "base_only.txt"]);
        git(repo.path(), &["commit", "-m", "base commit"]);
        fs::write(feature.join("feature_only.txt"), "feature work\n").expect("write");
        git(&feature, &["add", "feature_only.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        let (_start, outcome) =
            attempt_merge(repo.path(), &feature).expect("attempt_merge should succeed");
        let MergeOutcome::Clean { files } = outcome else {
            panic!("expected a clean merge, got {outcome:?}");
        };
        assert!(files.contains(&PathBuf::from("feature_only.txt")));

        complete_merge(repo.path()).expect("complete_merge");
        assert_eq!(status(repo.path()), "");
        assert_eq!(parent_count(repo.path(), "HEAD"), 2);
        assert!(repo.path().join("base_only.txt").is_file());
        assert!(repo.path().join("feature_only.txt").is_file());
        assert!(log_subjects(repo.path()).contains(&"Merge branch 'feature'".to_string()));
    }

    #[test]
    fn conflicting_merge_reports_conflicted_and_clean_files_separately() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        fs::write(repo.path().join("clean.txt"), "clean1\nclean2\n").expect("write");
        git(repo.path(), &["add", "shared.txt", "clean.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared/clean files"]);

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);

        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        fs::write(
            feature.join("clean.txt"),
            "clean1\nclean2 changed by feature\n",
        )
        .expect("write");
        git(
            &feature,
            &["commit", "-am", "feature changes shared.txt and clean.txt"],
        );

        let (_start, outcome) =
            attempt_merge(repo.path(), &feature).expect("attempt_merge should succeed");
        let MergeOutcome::Conflicted {
            conflicted_files,
            clean_files,
        } = outcome
        else {
            panic!("expected a conflicted merge, got {outcome:?}");
        };
        assert_eq!(conflicted_files, vec![PathBuf::from("shared.txt")]);
        assert_eq!(clean_files, vec![PathBuf::from("clean.txt")]);

        // The real conflict markers are genuinely on disk.
        let on_disk = fs::read_to_string(repo.path().join("shared.txt")).expect("read");
        assert!(on_disk.contains("<<<<<<< HEAD"));
        assert!(on_disk.contains("======="));
        assert!(on_disk.contains(">>>>>>> feature"));
        // The auto-merged file has real, correct content already.
        let clean_on_disk = fs::read_to_string(repo.path().join("clean.txt")).expect("read");
        assert_eq!(clean_on_disk, "clean1\nclean2 changed by feature\n");
    }

    #[test]
    fn resolving_via_take_left_take_right_take_both_writes_real_content_and_stages_it() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (_start, outcome) =
            attempt_merge(repo.path(), &feature).expect("attempt_merge should succeed");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected conflicts");
        };
        assert_eq!(conflicted_files, vec![PathBuf::from("shared.txt")]);

        // Take-left.
        let mut file =
            load_conflicted_file(repo.path(), &conflicted_files[0]).expect("load_conflicted_file");
        assert_eq!(file.remaining_conflicts(), 1);
        resolve_hunk(&mut file, 1, ConflictChoice::Left).expect("resolve_hunk");
        assert!(file.is_resolved());
        assert_eq!(file.render(), "line1\nBASE CHANGED\nline3\n");
        write_resolved_file(repo.path(), &file).expect("write_resolved_file");
        assert_eq!(
            fs::read_to_string(repo.path().join("shared.txt")).expect("read"),
            "line1\nBASE CHANGED\nline3\n"
        );
        // Real, correct git behavior for a take-left resolution: since the resolved content
        // is identical to the base branch's own pre-merge `HEAD` content, `git status`
        // reports no working-tree change for this file at all once it's staged - only that
        // it's no longer unmerged (`UU`). Asserting "no longer UU" (rather than assuming a
        // literal `M` line) is what's actually true here, verified empirically above.
        assert!(!status(repo.path()).contains("UU shared.txt"));

        abort_merge(repo.path()).expect("abort_merge");

        // Take-right, from a fresh attempt.
        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected conflicts again");
        };
        let mut file =
            load_conflicted_file(repo.path(), &conflicted_files[0]).expect("load_conflicted_file");
        resolve_hunk(&mut file, 1, ConflictChoice::Right).expect("resolve_hunk");
        assert_eq!(file.render(), "line1\nFEATURE CHANGED\nline3\n");
        write_resolved_file(repo.path(), &file).expect("write_resolved_file");

        abort_merge(repo.path()).expect("abort_merge");

        // Take-both, from another fresh attempt.
        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected conflicts a third time");
        };
        let mut file =
            load_conflicted_file(repo.path(), &conflicted_files[0]).expect("load_conflicted_file");
        resolve_hunk(&mut file, 1, ConflictChoice::Both).expect("resolve_hunk");
        assert_eq!(
            file.render(),
            "line1\nBASE CHANGED\nFEATURE CHANGED\nline3\n"
        );
        write_resolved_file(repo.path(), &file).expect("write_resolved_file");
        assert_eq!(
            fs::read_to_string(repo.path().join("shared.txt")).expect("read"),
            "line1\nBASE CHANGED\nFEATURE CHANGED\nline3\n"
        );
    }

    #[test]
    fn completing_a_resolved_conflicted_merge_produces_a_clean_real_merge_commit() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected conflicts");
        };
        let mut file =
            load_conflicted_file(repo.path(), &conflicted_files[0]).expect("load_conflicted_file");
        resolve_hunk(&mut file, 1, ConflictChoice::Both).expect("resolve_hunk");
        write_resolved_file(repo.path(), &file).expect("write_resolved_file");

        complete_merge(repo.path()).expect("complete_merge");

        assert_eq!(status(repo.path()), "", "repository must end up clean");
        assert!(
            !merge_head_exists(repo.path()).expect("merge_head_exists"),
            "no leftover MERGE_HEAD after a real commit"
        );
        assert_eq!(parent_count(repo.path(), "HEAD"), 2);
        assert!(log_subjects(repo.path()).contains(&"Merge branch 'feature'".to_string()));
        assert_eq!(
            fs::read_to_string(repo.path().join("shared.txt")).expect("read"),
            "line1\nBASE CHANGED\nFEATURE CHANGED\nline3\n"
        );
    }

    #[test]
    fn aborting_a_conflicted_merge_recovers_a_clean_real_repository_state() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let before_head = rev_parse(repo.path(), "HEAD");
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);
        let base_head_before_merge = rev_parse(repo.path(), "HEAD");
        let _ = before_head;

        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        assert!(matches!(outcome, MergeOutcome::Conflicted { .. }));
        assert!(merge_head_exists(repo.path()).expect("merge_head_exists"));

        abort_merge(repo.path()).expect("abort_merge");

        assert_eq!(status(repo.path()), "", "abort must leave a clean tree");
        assert!(
            !merge_head_exists(repo.path()).expect("merge_head_exists"),
            "no leftover MERGE_HEAD after abort"
        );
        assert_eq!(
            rev_parse(repo.path(), "HEAD"),
            base_head_before_merge,
            "HEAD must be unchanged by an aborted merge"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("shared.txt")).expect("read"),
            "line1\nBASE CHANGED\nline3\n",
            "the real pre-merge content must be restored, with no leftover conflict markers"
        );
    }

    #[test]
    fn dirty_base_worktree_is_refused_before_touching_anything() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        // Dirty the base (main) worktree.
        fs::write(repo.path().join("base.txt"), "uncommitted change\n").expect("write");

        let err = attempt_merge(repo.path(), &feature)
            .expect_err("a dirty base worktree must be refused");
        match err {
            Error::MergeTargetDirty { path } => {
                assert_eq!(
                    fs::canonicalize(&path).expect("canonicalize"),
                    fs::canonicalize(repo.path()).expect("canonicalize")
                );
            }
            other => panic!("expected Error::MergeTargetDirty, got {other:?}"),
        }
        assert!(
            !merge_head_exists(repo.path()).expect("merge_head_exists"),
            "no merge must have been started"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("base.txt")).expect("read"),
            "uncommitted change\n",
            "the real dirty content must be untouched"
        );
    }

    #[test]
    fn base_branch_not_checked_out_anywhere_is_refused_cleanly() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        // The base worktree (main) is now on a detached HEAD - `main` is checked out nowhere.
        git(repo.path(), &["checkout", "--detach", "HEAD"]);

        let err = attempt_merge(repo.path(), &feature)
            .expect_err("a base branch checked out nowhere must be refused");
        match err {
            Error::MergeBaseBranchNotCheckedOut { branch } => assert_eq!(branch, "main"),
            other => panic!("expected Error::MergeBaseBranchNotCheckedOut, got {other:?}"),
        }
    }

    #[test]
    fn merging_from_a_detached_session_worktree_is_refused() {
        let repo = init_repo();
        let container = TempDir::new().expect("tempdir");
        let detached_path = container.path().join("detached-wt");
        drop(container);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--detach",
                detached_path.to_str().expect("utf8 path"),
                "main",
            ],
        );

        let err = attempt_merge(repo.path(), &detached_path)
            .expect_err("a detached session worktree must be refused");
        assert!(matches!(err, Error::MergeSourceDetached { .. }));
    }

    #[test]
    fn merging_the_base_branch_into_itself_is_refused() {
        let repo = init_repo();
        let err = attempt_merge(repo.path(), repo.path())
            .expect_err("merging the base branch into itself must be refused");
        match err {
            Error::MergeSourceIsBaseBranch { branch } => assert_eq!(branch, "main"),
            other => panic!("expected Error::MergeSourceIsBaseBranch, got {other:?}"),
        }
    }

    #[test]
    fn already_merged_branch_reports_no_op_without_creating_merge_head_or_a_commit() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        assert!(matches!(outcome, MergeOutcome::Clean { .. }));
        complete_merge(repo.path()).expect("complete_merge");
        let head_after_first_merge = rev_parse(repo.path(), "HEAD");

        let (_start, outcome) = attempt_merge(repo.path(), &feature)
            .expect("merging an already-merged branch again should not error");
        assert_eq!(outcome, MergeOutcome::AlreadyUpToDate);
        assert!(!merge_head_exists(repo.path()).expect("merge_head_exists"));
        assert_eq!(
            rev_parse(repo.path(), "HEAD"),
            head_after_first_merge,
            "an already-up-to-date merge must not move HEAD or create a commit"
        );
    }

    #[test]
    fn parse_conflict_segments_splits_ours_theirs_and_preserves_labels() {
        let text =
            "before\n<<<<<<< HEAD\nours line\n=======\ntheirs line\n>>>>>>> feature\nafter\n";
        let segments = parse_conflict_segments(text, Path::new("f.txt")).expect("parse");
        assert_eq!(segments.len(), 3);
        assert_eq!(
            segments[0],
            ConflictSegment::Common(vec!["before".to_string()])
        );
        let ConflictSegment::Conflict(hunk) = &segments[1] else {
            panic!("expected a conflict segment");
        };
        assert_eq!(hunk.ours_label, "HEAD");
        assert_eq!(hunk.ours, vec!["ours line".to_string()]);
        assert_eq!(hunk.theirs_label, "feature");
        assert_eq!(hunk.theirs, vec!["theirs line".to_string()]);
        assert_eq!(
            segments[2],
            ConflictSegment::Common(vec!["after".to_string()])
        );
    }

    #[test]
    fn parse_conflict_segments_rejects_diff3_style_markers() {
        let text = "<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> feature\n";
        let err = parse_conflict_segments(text, Path::new("f.txt"))
            .expect_err("diff3 markers must be rejected");
        assert!(matches!(err, Error::MergeUnsupportedConflictStyle { .. }));
    }

    #[test]
    fn parse_conflict_segments_rejects_unterminated_markers() {
        let text = "<<<<<<< HEAD\nours\n=======\ntheirs\n";
        let err = parse_conflict_segments(text, Path::new("f.txt"))
            .expect_err("an unterminated conflict block must be rejected");
        assert!(matches!(err, Error::MergeMalformedConflictMarkers { .. }));
    }

    #[test]
    fn conflicted_file_round_trips_trailing_newline_state() {
        let with_newline = ConflictedFile {
            relative_path: PathBuf::from("f.txt"),
            segments: vec![ConflictSegment::Common(vec![
                "a".to_string(),
                "b".to_string(),
            ])],
            trailing_newline: true,
        };
        assert_eq!(with_newline.render(), "a\nb\n");

        let without_newline = ConflictedFile {
            trailing_newline: false,
            ..with_newline
        };
        assert_eq!(without_newline.render(), "a\nb");
    }

    #[test]
    fn load_conflicted_file_rejects_non_utf8_content() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("binary.bin");
        fs::write(&path, [0u8, 159, 146, 0]).expect("write");
        let err = load_conflicted_file(dir.path(), Path::new("binary.bin"))
            .expect_err("non-UTF-8 content must be refused");
        assert!(matches!(err, Error::MergeConflictFileNotUtf8 { .. }));
    }

    #[test]
    fn resolve_hunk_rejects_invalid_or_already_resolved_index() {
        let mut file = ConflictedFile {
            relative_path: PathBuf::from("f.txt"),
            segments: vec![ConflictSegment::Common(vec!["only line".to_string()])],
            trailing_newline: true,
        };
        let err = resolve_hunk(&mut file, 0, ConflictChoice::Left)
            .expect_err("resolving a non-conflict segment must fail");
        assert!(matches!(err, Error::MergeNoSuchHunk { index: 0, .. }));

        let err = resolve_hunk(&mut file, 5, ConflictChoice::Left)
            .expect_err("an out-of-range index must fail");
        assert!(matches!(err, Error::MergeNoSuchHunk { index: 5, .. }));
    }

    #[test]
    fn write_resolved_file_refuses_when_conflicts_remain() {
        let dir = TempDir::new().expect("tempdir");
        let file = ConflictedFile {
            relative_path: PathBuf::from("f.txt"),
            segments: vec![ConflictSegment::Conflict(ConflictHunk {
                ours_label: "HEAD".to_string(),
                ours: vec!["ours".to_string()],
                theirs_label: "feature".to_string(),
                theirs: vec!["theirs".to_string()],
            })],
            trailing_newline: true,
        };
        let err = write_resolved_file(dir.path(), &file)
            .expect_err("a file with unresolved conflicts must be refused");
        assert!(matches!(err, Error::MergeFileNotFullyResolved { .. }));
    }

    #[test]
    fn non_ascii_filename_round_trips_through_the_real_conflict_and_resolution_pipeline() {
        // Regression test for a real, verified bug: without `-c core.quotePath=false`,
        // `git diff --name-only` prints a non-ASCII path octal-escaped and quoted
        // (`"caf\303\251.txt"`), which `parse_paths` would take literally - `load_conflicted_
        // file` would fail to find the real file, and `write_resolved_file` would `fs::write`
        // a brand new, wrongly-named file (quote marks included) into the real worktree.
        let repo = init_repo();
        fs::write(repo.path().join("café.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "café.txt"]);
        git(repo.path(), &["commit", "-m", "seed café.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(repo.path().join("café.txt"), "line1\nBASE CHANGED\nline3\n").expect("write");
        git(repo.path(), &["commit", "-am", "base changes café.txt"]);
        fs::write(feature.join("café.txt"), "line1\nFEATURE CHANGED\nline3\n").expect("write");
        git(&feature, &["commit", "-am", "feature changes café.txt"]);

        let (_start, outcome) =
            attempt_merge(repo.path(), &feature).expect("attempt_merge should succeed");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected conflicts");
        };
        // The real, bare, unquoted, unescaped path - not `"caf\303\251.txt"`.
        assert_eq!(conflicted_files, vec![PathBuf::from("café.txt")]);

        let mut file = load_conflicted_file(repo.path(), &conflicted_files[0])
            .expect("load_conflicted_file must find the real café.txt, not a mangled path");
        resolve_hunk(&mut file, 1, ConflictChoice::Both).expect("resolve_hunk");
        write_resolved_file(repo.path(), &file).expect("write_resolved_file");

        // The real file (real name) has the real resolved content...
        assert_eq!(
            fs::read_to_string(repo.path().join("café.txt")).expect("read café.txt"),
            "line1\nBASE CHANGED\nFEATURE CHANGED\nline3\n"
        );
        // ...and no stray, wrongly-named file (e.g. a literal `"caf\303\251.txt"`) was ever
        // created alongside it - only the real `café.txt` and `init_repo`'s own seed file.
        let entries: Vec<String> = fs::read_dir(repo.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".git")
            .collect();
        let mut sorted_entries = entries.clone();
        sorted_entries.sort();
        assert_eq!(
            sorted_entries,
            vec!["base.txt".to_string(), "café.txt".to_string()],
            "no stray quoted/escaped-name file should exist: {entries:?}"
        );

        complete_merge(repo.path()).expect("complete_merge");
        assert_eq!(status(repo.path()), "");
    }

    #[test]
    fn modify_delete_conflict_is_classified_as_unmergeable_not_falsely_resolved() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");

        // Base deletes the file...
        git(repo.path(), &["rm", "-q", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "base deletes shared.txt"]);
        // ...while feature modifies it - a real modify/delete conflict.
        fs::write(feature.join("shared.txt"), "modified by feature\n").expect("write");
        git(&feature, &["commit", "-am", "feature modifies shared.txt"]);

        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected a conflicted merge");
        };
        assert_eq!(conflicted_files, vec![PathBuf::from("shared.txt")]);

        let classified = classify_conflicted_file(repo.path(), &conflicted_files[0])
            .expect("classify_conflicted_file");
        assert_eq!(
            classified,
            ConflictedPath::Unmergeable {
                relative_path: PathBuf::from("shared.txt"),
                reason: UnmergeableReason::ModifyDelete,
            },
            "a modify/delete conflict must be classified as Unmergeable, never as an \
             already-resolved text file"
        );

        // Real defense in depth: `complete_merge` must refuse too, even though nothing here
        // called any (nonexistent, for this file) resolution API.
        let err = complete_merge(repo.path())
            .expect_err("complete_merge must refuse while shared.txt is still really unmerged");
        assert!(matches!(err, Error::MergeFilesStillConflicted { .. }));

        abort_merge(repo.path()).expect("abort_merge");
    }

    #[test]
    fn binary_conflict_with_a_nul_byte_is_classified_as_unmergeable_not_falsely_resolved() {
        // A NUL byte is itself a valid single-byte UTF-8 codepoint, so `String::from_utf8`
        // alone can't detect this case - `git` itself still treats the file as binary (its
        // own heuristic scans for embedded NULs) and leaves no `<<<<<<<` markers in the
        // working tree at all, which is exactly what makes this distinct from a genuinely
        // resolved (zero-conflict) text file.
        let repo = init_repo();
        fs::write(repo.path().join("shared.bin"), b"line1\x00line2\n").expect("write");
        git(repo.path(), &["add", "shared.bin"]);
        git(repo.path(), &["commit", "-m", "seed shared.bin"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(repo.path().join("shared.bin"), b"line1\x00BASE\n").expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.bin"]);
        fs::write(feature.join("shared.bin"), b"line1\x00FEATURE\n").expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.bin"]);

        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected a conflicted merge");
        };
        assert_eq!(conflicted_files, vec![PathBuf::from("shared.bin")]);

        // Confirm the real premise: the on-disk content really is valid UTF-8 (so a plain
        // `String::from_utf8` check alone would not catch this).
        let on_disk = fs::read(repo.path().join("shared.bin")).expect("read");
        assert!(
            String::from_utf8(on_disk).is_ok(),
            "the working-tree content must be valid UTF-8 for this to be a real regression test"
        );

        let classified = classify_conflicted_file(repo.path(), &conflicted_files[0])
            .expect("classify_conflicted_file");
        assert_eq!(
            classified,
            ConflictedPath::Unmergeable {
                relative_path: PathBuf::from("shared.bin"),
                reason: UnmergeableReason::Binary,
            },
            "a binary (NUL-containing) conflict must be classified as Unmergeable, never as \
             an already-resolved text file"
        );

        let err = complete_merge(repo.path())
            .expect_err("complete_merge must refuse while shared.bin is still really unmerged");
        assert!(matches!(err, Error::MergeFilesStillConflicted { .. }));

        abort_merge(repo.path()).expect("abort_merge");
    }

    #[test]
    fn classify_conflicted_file_reports_a_real_text_conflict_as_text() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (_start, outcome) = attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let MergeOutcome::Conflicted {
            conflicted_files, ..
        } = outcome
        else {
            panic!("expected a conflicted merge");
        };

        let classified = classify_conflicted_file(repo.path(), &conflicted_files[0])
            .expect("classify_conflicted_file");
        match classified {
            ConflictedPath::Text(file) => assert_eq!(file.remaining_conflicts(), 1),
            other => panic!("expected a real text conflict, got {other:?}"),
        }
    }

    #[test]
    fn complete_merge_refuses_when_no_merge_is_in_progress() {
        let repo = init_repo();
        let err = complete_merge(repo.path())
            .expect_err("complete_merge must refuse when there is no real merge in progress");
        assert!(matches!(err, Error::MergeNotInProgress { .. }));
    }

    #[test]
    fn find_in_progress_merge_reports_the_real_base_worktree_only_while_merge_head_exists() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        assert_eq!(
            find_in_progress_merge(repo.path()).expect("find_in_progress_merge"),
            None,
            "no merge is in progress yet"
        );

        attempt_merge(repo.path(), &feature).expect("attempt_merge");
        let found = find_in_progress_merge(repo.path())
            .expect("find_in_progress_merge")
            .expect("a real merge is genuinely in progress");
        assert_eq!(
            fs::canonicalize(&found).expect("canonicalize"),
            fs::canonicalize(repo.path()).expect("canonicalize")
        );

        abort_merge(repo.path()).expect("abort_merge");
        assert_eq!(
            find_in_progress_merge(repo.path()).expect("find_in_progress_merge"),
            None,
            "no merge is in progress any more after a real abort"
        );
    }
}
