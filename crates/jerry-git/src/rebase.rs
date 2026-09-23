//! Headless `git rebase --interactive`: the engine, with no UI.
//!
//! Real `git rebase` driven non-interactively through `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR`, pointed
//! at hidden `jerry git-sequence-editor`/`jerry git-editor` subcommands - see
//! `docs/architecture/decisions.md` §7 for the mechanism, §20 for the Commands built on it.
//!
//! Sidecar state lives under `<git-dir>/ade-rebase/` until the rebase completes or aborts, so
//! [`rebase_status`] can reconstruct a stop after a process restart.
//!
//! Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, GitExit};
use crate::{absolutize, check_success, format_args, git_command, is_dirty, run_git};

/// One of git's six interactive-rebase todo verbs, with a pre-supplied message added to `reword`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    /// `Some(message)` runs straight through with no stop; `None` stops exactly as
    /// [`RebaseAction::Edit`] does, by letting git's message editor fail.
    Reword(Option<String>),
    /// Always stops after applying the commit; git's native behaviour, no editor involved.
    Edit,
    /// Folds into the previous row, always keeping git's default combined message.
    Squash,
    /// Folds into the previous row, discarding this commit's message. Never opens an editor.
    Fixup,
    /// Removes this commit from history entirely.
    Drop,
}

impl RebaseAction {
    /// The literal verb git's todo format expects.
    fn todo_verb(&self) -> &'static str {
        match self {
            RebaseAction::Pick => "pick",
            RebaseAction::Reword(_) => "reword",
            RebaseAction::Edit => "edit",
            RebaseAction::Squash => "squash",
            RebaseAction::Fixup => "fixup",
            RebaseAction::Drop => "drop",
        }
    }

    /// The tag persisted in `commits.txt`. Differs from [`Self::todo_verb`] only for `Reword`,
    /// where a later stop must be cross-referenced against whether a message was supplied.
    fn state_tag(&self) -> &'static str {
        match self {
            RebaseAction::Pick => "pick",
            RebaseAction::Edit => "edit",
            RebaseAction::Squash => "squash",
            RebaseAction::Fixup => "fixup",
            RebaseAction::Drop => "drop",
            RebaseAction::Reword(Some(_)) => "reword-msg",
            RebaseAction::Reword(None) => "reword-nomsg",
        }
    }
}

/// One row of a rebase plan. A full plan is given oldest-first, matching git's generated todo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePlanEntry {
    /// A full object id, not abbreviated.
    pub commit: String,
    pub action: RebaseAction,
}

/// Why a [`RebaseOutcome::StoppedForEdit`] happened, recovered from the persisted plan - git
/// treats both cases identically on disk.
///
/// `None` when the stopped commit is not in the plan (state already cleaned up, or the rebase was
/// not started by this module), never defaulted to [`StopReason::Edit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The stopped row was [`RebaseAction::Edit`].
    Edit,
    /// The stopped row was [`RebaseAction::Reword`] with no supplied message.
    RewordNeedsMessage,
}

/// The result of driving one `git rebase` invocation.
///
/// No `Aborted` variant: [`abort_rebase`] does not drive the plan forward, and folding it in here
/// would imply a continuity that does not survive `git rebase --abort`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The whole plan applied; the state directory is already cleaned up.
    Completed,
    /// A deliberate, non-conflict stop at an `edit` or message-less `reword` row. Resolve with
    /// [`continue_rebase`] or [`skip_rebase_commit`], as the command line would.
    StoppedForEdit {
        commit: String,
        reason: Option<StopReason>,
    },
    /// A conflict: the worktree keeps its markers and `REBASE_HEAD` for the caller to resolve.
    /// `commit` is the commit that failed to apply.
    StoppedForConflict {
        commit: String,
        conflicted_files: Vec<PathBuf>,
    },
}

