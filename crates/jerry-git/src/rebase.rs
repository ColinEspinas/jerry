//! Headless `git rebase --interactive`: the engine, with no UI.
//!
//! Real `git rebase` driven non-interactively through `GIT_SEQUENCE_EDITOR` and `GIT_EDITOR` - the
//! same hooks a human's `$EDITOR` goes through, pointed at scripts this module writes. See
//! `docs/architecture/decisions.md` §7 for the mechanism and why each part of it is load-bearing.
//!
//! Sidecar state lives under `<git-dir>/ade-rebase/` until the rebase completes or aborts, so
//! [`rebase_status`] can reconstruct a stop after a process restart.
//!
//! Unix-only: the editor script is `/bin/sh`. Performs blocking I/O; see the crate-level docs.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, GitExit};
use crate::{absolutize, check_success, format_args, git_command, run_git};

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

/// Template for [`write_editor_script`]'s `GIT_EDITOR` script; its three cases are set out in
/// `docs/architecture/decisions.md` §7.
const EDITOR_SCRIPT_TEMPLATE: &str = r#"#!/bin/sh
set -eu
QUEUE_DIR=__QUEUE_DIR__
CURSOR_FILE=__CURSOR_FILE__
TARGET="$1"

# Squash message-combination: accept git's own default combined message unmodified.
if head -n 1 -- "$TARGET" | grep -q '^# This is a combination of'; then
    exit 0
fi

# A genuine reword invocation: consume the next queued slot.
if grep -q 'You are currently editing a commit' -- "$TARGET"; then
    if [ -f "$CURSOR_FILE" ]; then
        CURSOR=$(cat -- "$CURSOR_FILE")
    else
        CURSOR=0
    fi
    NEXT=$((CURSOR + 1))
    printf '%s' "$NEXT" > "$CURSOR_FILE"
    SLOT="$QUEUE_DIR/$CURSOR"
    if [ -f "$SLOT" ]; then
        cp -- "$SLOT" "$TARGET"
        exit 0
    fi
    exit 1
fi

# Anything else (e.g. a plain pick/fixup resumed after a real conflict, which git re-commits
# through the ordinary editor-invoking codepath) - accept the pre-filled default, unmodified,
# and never touch the reword queue's cursor.
exit 0
"#;

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), Error> {
    Ok(())
}

