//! Merging a worktree's branch into the detected base branch, and resolving the conflicts.
//!
//! git can only check a branch out in one worktree at a time, so [`attempt_merge`] runs in
//! whichever worktree already has the base branch, rather than staging a temporary checkout.
//!
//! `--no-commit --no-ff` together, since `--no-commit` alone still auto-commits a fast-forward.
//! "Already up to date" exits 0 without creating `MERGE_HEAD`, so that file decides the outcome
//! rather than the exit status. `merge.conflictStyle=merge` is pinned to keep markers two-way.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::diff::detect_default_base;
use crate::error::{Error, GitExit};
use crate::{check_success, format_args, git_command, is_dirty, list_worktrees, open_repo};

/// Means the same from either entry point: `base_branch` was merged **into**, `session_branch`
/// merged **from**, and `base_worktree_path` is where `git merge` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeStart {
    pub base_branch: String,
    pub base_worktree_path: PathBuf,
    pub session_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The base branch already contains every commit on the session branch; nothing changed.
    AlreadyUpToDate,
    /// Merged without conflicts, staged but uncommitted; [`complete_merge`] finishes it.
    Clean { files: Vec<PathBuf> },
    /// `clean_files` git resolved on its own; `conflicted_files` carry markers on disk and need
    /// [`load_conflicted_file`] and resolution before [`complete_merge`] can run.
    Conflicted {
        conflicted_files: Vec<PathBuf>,
        clean_files: Vec<PathBuf>,
    },
}