/// The on-disk state of a rebase stopped mid-flight, read from `.git/rebase-merge/` rather than
/// cached, so it survives a process restart.
///
/// Every field but `conflicted_files` is `None` when git's own state files do not expose it,
/// never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStatus {
    /// The commit the rebase is replaying onto (`.git/rebase-merge/onto`), if readable.
    pub onto: Option<String>,
    /// 1-indexed position of the current/next step (`.git/rebase-merge/msgnum`), if readable.
    pub current_step: Option<usize>,
    /// Total number of steps in the plan (`.git/rebase-merge/end`), if readable.
    pub total_steps: Option<usize>,
    /// The commit stopped at, present for both a deliberate stop and a conflict.
    pub stopped_commit: Option<String>,
    /// Unresolved conflicted files. Non-empty is what distinguishes a conflict from a stop.
    pub conflicted_files: Vec<PathBuf>,
    /// Always `None` when `conflicted_files` is non-empty.
    pub stop_reason: Option<StopReason>,
}

/// The sidecar state directory, under the worktree's own administrative directory.
///
/// Not under `.git/rebase-merge/`, which git may not have created yet when this starts writing,
/// and not the shared common dir, since rebase state is per-worktree.
const STATE_DIR_NAME: &str = "ade-rebase";

fn state_dir(git_dir: &Path) -> PathBuf {
    git_dir.join(STATE_DIR_NAME)
}

/// Resolves `worktree_path`'s own administrative directory, which for a linked worktree is
/// `<common-dir>/worktrees/<name>` - distinct from [`crate::git_common_dir`]'s shared one.
fn worktree_git_dir(worktree_path: &Path) -> Result<PathBuf, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "--git-dir".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(absolutize(Path::new(raw.trim()), worktree_path))
}

/// POSIX-single-quotes `path` for embedding in a `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR` value.
///
/// git splices those values verbatim into `sh -c "<value> \"$@\""` rather than treating them as
/// already-quoted arguments, so a path containing a space would otherwise be word-split.
fn shell_single_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// One `GIT_EDITOR` invocation's outcome, decided from the message file's own content plus the
/// reword queue's current cursor - never by invocation order (decisions §7). Pure: [`run_editor`]
/// performs every read/write this implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// A squash message combination: accept git's own combined message unmodified.
    AcceptUnmodified,
    /// A genuine reword: replace the message file with queue slot `slot_index`, if one was
    /// queued. Either way the cursor advances past it - each reword invocation consumes exactly
    /// one slot even when it stops for a missing one.
    Reword { slot_index: usize },
    /// A conflict-resumed step, or an `edit` stop's own ordinary commit: accept git's pre-filled
    /// message and never touch the queue.
    AcceptUnmodifiedNoAdvance,
}

/// Classifies one `GIT_EDITOR` invocation from the message file's own content, per
/// `docs/architecture/decisions.md` §7's three cases - never by invocation order.
pub fn classify_editor_invocation(message: &str, cursor: usize) -> EditorAction {
    if message
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("# This is a combination of"))
    {
        return EditorAction::AcceptUnmodified;
    }
    if message.contains("You are currently editing a commit") {
        return EditorAction::Reword { slot_index: cursor };
    }
    EditorAction::AcceptUnmodifiedNoAdvance
}