/// Writes the `GIT_EDITOR` script into `dir` and returns its path.
fn write_editor_script(dir: &Path) -> Result<PathBuf, Error> {
    let script_path = dir.join("editor.sh");
    let queue_dir = dir.join("queue");
    let cursor_file = dir.join("cursor");
    let script = EDITOR_SCRIPT_TEMPLATE
        .replace("__QUEUE_DIR__", &shell_single_quote(&queue_dir))
        .replace("__CURSOR_FILE__", &shell_single_quote(&cursor_file));
    fs::write(&script_path, script)?;
    make_executable(&script_path)?;
    Ok(script_path)
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

/// Rebases the current branch onto `onto`, driving `plan` to completion or the first stop.
///
/// `plan` is oldest-first and is not reversed here.
pub fn start_interactive_rebase(
    worktree_path: &Path,
    onto: &str,
    plan: &[RebasePlanEntry],
) -> Result<RebaseOutcome, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    let dir = state_dir(&git_dir);
    // Clear any stale state from a previous, uncleaned-up run before starting fresh.
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    let todo_path = write_plan_state(&dir, plan)?;
    let editor_path = write_editor_script(&dir)?;

    let args: Vec<OsString> = vec!["rebase".into(), "-i".into(), onto.into()];
    let mut command = git_command(worktree_path, &args);
    command.env(
        "GIT_SEQUENCE_EDITOR",
        format!("cp {}", shell_single_quote(&todo_path)),
    );
    command.env("GIT_EDITOR", shell_single_quote(&editor_path));
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

/// Drives one `git rebase --continue`/`--skip` with the persisted `GIT_EDITOR` script active,
/// since a later row may still need it.
///
/// Falls back to `GIT_EDITOR=true` when that state is gone, so this can never be left waiting on
/// an interactive editor that will never open.
fn run_rebase_step(
    worktree_path: &Path,
    git_dir: &Path,
    extra_arg: &str,
) -> Result<RebaseOutcome, Error> {
    let editor_path = state_dir(git_dir).join("editor.sh");
    let args: Vec<OsString> = vec!["rebase".into(), extra_arg.into()];
    let mut command = git_command(worktree_path, &args);
    if editor_path.is_file() {
        command.env("GIT_EDITOR", shell_single_quote(&editor_path));
    } else {
        command.env("GIT_EDITOR", "true");
    }
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
    use tempfile::TempDir;
    use test_support::{git, git_output, seed_empty_repo};

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) -> String {
        fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
        git_output(dir, &["rev-parse", "HEAD"])
    }

    fn log_subjects(dir: &Path) -> Vec<String> {
        git_output(dir, &["log", "--format=%s", "--reverse"])
            .lines()
            .map(str::to_string)
            .collect()
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

    // --- start_interactive_rebase: no-stop plans --------------------------------------------

    #[test]
    fn all_pick_plan_completes_cleanly_with_history_matching_the_plan() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "b.txt", "2", "commit 2");
        let c3 = commit(repo.path(), "c.txt", "3", "commit 3");

        let plan = vec![
            RebasePlanEntry {
                commit: c1,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c3,
                action: RebaseAction::Pick,
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(
            log_subjects(repo.path()),
            vec!["base", "commit 1", "commit 2", "commit 3"]
        );
    }

    #[test]
    fn drop_removes_the_commit_from_history() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

        let plan = vec![
            RebasePlanEntry {
                commit: c1,
                action: RebaseAction::Drop,
            },
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Pick,
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(log_subjects(repo.path()), vec!["base", "commit 2"]);
        assert!(!repo.path().join("a.txt").exists());
    }

    #[test]
    fn squash_folds_into_the_previous_commit_keeping_both_original_messages() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

        let plan = vec![
            RebasePlanEntry {
                commit: c1,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Squash,
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(log_subjects(repo.path()), vec!["base", "commit 1"]);
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let message = commit_message(repo.path(), &head);
        assert!(
            message.contains("commit 1") && message.contains("commit 2"),
            "the combined message must contain both original messages unmodified, got: \
             {message:?}"
        );
        assert!(repo.path().join("a.txt").exists());
        assert!(repo.path().join("b.txt").exists());
    }

    #[test]
    fn fixup_folds_into_the_previous_commit_discarding_its_own_message() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

        let plan = vec![
            RebasePlanEntry {
                commit: c1,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Fixup,
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(log_subjects(repo.path()), vec!["base", "commit 1"]);
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        let message = commit_message(repo.path(), &head);
        assert!(message.contains("commit 1"));
        assert!(
            !message.contains("commit 2"),
            "fixup must discard the folded commit's own message, got: {message:?}"
        );
    }

    #[test]
    fn reword_with_a_supplied_message_runs_straight_through_with_no_stop() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "original message");

        let plan = vec![RebasePlanEntry {
            commit: c1,
            action: RebaseAction::Reword(Some("new message".to_string())),
        }];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(commit_message(repo.path(), &head), "new message");
    }

    // --- amend_head_message ------------------------------------------------------------------

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

    // --- Deliberate stops: reword-without-message and edit -----------------------------------

    #[test]
    fn reword_with_no_message_stops_and_reports_the_right_commit_and_reason() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");

        let plan = vec![RebasePlanEntry {
            commit: c1.clone(),
            action: RebaseAction::Reword(None),
        }];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        match outcome {
            RebaseOutcome::StoppedForEdit { commit, reason } => {
                assert_eq!(commit, c1);
                assert_eq!(reason, Some(StopReason::RewordNeedsMessage));
            }
            other => panic!("expected StoppedForEdit, got {other:?}"),
        }

        git(repo.path(), &["commit", "--amend", "-m", "amended message"]);
        let outcome = continue_rebase(repo.path()).expect("continue_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(commit_message(repo.path(), &head), "amended message");
    }

    #[test]
    fn edit_always_stops_even_with_no_special_handling_and_continue_completes_it() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");

        let plan = vec![RebasePlanEntry {
            commit: c1.clone(),
            action: RebaseAction::Edit,
        }];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        match outcome {
            RebaseOutcome::StoppedForEdit { commit, reason } => {
                assert_eq!(commit, c1);
                assert_eq!(reason, Some(StopReason::Edit));
            }
            other => panic!("expected StoppedForEdit, got {other:?}"),
        }

        let outcome = continue_rebase(repo.path()).expect("continue_rebase with no changes");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(log_subjects(repo.path()), vec!["base", "commit 1"]);
    }

    // --- Real conflicts, abort, skip -----------------------------------------------------

    /// Sets up a conflict: `base` sets `file.txt`, `v1` and `v2` change it differently, and the
    /// plan replays `v1` straight onto `base`.
    fn conflicting_repo() -> (TempDir, String, String, String) {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "file.txt", "base", "base");
        commit(repo.path(), "file.txt", "v1", "commit v1");
        let v2 = commit(repo.path(), "file.txt", "v2", "commit v2");
        (repo, base, String::new(), v2)
    }

    #[test]
    fn a_real_conflict_is_reported_with_the_right_file_and_abort_restores_original_state() {
        let (repo, base, _unused, v2) = conflicting_repo();
        let before_head = git_output(repo.path(), &["rev-parse", "HEAD"]);

        let plan = vec![RebasePlanEntry {
            commit: v2.clone(),
            action: RebaseAction::Pick,
        }];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        match outcome {
            RebaseOutcome::StoppedForConflict {
                commit,
                conflicted_files,
            } => {
                assert_eq!(commit, v2);
                assert_eq!(conflicted_files, vec![PathBuf::from("file.txt")]);
            }
            other => panic!("expected StoppedForConflict, got {other:?}"),
        }

        abort_rebase(repo.path()).expect("abort_rebase");
        let after_head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            before_head, after_head,
            "abort must restore HEAD to exactly its pre-rebase state"
        );
        assert!(!repo.path().join(".git").join("rebase-merge").exists());
    }

    #[test]
    fn resolving_a_real_conflict_for_real_and_continuing_completes_the_rebase() {
        let (repo, base, _unused, v2) = conflicting_repo();

        let plan = vec![RebasePlanEntry {
            commit: v2,
            action: RebaseAction::Pick,
        }];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

        fs::write(repo.path().join("file.txt"), "resolved").expect("write resolution");
        git(repo.path(), &["add", "file.txt"]);
        let outcome = continue_rebase(repo.path()).expect("continue_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).expect("read file.txt"),
            "resolved"
        );
    }

    #[test]
    fn skip_rebase_commit_genuinely_skips_the_stopped_commit_and_continues() {
        let (repo, base, _unused, v2) = conflicting_repo();
        let c3 = commit(repo.path(), "other.txt", "3", "commit 3");

        let plan = vec![
            RebasePlanEntry {
                commit: v2,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c3,
                action: RebaseAction::Pick,
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

        let outcome = skip_rebase_commit(repo.path()).expect("skip_rebase_commit");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(log_subjects(repo.path()), vec!["base", "commit 3"]);
    }

    // --- The mixed-action mega-scenario: catches message-editor disambiguation bugs ---------

    #[test]
    fn a_plan_mixing_every_action_type_produces_the_exact_expected_history() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "f1.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "f2.txt", "2", "commit 2");
        let c3 = commit(repo.path(), "f3.txt", "3", "commit 3");
        let c4 = commit(repo.path(), "f4.txt", "4", "commit 4");
        let c5 = commit(repo.path(), "f5.txt", "5", "commit 5");
        let c6 = commit(repo.path(), "f6.txt", "6", "commit 6");

        let plan = vec![
            RebasePlanEntry {
                commit: c1,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Squash,
            },
            RebasePlanEntry {
                commit: c3,
                action: RebaseAction::Reword(Some("reworded commit 3".to_string())),
            },
            RebasePlanEntry {
                commit: c4.clone(),
                action: RebaseAction::Reword(None),
            },
            RebasePlanEntry {
                commit: c5,
                action: RebaseAction::Drop,
            },
            RebasePlanEntry {
                commit: c6,
                action: RebaseAction::Pick,
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        match &outcome {
            RebaseOutcome::StoppedForEdit { commit, reason } => {
                assert_eq!(commit, &c4);
                assert_eq!(*reason, Some(StopReason::RewordNeedsMessage));
            }
            other => panic!("expected StoppedForEdit at commit 4, got {other:?}"),
        }

        git(
            repo.path(),
            &["commit", "--amend", "-m", "amended commit 4"],
        );
        let outcome = continue_rebase(repo.path()).expect("continue_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);

        assert_eq!(
            log_subjects(repo.path()),
            vec![
                "base",
                "commit 1",
                "reworded commit 3",
                "amended commit 4",
                "commit 6",
            ]
        );
        let all_shas = git_output(repo.path(), &["log", "--format=%H", "--reverse"]);
        let second_sha = all_shas.lines().nth(1).expect("second commit");
        let squashed_message = commit_message(repo.path(), second_sha);
        assert!(squashed_message.contains("commit 1"));
        assert!(squashed_message.contains("commit 2"));
    }

    #[test]
    fn cascading_conflicts_before_a_reword_do_not_misalign_the_message_queue() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "file.txt", "base", "base");
        let c1 = commit(repo.path(), "file.txt", "v1", "commit v1");
        let c2 = commit(repo.path(), "file.txt", "v2", "commit v2");
        let c3 = commit(repo.path(), "other.txt", "3", "commit 3");

        // Reordering v2 before v1 (both touching the same file) forces two cascading
        // conflicts before the reword step is ever reached.
        let plan = vec![
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c1,
                action: RebaseAction::Pick,
            },
            RebasePlanEntry {
                commit: c3,
                action: RebaseAction::Reword(Some("reworded commit 3".to_string())),
            },
        ];

        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));
        fs::write(repo.path().join("file.txt"), "resolved-1").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        let outcome = continue_rebase(repo.path()).expect("continue after first conflict");
        assert!(
            matches!(outcome, RebaseOutcome::StoppedForConflict { .. }),
            "expected a second real conflict, got {outcome:?}"
        );

        fs::write(repo.path().join("file.txt"), "resolved-2").expect("write");
        git(repo.path(), &["add", "file.txt"]);
        let outcome = continue_rebase(repo.path()).expect("continue after second conflict");
        assert_eq!(
            outcome,
            RebaseOutcome::Completed,
            "the reword step must run straight through with its own real message, not stop"
        );

        assert_eq!(
            log_subjects(repo.path()),
            vec!["base", "commit v2", "commit v1", "reworded commit 3"]
        );
    }

    // --- rebase_status -------------------------------------------------------------------

    #[test]
    fn rebase_status_is_none_when_no_rebase_is_in_progress() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "commit 1");
        assert_eq!(rebase_status(repo.path()).expect("rebase_status"), None);
    }

    #[test]
    fn rebase_status_reports_real_state_when_stopped_mid_flight() {
        let repo = seed_empty_repo();
        let base = commit(repo.path(), "base.txt", "base", "base");
        let c1 = commit(repo.path(), "a.txt", "1", "commit 1");
        let c2 = commit(repo.path(), "b.txt", "2", "commit 2");

        let plan = vec![
            RebasePlanEntry {
                commit: c1.clone(),
                action: RebaseAction::Edit,
            },
            RebasePlanEntry {
                commit: c2,
                action: RebaseAction::Pick,
            },
        ];
        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert!(matches!(outcome, RebaseOutcome::StoppedForEdit { .. }));

        let status = rebase_status(repo.path())
            .expect("rebase_status")
            .expect("a rebase is really in progress");
        assert_eq!(status.stopped_commit.as_deref(), Some(c1.as_str()));
        assert_eq!(status.stop_reason, Some(StopReason::Edit));
        assert!(status.conflicted_files.is_empty());
        assert_eq!(status.current_step, Some(1));
        assert_eq!(status.total_steps, Some(2));
        assert!(status.onto.is_some());

        // Recovery after a real "process restart" (nothing here relies on in-memory state):
        // continuing and finishing still works using only what's on disk.
        let outcome = continue_rebase(repo.path()).expect("continue_rebase");
        assert_eq!(outcome, RebaseOutcome::Completed);
        assert_eq!(rebase_status(repo.path()).expect("rebase_status"), None);
    }

    #[test]
    fn rebase_status_reports_conflicted_files_and_no_stop_reason_for_a_real_conflict() {
        let (repo, base, _unused, v2) = conflicting_repo();
        let plan = vec![RebasePlanEntry {
            commit: v2,
            action: RebaseAction::Pick,
        }];
        let outcome =
            start_interactive_rebase(repo.path(), &base, &plan).expect("start_interactive_rebase");
        assert!(matches!(outcome, RebaseOutcome::StoppedForConflict { .. }));

        let status = rebase_status(repo.path())
            .expect("rebase_status")
            .expect("a rebase is really in progress");
        assert_eq!(status.conflicted_files, vec![PathBuf::from("file.txt")]);
        assert_eq!(
            status.stop_reason, None,
            "a real conflict must never report a deliberate-stop reason"
        );
    }
}