/// Merges the branch checked out in `session_worktree_path` into the detected base branch.
///
/// `repo_path` only locates the repository; the merge runs in whichever worktree has the base
/// branch checked out.
///
/// Refuses with [`Error::MergeTargetDirty`] if that worktree is dirty. git often refuses too, but
/// checking first gives callers one structured error instead of stderr to parse.
pub fn attempt_merge(
    repo_path: &Path,
    session_worktree_path: &Path,
) -> Result<(MergeStart, MergeOutcome), Error> {
    let Some(session_branch) = checked_out_branch(session_worktree_path)? else {
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

    let outcome = run_merge(&base_worktree_path, &session_branch)?;
    let start = MergeStart {
        base_branch,
        base_worktree_path,
        session_branch,
    };
    Ok((start, outcome))
}

/// Merges `source_branch` into whatever is checked out in `target_worktree_path` - the opposite
/// direction from [`attempt_merge`].
///
/// The target worktree is given rather than searched for, so there is no base-branch detection
/// here. Everything else runs through the same [`run_merge`], so the two directions cannot
/// classify a merge differently.
///
/// Three preconditions are refused before `git merge` runs: a detached target
/// ([`Error::MergeTargetDetached`]), which git would otherwise merge onto silently; a dirty target
/// ([`Error::MergeTargetDirty`]); and merging a branch into itself
/// ([`Error::MergeSourceIsCurrentBranch`]). A nonexistent `source_branch` is left to git.
pub fn attempt_merge_into_current(
    target_worktree_path: &Path,
    source_branch: &str,
) -> Result<(MergeStart, MergeOutcome), Error> {
    let Some(target_branch) = checked_out_branch(target_worktree_path)? else {
        return Err(Error::MergeTargetDetached {
            path: target_worktree_path.to_path_buf(),
        });
    };

    if target_branch == source_branch {
        return Err(Error::MergeSourceIsCurrentBranch {
            branch: target_branch,
        });
    }

    if is_dirty(target_worktree_path)? {
        return Err(Error::MergeTargetDirty {
            path: target_worktree_path.to_path_buf(),
        });
    }

    let outcome = run_merge(target_worktree_path, source_branch)?;
    let start = MergeStart {
        base_branch: target_branch,
        base_worktree_path: target_worktree_path.to_path_buf(),
        session_branch: source_branch.to_string(),
    };
    Ok((start, outcome))
}

/// `worktree_path` must already have the branch being merged *into* checked out, be clean, and not
/// be `source_branch`'s own branch; each caller checks that itself.
fn run_merge(worktree_path: &Path, source_branch: &str) -> Result<MergeOutcome, Error> {
    let args: Vec<OsString> = vec![
        "-c".into(),
        "merge.conflictStyle=merge".into(),
        "merge".into(),
        "--no-commit".into(),
        "--no-ff".into(),
        "--".into(),
        source_branch.into(),
    ];
    let mut command = git_command(worktree_path, &args);
    let output = command.output().map_err(|source| Error::GitSpawn {
        args: format_args(&args),
        source,
    })?;

    if output.status.success() {
        if !merge_head_exists(worktree_path)? {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        let files = touched_files(worktree_path)?;
        return Ok(MergeOutcome::Clean { files });
    }

    let conflicted_files = conflicted_files(worktree_path)?;
    if conflicted_files.is_empty() {
        return Err(Error::GitCommand {
            args: format_args(&args),
            exit: GitExit::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let touched = touched_files(worktree_path)?;
    let clean_files = touched
        .into_iter()
        .filter(|f| !conflicted_files.contains(f))
        .collect();

    Ok(MergeOutcome::Conflicted {
        conflicted_files,
        clean_files,
    })
}

/// The short name of the branch checked out in `worktree_path`, read from `HEAD` rather than
/// assumed. `None` when detached; each caller turns that into its own refusal.
fn checked_out_branch(worktree_path: &Path) -> Result<Option<String>, Error> {
    let repo = open_repo(worktree_path)?;
    let head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    Ok(head.referent_name().map(|name| name.shorten().to_string()))
}

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

/// Commits an in-progress merge using the message git prepared in `MERGE_MSG`.
///
/// Valid after either a [`MergeOutcome::Clean`] result or a fully resolved
/// [`MergeOutcome::Conflicted`] one; both leave the same staged-but-uncommitted state.
///
/// Re-checks git's own ground truth first rather than trusting the caller: a marker parser only
/// sees *text* conflicts, while a modify/delete or binary conflict leaves the index unmerged with
/// no markers at all.
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

/// Whether the base branch's worktree has a merge in progress, for offering an abort after some
/// other failure. `Ok(None)` if no base branch is detectable or checked out anywhere.
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

/// The paths git reports as unmerged, with `core.quotePath=false` pinned: otherwise a non-ASCII
/// path comes back octal-escaped and the load and write act on a wrongly-named file.
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

/// The worktree whose checked-out branch is exactly `branch`, if any. A worktree that failed to
/// describe is skipped rather than failing the lookup.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictHunk {
    /// The label git wrote after `<<<<<<< `, typically `HEAD`.
    pub ours_label: String,
    pub ours: Vec<String>,
    /// 1-indexed line of `ours`' first content line in the file on disk.
    ///
    /// Meaningless when `ours` is empty, where it equals the `=======` line instead, so callers
    /// must gate gutter rendering on `ours.is_empty()`.
    pub ours_start_line: usize,
    /// The label git wrote after `>>>>>>> `.
    pub theirs_label: String,
    pub theirs: Vec<String>,
    /// 1-indexed line of `theirs`' first content line, with the same caveat as
    /// [`Self::ours_start_line`].
    pub theirs_start_line: usize,
}

/// One segment of a conflicted file. Concatenating every segment reproduces the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictSegment {
    Common(Vec<String>),
    Conflict(ConflictHunk),
}

/// A conflicted file's content, parsed from its markers on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedFile {
    pub relative_path: PathBuf,
    pub segments: Vec<ConflictSegment>,
    /// Preserved through [`ConflictedFile::render`] so resolving never adds or drops one.
    pub trailing_newline: bool,
}

impl ConflictedFile {
    pub fn remaining_conflicts(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| matches!(s, ConflictSegment::Conflict(_)))
            .count()
    }

    pub fn is_resolved(&self) -> bool {
        self.remaining_conflicts() == 0
    }

    /// Reconstructs the file's text from its possibly partly-resolved segments.
    ///
    /// Unresolved hunks round-trip as markers, so this is safe for a live preview; only
    /// [`write_resolved_file`] refuses an unresolved file.
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

/// Reads the conflicted file from disk and parses its markers.
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

/// Which index stages a conflicted path has, which is what decides the *kind* of conflict
/// independently of whatever is in the working tree.
///
/// A two-sided text conflict has all three; a modify/delete conflict is missing whichever side
/// deleted the file.
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

/// Stage presence for every unmerged path, from one `git ls-files -u`.
///
/// [`classify_conflicted_file`] calls this per path rather than batching, so a merge with many
/// conflicts pays one subprocess each. Correct either way, but worth knowing if it shows up as
/// overhead. Pins `core.quotePath=false`, as [`conflicted_files`] does.
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
    // Each line is `<mode> <sha> <stage>\t<path>`.
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

/// Why a conflicted path has no resolvable text markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmergeableReason {
    /// One side deleted the file and the other modified it, so only two stages exist and the
    /// working tree holds the surviving side verbatim, unmarked.
    ModifyDelete,
    /// All three stages exist but no markers do: git's binary-content heuristic - which can fire
    /// on valid UTF-8 containing a NUL - left one side's content verbatim.
    Binary,
}

/// A conflicted path, classified as a resolvable text conflict or as one with no text-hunk
/// resolution.
///
/// Not the same question as "did the parser find markers": git's index is the ground truth for
/// whether a path is unmerged, and a zero-marker parse would otherwise read as already resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictedPath {
    /// Parseable markers, resolvable via [`resolve_hunk`] and [`write_resolved_file`].
    Text(ConflictedFile),
    /// Never treated as resolved; there is deliberately no `is_resolved` here to default to
    /// `true`.
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

/// Classifies an already-known-conflicted path.
///
/// Use this rather than [`load_conflicted_file`] directly, which cannot tell a binary or
/// modify/delete conflict from an already-resolved one. A zero-marker parse is only read as
/// `Binary` once the stage shape confirms a two-sided conflict.
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
        // Two-sided but unmarked: git's binary heuristic, not an already-resolved file - a merge
        // never auto-resolves a path git still lists as unmerged.
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
        /// Captured on entering this state; see [`ConflictHunk::ours_start_line`].
        ours_start_line: usize,
    },
    Theirs {
        ours_label: String,
        ours: Vec<String>,
        ours_start_line: usize,
        theirs: Vec<String>,
        /// Captured on entering this state; see [`ConflictHunk::theirs_start_line`].
        theirs_start_line: usize,
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

    for (index, line) in text.lines().enumerate() {
        // 1-indexed line number of `line` itself.
        let line_number = index + 1;
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
                        ours_start_line: line_number + 1,
                    }
                } else {
                    common.push(line.to_string());
                    ParseState::Outside
                }
            }
            ParseState::Ours {
                ours_label,
                mut ours,
                ours_start_line,
            } => {
                if line == "=======" {
                    ParseState::Theirs {
                        ours_label,
                        ours,
                        ours_start_line,
                        theirs: Vec::new(),
                        theirs_start_line: line_number + 1,
                    }
                } else if line.starts_with("|||||||") {
                    return Err(Error::MergeUnsupportedConflictStyle {
                        path: relative_path.to_path_buf(),
                    });
                } else if line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ") {
                    return Err(malformed());
                } else {
                    ours.push(line.to_string());
                    ParseState::Ours {
                        ours_label,
                        ours,
                        ours_start_line,
                    }
                }
            }
            ParseState::Theirs {
                ours_label,
                ours,
                ours_start_line,
                mut theirs,
                theirs_start_line,
            } => {
                if let Some(label) = line
                    .strip_prefix(">>>>>>> ")
                    .or_else(|| (line == ">>>>>>>").then_some(""))
                {
                    segments.push(ConflictSegment::Conflict(ConflictHunk {
                        ours_label,
                        ours,
                        ours_start_line,
                        theirs_label: label.to_string(),
                        theirs,
                        theirs_start_line,
                    }));
                    ParseState::Outside
                } else if line.starts_with("<<<<<<< ") || line == "=======" {
                    return Err(malformed());
                } else {
                    theirs.push(line.to_string());
                    ParseState::Theirs {
                        ours_label,
                        ours,
                        ours_start_line,
                        theirs,
                        theirs_start_line,
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
        // An unterminated block at end of file.
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

/// Resolves the hunk at `hunk_index` within [`ConflictedFile::segments`] by keeping `choice`'s
/// content. In memory only; [`write_resolved_file`] persists it.
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

/// Writes a fully-resolved file back to disk and stages it.
///
/// The only path that writes a conflicted file back, and it refuses with
/// [`Error::MergeFileNotFullyResolved`] rather than persisting remaining markers.
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

    /// A linked worktree on a new branch. The throwaway `TempDir` is dropped immediately, purely
    /// to mint a path `git worktree add` can create.
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
        assert_eq!(parent_count(repo.path(), "HEAD"), 2);
    }

    #[test]
    fn clean_non_fast_forward_three_way_merge_produces_a_real_merge_commit() {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
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

        let on_disk = fs::read_to_string(repo.path().join("shared.txt")).expect("read");
        assert!(on_disk.contains("<<<<<<< HEAD"));
        assert!(on_disk.contains("======="));
        assert!(on_disk.contains(">>>>>>> feature"));
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
        // Take-left leaves content identical to the pre-merge `HEAD`, so `git status` reports no
        // working-tree change at all once staged - only that it is no longer `UU`. Asserting that,
        // rather than a literal `M` line, is what is actually true.
        assert!(!status(repo.path()).contains("UU shared.txt"));

        abort_merge(repo.path()).expect("abort_merge");

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

    // --- `attempt_merge_into_current` ------------------------------------------------------
    //
    // The opposite direction, covered one-for-one. Every fixture leaves the source branch checked
    // out nowhere, matching how a caller picks a branch it is not currently on.

    /// Creates `branch` off `HEAD`, commits on it, and switches back, leaving it diverged and
    /// checked out nowhere.
    fn branch_with_commit(dir: &Path, branch: &str, file: &str, contents: &str, message: &str) {
        let previous = String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .expect("git rev-parse")
                .stdout,
        )
        .trim()
        .to_string();
        git(dir, &["checkout", "-b", branch]);
        fs::write(dir.join(file), contents).expect("write");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
        git(dir, &["checkout", &previous]);
    }

    #[test]
    fn merging_a_branch_into_the_current_one_stays_uncommitted_until_complete_merge() {
        let repo = init_repo();
        branch_with_commit(
            repo.path(),
            "feature",
            "new.txt",
            "from feature\n",
            "feature commit",
        );

        let (start, outcome) = attempt_merge_into_current(repo.path(), "feature")
            .expect("attempt_merge_into_current should succeed");
        assert_eq!(
            start.base_branch, "main",
            "the branch merged into must be read from the target worktree's real HEAD"
        );
        assert_eq!(start.session_branch, "feature");
        assert_eq!(
            fs::canonicalize(&start.base_worktree_path).expect("canonicalize"),
            fs::canonicalize(repo.path()).expect("canonicalize")
        );
        let MergeOutcome::Clean { files } = outcome else {
            panic!("expected a clean merge, got {outcome:?}");
        };
        assert_eq!(files, vec![PathBuf::from("new.txt")]);

        assert!(
            merge_head_exists(repo.path()).expect("merge_head_exists"),
            "MERGE_HEAD must exist while the merge is uncommitted"
        );
        complete_merge(repo.path()).expect("complete_merge");
        assert_eq!(
            status(repo.path()),
            "",
            "working tree must be clean after completing"
        );
        assert_eq!(parent_count(repo.path(), "HEAD"), 2);
    }

    #[test]
    fn merging_a_diverged_branch_into_the_current_one_produces_a_real_three_way_merge_commit() {
        let repo = init_repo();
        branch_with_commit(
            repo.path(),
            "feature",
            "feature_only.txt",
            "feature work\n",
            "feature commit",
        );
        fs::write(repo.path().join("base_only.txt"), "base work\n").expect("write");
        git(repo.path(), &["add", "base_only.txt"]);
        git(repo.path(), &["commit", "-m", "base commit"]);

        let (_start, outcome) =
            attempt_merge_into_current(repo.path(), "feature").expect("attempt_merge_into_current");
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
    fn merging_a_conflicting_branch_into_the_current_one_reports_real_conflict_markers() {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        fs::write(repo.path().join("clean.txt"), "clean1\nclean2\n").expect("write");
        git(repo.path(), &["add", "shared.txt", "clean.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared/clean files"]);

        git(repo.path(), &["checkout", "-b", "feature"]);
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        fs::write(
            repo.path().join("clean.txt"),
            "clean1\nclean2 changed by feature\n",
        )
        .expect("write");
        git(
            repo.path(),
            &["commit", "-am", "feature changes shared.txt and clean.txt"],
        );
        git(repo.path(), &["checkout", "main"]);
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "main changes shared.txt"]);

        let (_start, outcome) =
            attempt_merge_into_current(repo.path(), "feature").expect("attempt_merge_into_current");
        let MergeOutcome::Conflicted {
            conflicted_files,
            clean_files,
        } = outcome
        else {
            panic!("expected a conflicted merge, got {outcome:?}");
        };
        assert_eq!(conflicted_files, vec![PathBuf::from("shared.txt")]);
        assert_eq!(clean_files, vec![PathBuf::from("clean.txt")]);

        let on_disk = fs::read_to_string(repo.path().join("shared.txt")).expect("read");
        assert!(on_disk.contains("<<<<<<< HEAD"));
        assert!(on_disk.contains("======="));
        assert!(on_disk.contains(">>>>>>> feature"));
        let clean_on_disk = fs::read_to_string(repo.path().join("clean.txt")).expect("read");
        assert_eq!(clean_on_disk, "clean1\nclean2 changed by feature\n");
    }

    #[test]
    fn merging_an_already_merged_branch_into_the_current_one_reports_a_real_no_op() {
        let repo = init_repo();
        branch_with_commit(
            repo.path(),
            "feature",
            "new.txt",
            "from feature\n",
            "feature commit",
        );

        let (_start, outcome) =
            attempt_merge_into_current(repo.path(), "feature").expect("attempt_merge_into_current");
        assert!(matches!(outcome, MergeOutcome::Clean { .. }));
        complete_merge(repo.path()).expect("complete_merge");
        let head_after_first_merge = rev_parse(repo.path(), "HEAD");

        let (_start, outcome) = attempt_merge_into_current(repo.path(), "feature")
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
    fn merging_the_current_branch_into_itself_is_refused() {
        let repo = init_repo();
        let err = attempt_merge_into_current(repo.path(), "main")
            .expect_err("merging the current branch into itself must be refused");
        match err {
            Error::MergeSourceIsCurrentBranch { branch } => assert_eq!(branch, "main"),
            other => panic!("expected Error::MergeSourceIsCurrentBranch, got {other:?}"),
        }
        assert!(
            !merge_head_exists(repo.path()).expect("merge_head_exists"),
            "no merge must have been started"
        );
    }

    #[test]
    fn merging_into_a_dirty_target_worktree_is_refused_before_touching_anything() {
        let repo = init_repo();
        branch_with_commit(
            repo.path(),
            "feature",
            "new.txt",
            "from feature\n",
            "feature commit",
        );
        fs::write(repo.path().join("base.txt"), "uncommitted change\n").expect("write");

        let err = attempt_merge_into_current(repo.path(), "feature")
            .expect_err("a dirty target worktree must be refused");
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
        assert!(
            !repo.path().join("new.txt").exists(),
            "the refused merge must not have brought the source branch's file across"
        );
    }

    #[test]
    fn merging_into_a_detached_target_worktree_is_refused() {
        let repo = init_repo();
        branch_with_commit(
            repo.path(),
            "feature",
            "new.txt",
            "from feature\n",
            "feature commit",
        );
        let detached = add_worktree_detached(repo.path(), "detached-wt");

        let err = attempt_merge_into_current(&detached, "feature")
            .expect_err("a detached target worktree must be refused");
        match err {
            Error::MergeTargetDetached { path } => assert_eq!(path, detached),
            other => panic!("expected Error::MergeTargetDetached, got {other:?}"),
        }
        assert!(
            !merge_head_exists(&detached).expect("merge_head_exists"),
            "no merge must have been started"
        );
    }

    /// A linked worktree on a detached `HEAD`.
    fn add_worktree_detached(repo_path: &Path, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().expect("utf8 path"),
                "main",
            ],
        );
        path
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
        // "before"=1, "<<<<<<< HEAD"=2, "ours line"=3, "======="=4, "theirs line"=5,
        // ">>>>>>> feature"=6, "after"=7.
        assert_eq!(hunk.ours_start_line, 3);
        assert_eq!(hunk.theirs_start_line, 5);
        assert_eq!(
            segments[2],
            ConflictSegment::Common(vec!["after".to_string()])
        );
    }

    #[test]
    fn parse_conflict_segments_handles_a_genuinely_empty_side() {
        let text = "<<<<<<< HEAD\n=======\ntheirs line\n>>>>>>> feature\n";
        let segments = parse_conflict_segments(text, Path::new("f.txt")).expect("parse");
        let ConflictSegment::Conflict(hunk) = &segments[0] else {
            panic!("expected a conflict segment");
        };
        assert!(hunk.ours.is_empty());
        assert_eq!(hunk.theirs, vec!["theirs line".to_string()]);
        assert_eq!(
            hunk.ours_start_line, 2,
            "with zero ours lines, ours_start_line lands on the ======= line itself - the \
             documented, honest 'meaningless when empty' case"
        );
        assert_eq!(hunk.theirs_start_line, 3);
    }

    #[test]
    fn parse_conflict_segments_computes_real_start_lines_across_multiple_hunks() {
        let text = "line1\n\
<<<<<<< HEAD\n\
ours a\n\
ours b\n\
=======\n\
theirs a\n\
theirs b\n\
theirs c\n\
>>>>>>> feature\n\
line2\n\
line3\n\
<<<<<<< HEAD\n\
second ours\n\
=======\n\
second theirs a\n\
second theirs b\n\
>>>>>>> feature\n\
line4\n";
        let segments = parse_conflict_segments(text, Path::new("f.txt")).expect("parse");
        let hunks: Vec<&ConflictHunk> = segments
            .iter()
            .filter_map(|segment| match segment {
                ConflictSegment::Conflict(hunk) => Some(hunk),
                ConflictSegment::Common(_) => None,
            })
            .collect();
        assert_eq!(hunks.len(), 2);

        // First hunk: "line1"=1, "<<<<<<< HEAD"=2, "ours a"=3, "ours b"=4, "======="=5,
        // "theirs a"=6, "theirs b"=7, "theirs c"=8, ">>>>>>> feature"=9.
        assert_eq!(
            hunks[0].ours,
            vec!["ours a".to_string(), "ours b".to_string()]
        );
        assert_eq!(hunks[0].ours_start_line, 3);
        assert_eq!(
            hunks[0].theirs,
            vec![
                "theirs a".to_string(),
                "theirs b".to_string(),
                "theirs c".to_string()
            ]
        );
        assert_eq!(hunks[0].theirs_start_line, 6);

        // Second hunk: "line2"=10, "line3"=11, "<<<<<<< HEAD"=12, "second ours"=13,
        // "======="=14, "second theirs a"=15, "second theirs b"=16, ">>>>>>> feature"=17.
        assert_eq!(hunks[1].ours, vec!["second ours".to_string()]);
        assert_eq!(hunks[1].ours_start_line, 13);
        assert_eq!(
            hunks[1].theirs,
            vec!["second theirs a".to_string(), "second theirs b".to_string()]
        );
        assert_eq!(hunks[1].theirs_start_line, 15);
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
                ours_start_line: 2,
                theirs_label: "feature".to_string(),
                theirs: vec!["theirs".to_string()],
                theirs_start_line: 4,
            })],
            trailing_newline: true,
        };
        let err = write_resolved_file(dir.path(), &file)
            .expect_err("a file with unresolved conflicts must be refused");
        assert!(matches!(err, Error::MergeFileNotFullyResolved { .. }));
    }

    #[test]
    fn non_ascii_filename_round_trips_through_the_real_conflict_and_resolution_pipeline() {
        // Without `core.quotePath=false`, a non-ASCII path comes back octal-escaped and quoted,
        // which `parse_paths` takes literally: the load fails and the write creates a new,
        // wrongly-named file - quote marks included - in the worktree.
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
        assert_eq!(conflicted_files, vec![PathBuf::from("café.txt")]);

        let mut file = load_conflicted_file(repo.path(), &conflicted_files[0])
            .expect("load_conflicted_file must find the real café.txt, not a mangled path");
        resolve_hunk(&mut file, 1, ConflictChoice::Both).expect("resolve_hunk");
        write_resolved_file(repo.path(), &file).expect("write_resolved_file");

        assert_eq!(
            fs::read_to_string(repo.path().join("café.txt")).expect("read café.txt"),
            "line1\nBASE CHANGED\nFEATURE CHANGED\nline3\n"
        );
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

        git(repo.path(), &["rm", "-q", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "base deletes shared.txt"]);
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

        let err = complete_merge(repo.path())
            .expect_err("complete_merge must refuse while shared.txt is still really unmerged");
        assert!(matches!(err, Error::MergeFilesStillConflicted { .. }));

        abort_merge(repo.path()).expect("abort_merge");
    }

    #[test]
    fn binary_conflict_with_a_nul_byte_is_classified_as_unmergeable_not_falsely_resolved() {
        // A NUL is valid UTF-8, so `String::from_utf8` cannot detect this - but git's own
        // heuristic scans for embedded NULs, treats the file as binary, and leaves no markers.
        // That is what makes this distinct from a resolved text file.
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