/// Drives one real `GIT_EDITOR` invocation: reads `message_file_path`'s content and the sidecar's
/// reword cursor, classifies it ([`classify_editor_invocation`]), and applies whatever that
/// implies. Returns `false` exactly when git's own `edit`-stop behaviour should be reproduced (a
/// message-less reword with nothing queued) - `jerry git-editor` exits non-zero for that, matching
/// the former `/bin/sh` script's `exit 1`.
pub fn run_editor(worktree_path: &Path, message_file_path: &Path) -> Result<bool, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    let dir = state_dir(&git_dir);
    let message = fs::read_to_string(message_file_path)?;
    let cursor = read_trimmed(&dir.join("cursor"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    match classify_editor_invocation(&message, cursor) {
        EditorAction::AcceptUnmodified | EditorAction::AcceptUnmodifiedNoAdvance => Ok(true),
        EditorAction::Reword { slot_index } => {
            fs::write(dir.join("cursor"), (slot_index + 1).to_string())?;
            let slot = dir.join("queue").join(slot_index.to_string());
            if !slot.is_file() {
                return Ok(false);
            }
            fs::copy(&slot, message_file_path)?;
            Ok(true)
        }
    }
}

/// Drives one real `GIT_SEQUENCE_EDITOR` invocation: copies the prepared todo
/// ([`write_plan_state`]'s `todo.txt`) over git's own freshly generated one at
/// `generated_todo_path`.
pub fn run_sequence_editor(worktree_path: &Path, generated_todo_path: &Path) -> Result<(), Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    let prepared = state_dir(&git_dir).join("todo.txt");
    fs::copy(&prepared, generated_todo_path)?;
    Ok(())
}

/// Writes `plan`'s todo file, reword-message queue and `commits.txt` cross-reference into a fresh
/// `dir`, returning the todo file's path.
fn write_plan_state(dir: &Path, plan: &[RebasePlanEntry]) -> Result<PathBuf, Error> {
    fs::create_dir_all(dir)?;
    let queue_dir = dir.join("queue");
    fs::create_dir_all(&queue_dir)?;

    let mut todo = String::new();
    let mut commits = String::new();
    let mut reword_index = 0usize;

    for entry in plan {
        todo.push_str(entry.action.todo_verb());
        todo.push(' ');
        todo.push_str(&entry.commit);
        todo.push('\n');

        if let RebaseAction::Reword(message) = &entry.action {
            if let Some(message) = message {
                fs::write(queue_dir.join(reword_index.to_string()), message)?;
            }
            reword_index += 1;
        }

        commits.push_str(&entry.commit);
        commits.push(' ');
        commits.push_str(entry.action.state_tag());
        commits.push('\n');
    }

    let todo_path = dir.join("todo.txt");
    fs::write(&todo_path, todo)?;
    fs::write(dir.join("commits.txt"), commits)?;

    Ok(todo_path)
}

/// Removes the state directory, idempotently, once a rebase finishes or is aborted.
fn cleanup_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(state_dir(git_dir));
}

/// Reads `stopped-sha`, or `None` if absent or empty.
///
/// Populated identically for a conflict and a deliberate stop, so callers read it unconditionally
/// and branch on [`conflicted_files`] instead.
fn read_stopped_sha(git_dir: &Path) -> Option<String> {
    read_trimmed(&git_dir.join("rebase-merge").join("stopped-sha"))
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Recovers the [`StopReason`] for a stop at `commit` from the persisted `commits.txt`. `None`
/// if that file is missing or does not list `commit`.
fn lookup_stop_reason(git_dir: &Path, commit: &str) -> Option<StopReason> {
    let content = fs::read_to_string(state_dir(git_dir).join("commits.txt")).ok()?;
    for line in content.lines() {
        let (sha, tag) = line.split_once(' ')?;
        if sha == commit {
            return match tag {
                "edit" => Some(StopReason::Edit),
                "reword-nomsg" => Some(StopReason::RewordNeedsMessage),
                // "reword-msg" never stops, so there is no reason to report if this is reached.
                _ => None,
            };
        }
    }
    None
}

/// The unresolved conflicted paths, with `core.quotePath=false` pinned so a non-ASCII path does
/// not come back octal-escaped and mismatch the file on disk.
fn conflicted_files(worktree_path: &Path) -> Result<Vec<PathBuf>, Error> {
    let args: Vec<OsString> = vec![
        "-c".into(),
        "core.quotePath=false".into(),
        "diff".into(),
        "--name-only".into(),
        "--diff-filter=U".into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Interprets one `git rebase`/`--continue`/`--skip` invocation into a [`RebaseOutcome`].
///
/// Does not branch on exit status: a deliberate stop's exit code depends on how it was reached -
/// a final `edit` row exits 0, while a `reword` stopped by the editor script's own failure exits 1.
/// The reliable signal is whether `.git/rebase-merge/` still exists, which git removes only on
/// completion or abort.
fn interpret_result(
    worktree_path: &Path,
    git_dir: &Path,
    args: &[OsString],
    output: &std::process::Output,
) -> Result<RebaseOutcome, Error> {
    if !git_dir.join("rebase-merge").is_dir() {
        if output.status.success() {
            cleanup_state(git_dir);
            return Ok(RebaseOutcome::Completed);
        }
        // No rebase state left, yet the command failed: an unexpected error, not a stop.
        return Err(Error::GitCommand {
            args: format_args(args),
            exit: GitExit::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    // Still stopped mid-rebase, regardless of exit code.
    let stopped_commit = read_stopped_sha(git_dir);
    let conflicted = conflicted_files(worktree_path)?;

    if !conflicted.is_empty() {
        return Ok(RebaseOutcome::StoppedForConflict {
            commit: stopped_commit.unwrap_or_default(),
            conflicted_files: conflicted,
        });
    }

    if let Some(commit) = stopped_commit {
        let reason = lookup_stop_reason(git_dir, &commit);
        return Ok(RebaseOutcome::StoppedForEdit { commit, reason });
    }

    // State exists but reports neither a conflict nor a stop: an unexpected failure.
    Err(Error::GitCommand {
        args: format_args(args),
        exit: GitExit::from_status(&output.status),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Every commit `git rebase -i <onto>` would default to picking, oldest first.
///
/// Building the initial plan is the caller's job: [`start_interactive_rebase`] overwrites git's
/// autogenerated todo rather than reading it, so nothing derives this internally.
pub fn commits_to_rebase(worktree_path: &Path, onto: &str) -> Result<Vec<String>, Error> {
    let args: Vec<OsString> = vec![
        "rev-list".into(),
        "--reverse".into(),
        format!("{onto}..HEAD").into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect())
}

/// Every check [`start_interactive_rebase`] makes before touching git, split out so a caller
/// (the `RebaseStart` Command's own `validate`) can ask "would this run" without running it -
/// mirrors `jerry_git::merge::merge_preflight`'s own split from `attempt_merge`.
///
/// A stale, uncleaned-up `<git-dir>/ade-rebase/` from a previous run is not a reason to refuse
/// (see [`start_interactive_rebase`]'s own handling of it).
pub fn rebase_preflight(worktree_path: &Path) -> Result<(), Error> {
    if rebase_status(worktree_path)?.is_some() {
        return Err(Error::RebaseAlreadyInProgress {
            path: worktree_path.to_path_buf(),
        });
    }
    if is_dirty(worktree_path)? {
        return Err(Error::RebaseWorktreeDirty {
            path: worktree_path.to_path_buf(),
        });
    }
    Ok(())
}

/// Rebases the current branch onto `onto`, driving `plan` to completion or the first stop.
///
/// `plan` is oldest-first and is not reversed here. `jerry_binary` is spliced into
/// `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR` as `<jerry_binary> git-sequence-editor`/`<jerry_binary>
/// git-editor` - the caller's job to locate (the app via `crates/jerry-app/src/host.rs::
/// find_jerry_binary`, the CLI via `std::env::current_exe`), since this crate has no opinion on
/// installation layout.
pub fn start_interactive_rebase(
    worktree_path: &Path,
    onto: &str,
    plan: &[RebasePlanEntry],
    jerry_binary: &Path,
) -> Result<RebaseOutcome, Error> {
    rebase_preflight(worktree_path)?;
    let git_dir = worktree_git_dir(worktree_path)?;
    let dir = state_dir(&git_dir);
    // Clear any stale state from a previous, uncleaned-up run before starting fresh.
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    write_plan_state(&dir, plan)?;
    let jerry_quoted = shell_single_quote(jerry_binary);
    // Persisted, not just set on this invocation's env: `run_rebase_step` needs the exact same
    // `GIT_EDITOR` value for a later `--continue`/`--skip`, potentially after a process restart.
    let editor_command = format!("{jerry_quoted} git-editor");
    fs::write(dir.join("editor-command"), &editor_command)?;

    let args: Vec<OsString> = vec!["rebase".into(), "-i".into(), onto.into()];
    let mut command = git_command(worktree_path, &args);
    command.env(
        "GIT_SEQUENCE_EDITOR",
        format!("{jerry_quoted} git-sequence-editor"),
    );
    command.env("GIT_EDITOR", &editor_command);
    let output = command.output().map_err(|source| Error::GitSpawn {
        args: format_args(&args),
        source,
    })?;

    interpret_result(worktree_path, &git_dir, &args, &output)
}

/// Amends `HEAD`'s message, for applying a message obtained *after* a message-less `reword` stop.
///
/// The reword queue is fixed at [`start_interactive_rebase`] time, so a late message can only be
/// applied this way, not by feeding the queue retroactively.
///
/// Two guards, both refusals rather than silent corruption: `expected_head_original` must still
/// be `HEAD` ([`Error::RebaseAmendHeadMoved`]), catching a stale or double-continued caller; and
/// the index must be clean ([`Error::RebaseAmendIndexDirty`]), so unrelated staged content does
/// not get folded into the amended commit.
pub fn amend_head_message(
    worktree_path: &Path,
    expected_head_original: &str,
    message: &str,
) -> Result<(), Error> {
    let head_args: Vec<OsString> = vec!["rev-parse".into(), "HEAD".into()];
    let head_output = run_git(worktree_path, &head_args)?;
    check_success(&head_args, &head_output)?;
    let actual_head = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();
    if actual_head != expected_head_original {
        return Err(Error::RebaseAmendHeadMoved {
            path: worktree_path.to_path_buf(),
            expected: expected_head_original.to_string(),
            actual: actual_head,
        });
    }

    let diff_args: Vec<OsString> = vec!["diff".into(), "--cached".into(), "--quiet".into()];
    let diff_output = run_git(worktree_path, &diff_args)?;
    match diff_output.status.code() {
        // `--quiet` exits 0 with no staged diff and 1 with one.
        Some(0) => {}
        Some(1) => {
            return Err(Error::RebaseAmendIndexDirty {
                path: worktree_path.to_path_buf(),
            });
        }
        _ => {
            return Err(Error::GitCommand {
                args: format_args(&diff_args),
                exit: GitExit::from_status(&diff_output.status),
                stderr: String::from_utf8_lossy(&diff_output.stderr).into_owned(),
            });
        }
    }

    let args: Vec<OsString> = vec![
        "commit".into(),
        "--amend".into(),
        "-m".into(),
        message.into(),
    ];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Drives one `git rebase --continue`/`--skip` with the persisted `GIT_EDITOR` value active,
/// since a later row may still need it.
///
/// Falls back to `GIT_EDITOR=true` when that state is gone, so this can never be left waiting on
/// an interactive editor that will never open.
fn run_rebase_step(
    worktree_path: &Path,
    git_dir: &Path,
    extra_arg: &str,
) -> Result<RebaseOutcome, Error> {
    let editor_command = read_trimmed(&state_dir(git_dir).join("editor-command"));
    let args: Vec<OsString> = vec!["rebase".into(), extra_arg.into()];
    let mut command = git_command(worktree_path, &args);
    match editor_command {
        Some(value) => command.env("GIT_EDITOR", value),
        None => command.env("GIT_EDITOR", "true"),
    };
    let output = command.output().map_err(|source| Error::GitSpawn {
        args: format_args(&args),
        source,
    })?;

    interpret_result(worktree_path, git_dir, &args, &output)
}

/// Resumes a stopped rebase, driving it to completion or the next stop.
pub fn continue_rebase(worktree_path: &Path) -> Result<RebaseOutcome, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    run_rebase_step(worktree_path, &git_dir, "--continue")
}

/// Skips the commit a stopped rebase is at, driving it to completion or the next stop.
pub fn skip_rebase_commit(worktree_path: &Path) -> Result<RebaseOutcome, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    run_rebase_step(worktree_path, &git_dir, "--skip")
}

/// Aborts an in-progress rebase, restoring the pre-rebase state and cleaning up.
pub fn abort_rebase(worktree_path: &Path) -> Result<(), Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    let args: Vec<OsString> = vec!["rebase".into(), "--abort".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    cleanup_state(&git_dir);
    Ok(())
}

/// The on-disk state of a stopped rebase, or `Ok(None)` if none is in progress.
pub fn rebase_status(worktree_path: &Path) -> Result<Option<RebaseStatus>, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    let rebase_merge = git_dir.join("rebase-merge");
    if !rebase_merge.is_dir() {
        return Ok(None);
    }

    let onto = read_trimmed(&rebase_merge.join("onto"));
    let current_step = read_trimmed(&rebase_merge.join("msgnum")).and_then(|s| s.parse().ok());
    let total_steps = read_trimmed(&rebase_merge.join("end")).and_then(|s| s.parse().ok());
    let stopped_commit = read_stopped_sha(&git_dir);
    let conflicted_files = conflicted_files(worktree_path)?;
    let stop_reason = if conflicted_files.is_empty() {
        stopped_commit
            .as_deref()
            .and_then(|commit| lookup_stop_reason(&git_dir, commit))
    } else {
        None
    };

    Ok(Some(RebaseStatus {
        onto,
        current_step,
        total_steps,
        stopped_commit,
        conflicted_files,
        stop_reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use test_support::{git, git_output, seed_empty_repo};

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) -> String {
        fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
        git_output(dir, &["rev-parse", "HEAD"])
    }

    fn commit_message(dir: &Path, commit: &str) -> String {
        git_output(dir, &["log", "-1", "--format=%B", commit])
    }

    // --- shell_single_quote (pure) ----------------------------------------------------------

    #[test]
    fn shell_single_quote_wraps_a_plain_path() {
        assert_eq!(shell_single_quote(Path::new("/a/b")), "'/a/b'");
    }

    #[test]
    fn shell_single_quote_escapes_an_embedded_single_quote() {
        assert_eq!(
            shell_single_quote(Path::new("/a/it's/b")),
            "'/a/it'\\''s/b'"
        );
    }

    // --- commits_to_rebase --------------------------------------------------------------------

    #[test]
    fn commits_to_rebase_lists_commits_oldest_first_excluding_onto_itself() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "b.txt", "2", "commit 2");
        let c3 = commit(repo.path(), "c.txt", "3", "commit 3");

        let commits = commits_to_rebase(repo.path(), &base).expect("commits_to_rebase");
        assert_eq!(commits, vec![c1, c2, c3]);
    }

    #[test]
    fn commits_to_rebase_is_empty_when_onto_is_head_itself() {
        let repo = seed_empty_repo();
        commit(repo.path(), "base.txt", "base", "base");
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);

        let commits = commits_to_rebase(repo.path(), &head).expect("commits_to_rebase");
        assert!(commits.is_empty());
    }

    // --- amend_head_message ------------------------------------------------------------------
    //
    // Every test that drives a real `start_interactive_rebase`/`continue_rebase`/
    // `skip_rebase_commit` call (needing a genuinely executable `GIT_SEQUENCE_EDITOR`/
    // `GIT_EDITOR`) lives in `tests/rebase_editor.rs` instead, against this crate's own
    // `jerry_git_test_editor` `[[bin]]` - `CARGO_BIN_EXE_<name>` is only reliable for an
    // integration test, not a `--lib` unit test (verified against this crate's own build: the
    // identical `--lib` setup here failed to see the env var at all, even for a same-package
    // `[[bin]]`, let alone a dev-dependency's).

    #[test]
    fn amend_head_message_replaces_the_real_committed_message() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "original message");
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);

        amend_head_message(repo.path(), &head, "a real new message").expect("amend_head_message");

        let new_head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(commit_message(repo.path(), &new_head), "a real new message");
    }

    #[test]
    fn amend_head_message_refuses_when_head_has_moved_since_expected() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "original message");
        let stale_expected = "0000000000000000000000000000000000000000".to_string();

        let err = amend_head_message(repo.path(), &stale_expected, "a real new message")
            .expect_err("HEAD no longer matching the expected commit must refuse");
        assert!(
            matches!(err, Error::RebaseAmendHeadMoved { .. }),
            "expected RebaseAmendHeadMoved, got a different error: {err:?}"
        );

        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(commit_message(repo.path(), &head), "original message");
    }

    #[test]
    fn amend_head_message_refuses_when_the_real_index_has_staged_changes() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "original message");
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);

        fs::write(repo.path().join("sneaky.txt"), "sneaky").expect("write sneaky.txt");
        git(repo.path(), &["add", "sneaky.txt"]);

        let err = amend_head_message(repo.path(), &head, "a real new message")
            .expect_err("real staged changes must refuse the amend");
        assert!(
            matches!(err, Error::RebaseAmendIndexDirty { .. }),
            "expected RebaseAmendIndexDirty, got a different error: {err:?}"
        );

        let after_head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(after_head, head);
        assert_eq!(commit_message(repo.path(), &head), "original message");
        let sneaky_in_head_tree = Command::new("git")
            .current_dir(repo.path())
            .args(["cat-file", "-e", "HEAD:sneaky.txt"])
            .status()
            .expect("failed to spawn git")
            .success();
        assert!(
            !sneaky_in_head_tree,
            "the sneaky staged file must never have been folded into HEAD"
        );
    }

    // --- rebase_status -------------------------------------------------------------------
    //
    // Every other case needs a real `start_interactive_rebase` call and so lives in
    // `tests/rebase_editor.rs` alongside every other test that does (see this module's own note
    // above `amend_head_message`'s tests).

    #[test]
    fn rebase_status_is_none_when_no_rebase_is_in_progress() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "commit 1");
        assert_eq!(rebase_status(repo.path()).expect("rebase_status"), None);
    }

    // --- rebase_preflight -------------------------------------------------------------------
    //
    // The "already in progress" case needs a real `start_interactive_rebase` call and so lives
    // in `tests/rebase_editor.rs` alongside every other test that does.

    #[test]
    fn rebase_preflight_allows_a_stale_uncleaned_sidecar_directory() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        commit(repo.path(), "a.txt", "1", "commit 1");
        let git_dir = worktree_git_dir(repo.path()).expect("git dir");
        fs::create_dir_all(state_dir(&git_dir)).expect("stale sidecar dir");

        rebase_preflight(repo.path()).expect(
            "a leftover ade-rebase/ directory with no real git rebase-merge/ must not refuse",
        );
        let _ = base;
    }

    // --- classify_editor_invocation (pure) ----------------------------------------------------

    #[test]
    fn a_squash_combination_message_is_accepted_unmodified() {
        let message = "# This is a combination of 2 commits.\nsome body\n";
        assert_eq!(
            classify_editor_invocation(message, 3),
            EditorAction::AcceptUnmodified
        );
    }

    #[test]
    fn a_reword_invocation_reads_the_current_cursor_as_its_slot() {
        let message = "original subject\n\nYou are currently editing a commit while rebasing.\n";
        assert_eq!(
            classify_editor_invocation(message, 2),
            EditorAction::Reword { slot_index: 2 }
        );
    }

    #[test]
    fn a_conflict_resumed_step_never_advances_the_queue() {
        let message = "commit v1\n";
        assert_eq!(
            classify_editor_invocation(message, 5),
            EditorAction::AcceptUnmodifiedNoAdvance
        );
    }

    // --- run_editor / run_sequence_editor (impure, real sidecar files) -----------------------

    #[test]
    fn run_editor_pops_a_queued_reword_message_and_advances_the_cursor() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "commit 1");
        let git_dir = worktree_git_dir(repo.path()).expect("git dir");
        let dir = state_dir(&git_dir);
        fs::create_dir_all(dir.join("queue")).expect("queue dir");
        fs::write(dir.join("queue").join("0"), "a real queued message").expect("write slot");

        let message_file = repo.path().join("MSG");
        fs::write(
            &message_file,
            "original\n\nYou are currently editing a commit while rebasing branch 'x'.\n",
        )
        .expect("write message file");

        let accepted = run_editor(repo.path(), &message_file).expect("run_editor");
        assert!(accepted);
        assert_eq!(
            fs::read_to_string(&message_file).expect("read message"),
            "a real queued message"
        );
        assert_eq!(
            read_trimmed(&dir.join("cursor")).as_deref(),
            Some("1"),
            "the cursor must advance past the slot it just consumed"
        );
    }

    #[test]
    fn run_editor_reproduces_edit_stop_when_no_message_is_queued() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "commit 1");
        let git_dir = worktree_git_dir(repo.path()).expect("git dir");
        let dir = state_dir(&git_dir);
        fs::create_dir_all(dir.join("queue")).expect("queue dir");

        let message_file = repo.path().join("MSG");
        fs::write(
            &message_file,
            "You are currently editing a commit while rebasing branch 'x'.\n",
        )
        .expect("write message file");

        let accepted = run_editor(repo.path(), &message_file).expect("run_editor");
        assert!(
            !accepted,
            "nothing queued must reproduce git's own edit-stop by refusing"
        );
        assert_eq!(read_trimmed(&dir.join("cursor")).as_deref(), Some("1"));
    }

    #[test]
    fn run_editor_never_touches_the_cursor_for_a_conflict_resumed_step() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "commit 1");
        let git_dir = worktree_git_dir(repo.path()).expect("git dir");
        let dir = state_dir(&git_dir);
        fs::create_dir_all(&dir).expect("sidecar dir");
        fs::write(dir.join("cursor"), "3").expect("seed cursor");

        let message_file = repo.path().join("MSG");
        fs::write(&message_file, "commit v1\n").expect("write message file");

        let accepted = run_editor(repo.path(), &message_file).expect("run_editor");
        assert!(accepted);
        assert_eq!(
            fs::read_to_string(&message_file).expect("read message"),
            "commit v1\n",
            "git's own pre-filled message must be left untouched"
        );
        assert_eq!(
            read_trimmed(&dir.join("cursor")).as_deref(),
            Some("3"),
            "a conflict-resumed step must never advance the reword cursor"
        );
    }

    #[test]
    fn run_sequence_editor_copies_the_prepared_todo_over_gits_generated_one() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "commit 1");
        let git_dir = worktree_git_dir(repo.path()).expect("git dir");
        let dir = state_dir(&git_dir);
        fs::create_dir_all(&dir).expect("sidecar dir");
        fs::write(dir.join("todo.txt"), "pick deadbeef commit 1\n").expect("write prepared todo");

        let generated = repo.path().join("git-rebase-todo");
        fs::write(&generated, "pick deadbeef some other subject\n").expect("write git's own");

        run_sequence_editor(repo.path(), &generated).expect("run_sequence_editor");
        assert_eq!(
            fs::read_to_string(&generated).expect("read generated"),
            "pick deadbeef commit 1\n"
        );
    }
}
