//! Real, headless `git rebase --interactive` - GitHub issue #242 phase A: the engine only, no
//! UI. Every mutation shells out to a real `git rebase` subprocess and drives it non-interactively
//! via `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR`, exactly the mechanism a human's `$EDITOR` would be
//! invoked through, just pointed at scripts this module writes instead. All of the mechanics
//! below were verified empirically against a real `git` (2.43.0) before being committed to; see
//! each function's docs for what was tested and why the alternative it replaced didn't work.
//!
//! ## The six real rebase actions, and which ones stop
//!
//! [`RebaseAction`] models git's own `pick`/`reword`/`edit`/`squash`/`fixup`/`drop` todo verbs,
//! plus one addition on top of plain git: [`RebaseAction::Reword`] carries an
//! `Option<String>` - a pre-supplied replacement message. Plain `git rebase -i` always stops to
//! invoke `$EDITOR` for every `reword` step; this module's headless `GIT_EDITOR` script supplies
//! the message itself when one was given, so most rewords never have to stop at all. `pick`,
//! `squash`, `fixup`, and `drop` never stop for a message on their own - a `squash` step's
//! message-combination editor invocation always accepts git's own default (both original
//! messages, unmodified), and `fixup` never invokes the message editor at all (git's own
//! behavior, verified below). `edit` always stops (git's native behavior, no editor involved).
//! `reword` with no supplied message also stops - empirically, this behaves *identically* to
//! `edit` at every level of git's own on-disk state (same `stopped-sha`, same `REBASE_HEAD`, same
//! "nothing to commit, working tree clean" with `HEAD` left at the picked-but-not-yet-amended
//! commit) rather than needing a special "waiting for a message" mechanism: git's own message
//! editor for that step is made to fail (see [`write_editor_script`]'s docs), and git's real
//! response to an editor failure during `reword` is to leave the commit exactly where `edit`
//! would. [`RebasePlanEntry`]'s own plan (built by the caller, in oldest-first order - the same
//! order git's own generated todo uses) is persisted alongside git's own rebase state (see
//! below) so a later stop can be cross-referenced back to "was this row `edit`, or a
//! message-less `reword`" for [`RebaseOutcome::StoppedForEdit`]'s `reason` field, even across a
//! process restart - git itself has no way to tell the two apart once stopped.
//!
//! ## Driving the todo list: `GIT_SEQUENCE_EDITOR`
//!
//! `git rebase -i <onto>` generates its own default todo (one `pick <sha> <subject>` line per
//! commit, oldest first - confirmed empirically), writes it to a temp file, then invokes
//! `$GIT_SEQUENCE_EDITOR <path-to-that-file>` once to let the invoked program rewrite it.
//! [`write_plan_state`] writes this module's own plan to a file and sets
//! `GIT_SEQUENCE_EDITOR="cp <path>"`; git splices that value verbatim into `sh -c "<value>
//! \"$@\""` (confirmed empirically: a `cat`-as-sequence-editor genuinely dumped the *real*
//! generated todo to stdout, proving both the default todo's shape and this invocation form), so
//! a plain `cp` really does copy this module's plan over git's own generated file - no bespoke
//! script needed here, unlike `GIT_EDITOR` below. `pick <sha>`/`drop <sha>` with no subject text
//! at all was also confirmed to work (git only inspects the verb and the object id; the rest of
//! the line is cosmetic), so this module's todo lines never bother including one.
//!
//! Because the value is spliced unquoted into a shell command line, a path containing a space (a
//! real, already-tested case elsewhere in this crate - see
//! [`crate::parse_worktree_list_porcelain`]'s "My Projects" test) would otherwise be silently
//! word-split and break the injected command; [`shell_single_quote`] POSIX-single-quotes every
//! path embedded into these env var values, and this was verified end-to-end against a real
//! rebase run from a worktree path containing a space with a plan file *also* under a
//! space-containing path.
//!
//! ## Driving messages: `GIT_EDITOR`
//!
//! `GIT_EDITOR` is invoked for every step that needs a real commit-message edit: `reword`'s own
//! message, `squash`'s message-combination step, and - this was **not** anticipated up front,
//! only found by testing a real conflict mid-rebase - a plain `pick` (or any other step) that hit
//! a real conflict and is being finished via `git rebase --continue`. That last case goes through
//! git's ordinary `git commit` codepath (unlike a clean `pick`, which never invokes an editor at
//! all), which opens the message editor pre-filled with the original message. If this module's
//! script mishandled that case, it would misinterpret an ordinary conflict-resume as a `reword`
//! step and consume a queued message meant for a *later* real `reword` - a real, empirically
//! reproduced bug during development of this module (see the "cascading conflicts before a
//! reword" test below, which specifically guards against a queue-index regression here).
//!
//! [`write_editor_script`] resolves each invocation into exactly one of three cases by reading
//! the message file git hands it, matched by content, not by invocation order (per this module's
//! own design goal of never guessing blindly):
//! 1. The file's first line is `# This is a combination of ...` (git's own fixed boilerplate,
//!    confirmed to appear on every squash message-combination invocation and no other) - accept
//!    the file completely unmodified and exit `0`.
//! 2. The file contains the line `You are currently editing a commit` (confirmed to appear on
//!    every real `reword` invocation, and *no* other case - a squash-combination invocation says
//!    "rebasing branch", not "editing a commit") - pop the next slot from this rebase's own
//!    ordered reword-message queue (see below). If a message was queued there, overwrite the file
//!    with it and exit `0`. If nothing was queued (a message-less `reword`), exit non-zero,
//!    which reproduces `edit`'s own stop exactly (confirmed empirically).
//! 3. Anything else (a conflict-resumed `pick`/`fixup`/`drop`/`squash`, or any other case this
//!    module doesn't have a specific reason to touch) - accept the file exactly as git pre-filled
//!    it and exit `0`, *without* touching the reword queue's cursor at all. Confirmed empirically
//!    that this branch really is reached for a conflict-resumed `pick`, and that leaving the
//!    cursor untouched there is what keeps a later real `reword` in the same rebase pointed at
//!    the right queue slot.
//!
//! ## The reword-message queue
//!
//! Because `GIT_EDITOR` may be invoked again on a later `git rebase --continue`/`--skip` call -
//! a separate process invocation, possibly after this application itself restarted - the queue
//! can't just live in this process's memory. [`write_plan_state`] persists it as one plain file
//! per queued message (`queue/<n>` for the `n`-th `reword` row in the plan, 0-indexed among
//! `reword` rows only; a message-less `reword` simply has no file at its slot) plus a `cursor`
//! file the script itself advances, all inside this rebase's own state directory (see below) -
//! deliberately plain files, not JSON, so the `GIT_EDITOR` script (itself just `/bin/sh`) never
//! needs a JSON parser.
//!
//! ## Where this module's own state lives
//!
//! Everything this module writes (the plan's todo file, the `GIT_EDITOR` script, the reword
//! queue, and a `commits.txt` cross-reference of each plan row's own commit id and action) lives
//! under `<git-dir>/ade-rebase/`, where `<git-dir>` is *this worktree's own* private
//! administrative directory (`git rev-parse --git-dir`, confirmed to resolve to the
//! worktree-specific `<common-dir>/worktrees/<name>` for a linked worktree, not the shared common
//! dir - each worktree has its own independent rebase state, so this module's own sidecar state
//! must be equally worktree-specific). This directory persists for exactly as long as the rebase
//! itself is stopped (removed on [`RebaseOutcome::Completed`] and on [`abort_rebase`]), so
//! [`rebase_status`] can recover it - including [`RebaseOutcome::StoppedForEdit`]'s `reason` -
//! after a real process restart mid-rebase, without the caller needing to hold the original plan
//! in memory.
//!
//! ## Detecting conflicts vs. deliberate stops
//!
//! After driving `git rebase` (via [`start_interactive_rebase`], [`continue_rebase`], or
//! [`skip_rebase_commit`]), a non-zero exit means either a real conflict or a deliberate stop
//! (`edit`, or the `GIT_EDITOR` script's own non-zero exit for a message-less `reword`).
//! `git diff --name-only --diff-filter=U` (pinned to `core.quotePath=false`, exactly
//! [`crate::merge`]'s own convention) distinguishes the two: non-empty means a real conflict
//! ([`RebaseOutcome::StoppedForConflict`]); empty, with `.git/rebase-merge/stopped-sha` present,
//! means a deliberate stop ([`RebaseOutcome::StoppedForEdit`]) - both were confirmed empirically
//! to populate `stopped-sha` identically, so it's read unconditionally and only the conflict
//! check decides which outcome to report. Neither present is a genuine, unexpected failure,
//! surfaced as [`Error::GitCommand`] with git's own real stderr, matching every other module in
//! this crate.
//!
//! ## Not resolving conflicts itself
//!
//! Matches [`crate::rewrite`]'s own documented convention: a real conflict leaves the worktree in
//! exactly the state the equivalent command-line `git rebase -i` would (conflict markers,
//! `REBASE_HEAD`, `.git/rebase-merge/`), for the caller to resolve through this app's own
//! conflict-resolution surface or a real terminal - never silently rolled back, never
//! auto-resolved.
//!
//! ## Platform
//!
//! This module's `GIT_EDITOR` script is a POSIX `/bin/sh` script; it is only ever exercised on
//! Unix-like platforms in this crate's own tests, matching [`crate::error::GitExit`]'s own
//! existing Unix-only signal handling. On a platform with no `/bin/sh` this would surface as a
//! real `git`/spawn failure rather than silently misbehaving.
//!
//! Performs blocking I/O everywhere in this module (shells out to `git`, writes real files under
//! `.git/`); see the crate-level docs on offloading this to a background thread.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, GitExit};
use crate::{absolutize, check_success, format_args, git_command, run_git};

/// One of git's own six interactive-rebase todo verbs, plus Jerry's own pre-supplied-message
/// addition to `reword` - see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    /// `git rebase -i`'s `reword`. `Some(message)` supplies the replacement message up front,
    /// so this row runs straight through with no stop; `None` behaves exactly like
    /// [`RebaseAction::Edit`] - a real, deliberate stop (see the module docs for why this is
    /// implemented as "let git's own message editor fail" rather than a bespoke waiting state).
    Reword(Option<String>),
    /// Always stops after applying the commit, before the next step - git's own native
    /// behavior, no editor involved.
    Edit,
    /// Folds this commit into the previous row's commit. The combined message is always git's
    /// own default (both original messages, unmodified); this module never stops to let a
    /// caller edit it.
    Squash,
    /// Folds this commit into the previous row's commit, discarding this commit's own message
    /// entirely (keeps only the previous row's). Never invokes a message editor at all - git's
    /// own behavior, confirmed empirically.
    Fixup,
    /// Removes this commit from history entirely.
    Drop,
}

impl RebaseAction {
    /// The literal verb git's own interactive-rebase todo format expects.
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

    /// The tag persisted in `commits.txt` for this row - see [`lookup_stop_reason`]. Distinct
    /// from [`Self::todo_verb`] only for `Reword`, where the message's presence matters for
    /// cross-referencing a later stop.
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

/// One row of an interactive rebase plan: a real commit id, and what to do with it. A full plan
/// (`&[RebasePlanEntry]`) is given oldest-first, the same order git's own generated todo uses -
/// see [`start_interactive_rebase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePlanEntry {
    /// A real object id (full, not abbreviated - this module always resolves/accepts full ids,
    /// confirmed to work directly in git's own todo format).
    pub commit: String,
    pub action: RebaseAction,
}

/// Why a real stop reported as [`RebaseOutcome::StoppedForEdit`] happened, cross-referenced
/// against this rebase's own persisted plan (see the module docs) purely for a future UI's
/// pause-column semantics - git itself treats both cases identically on disk. `None` when the
/// stopped commit's own row couldn't be found in the persisted plan (e.g. this rebase's state
/// directory was already cleaned up, or the in-progress rebase wasn't started by this module at
/// all) - deliberately not fabricated to `Some(StopReason::Edit)` as a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The stopped commit's own plan row was [`RebaseAction::Edit`].
    Edit,
    /// The stopped commit's own plan row was [`RebaseAction::Reword`] with no supplied message.
    RewordNeedsMessage,
}

/// The real result of driving one `git rebase` invocation
/// ([`start_interactive_rebase`]/[`continue_rebase`]/[`skip_rebase_commit`]).
///
/// There is deliberately no `Aborted` variant here: [`abort_rebase`] is its own, separately
/// fallible operation (`Result<(), Error>`) rather than one more outcome of driving a step - it
/// doesn't "drive" anything forward the way the other three do, so folding it into this enum
/// would suggest a continuity ("aborted, but still mid-plan-at-row-N") that doesn't really exist
/// once `git rebase --abort` has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The whole plan applied with no stops remaining. This module's own state directory has
    /// already been cleaned up by the time this is returned.
    Completed,
    /// A deliberate, non-conflict stop: either an [`RebaseAction::Edit`] row, or a
    /// message-less [`RebaseAction::Reword`] row. `commit` is the real object id (as it
    /// appeared in the original plan) of the stopped row. Resolve with [`continue_rebase`]
    /// (after e.g. `git commit --amend`) or [`skip_rebase_commit`], exactly as the
    /// command-line workflow would.
    StoppedForEdit {
        commit: String,
        reason: Option<StopReason>,
    },
    /// A real conflict: the worktree is left with real conflict markers and `REBASE_HEAD`, for
    /// the caller to resolve through this app's own conflict-resolution surface or a real
    /// terminal - see the module docs. `commit` is the real object id of the commit that failed
    /// to apply.
    StoppedForConflict {
        commit: String,
        conflicted_files: Vec<PathBuf>,
    },
}

/// The real, on-disk state of an interactive rebase that's currently stopped mid-flight, as
/// read directly from git's own `.git/rebase-merge/` (plus this module's own persisted plan
/// cross-reference, when available) - not a cache of anything this process remembers, so this
/// reflects reality even after a real process restart. Every field besides `conflicted_files` is
/// `Option`/best-effort rather than fabricated: a field git's own state files don't currently
/// expose (or that this module's own state directory can't corroborate) is `None`, never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStatus {
    /// The commit the rebase is replaying onto (`.git/rebase-merge/onto`), if readable.
    pub onto: Option<String>,
    /// 1-indexed position of the current/next step (`.git/rebase-merge/msgnum`), if readable.
    pub current_step: Option<usize>,
    /// Total number of steps in the plan (`.git/rebase-merge/end`), if readable.
    pub total_steps: Option<usize>,
    /// The real object id of the commit currently stopped at (`.git/rebase-merge/stopped-sha`),
    /// if any - present for both a deliberate stop and a real conflict.
    pub stopped_commit: Option<String>,
    /// Files with real, unresolved `<<<<<<</=======/>>>>>>>` conflict markers, if any. Non-empty
    /// here is what distinguishes a real conflict from a deliberate stop.
    pub conflicted_files: Vec<PathBuf>,
    /// Why the stop is deliberate (not a conflict) - see [`StopReason`]. Always `None` when
    /// `conflicted_files` is non-empty.
    pub stop_reason: Option<StopReason>,
}

/// This module's own sidecar state directory name, nested under a worktree's private
/// administrative directory (`git rev-parse --git-dir`) - see the module docs for why here
/// specifically, not under `.git/rebase-merge/` itself (which git may not have created yet at
/// the moment this module wants to start writing) and not under the shared common dir (rebase
/// state, and so this module's own cross-reference of it, is per-worktree).
const STATE_DIR_NAME: &str = "ade-rebase";

fn state_dir(git_dir: &Path) -> PathBuf {
    git_dir.join(STATE_DIR_NAME)
}

/// Resolve `worktree_path`'s own private administrative directory (`git rev-parse --git-dir`) -
/// for a linked worktree this is `<common-dir>/worktrees/<name>`, confirmed empirically to be
/// distinct from [`crate::git_common_dir`]'s shared common dir, which is why this module doesn't
/// just reuse that function.
///
/// Performs blocking I/O.
fn worktree_git_dir(worktree_path: &Path) -> Result<PathBuf, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "--git-dir".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(absolutize(Path::new(raw.trim()), worktree_path))
}

/// POSIX-single-quote `path` for safe embedding inside a `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR`
/// value string.
///
/// git splices that value verbatim into `sh -c "<value> \"$@\""` rather than treating it as an
/// already-quoted argument (confirmed empirically - see the module docs), so a path containing a
/// space or other shell metacharacter would otherwise be silently word-split and break the
/// injected command; single-quoting the embedded path (escaping any literal `'` as `'\''`, the
/// standard POSIX technique) was verified end-to-end against a real rebase run from a
/// space-containing worktree path with a space-containing plan-file path too.
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

/// Template for [`write_editor_script`]'s real `GIT_EDITOR` script - see the module docs for
/// what each of the three cases handles and why, and for how this was verified.
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

/// Write this rebase's real `GIT_EDITOR` script into `dir` (this rebase's own state directory)
/// and return its path.
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

/// Write `plan`'s real todo file (for `GIT_SEQUENCE_EDITOR`), reword-message queue (for the
/// `GIT_EDITOR` script), and `commits.txt` cross-reference into `dir`, creating it fresh.
/// Returns the todo file's path.
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

/// Remove this rebase's own state directory (idempotent - fine if it's already gone). Called
/// once a rebase genuinely finishes ([`RebaseOutcome::Completed`]) or is aborted.
fn cleanup_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(state_dir(git_dir));
}

/// Read `.git/rebase-merge/stopped-sha`, trimmed, or `None` if absent/unreadable/empty.
/// Confirmed empirically to be populated identically for both a real conflict and a deliberate
/// stop, so callers read it unconditionally and only branch on [`conflicted_files`] to decide
/// which [`RebaseOutcome`] variant applies.
fn read_stopped_sha(git_dir: &Path) -> Option<String> {
    read_trimmed(&git_dir.join("rebase-merge").join("stopped-sha"))
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Cross-reference `commit` against this rebase's own persisted `commits.txt` (see
/// [`write_plan_state`]) to recover which [`StopReason`] a stop at `commit` represents. `None`
/// if the state file is missing (state already cleaned up, or this rebase wasn't started by
/// this module) or `commit` isn't in it.
fn lookup_stop_reason(git_dir: &Path, commit: &str) -> Option<StopReason> {
    let content = fs::read_to_string(state_dir(git_dir).join("commits.txt")).ok()?;
    for line in content.lines() {
        let (sha, tag) = line.split_once(' ')?;
        if sha == commit {
            return match tag {
                "edit" => Some(StopReason::Edit),
                "reword-nomsg" => Some(StopReason::RewordNeedsMessage),
                // "reword-msg" never actually stops (a supplied message always runs straight
                // through) - if this is somehow reached anyway, there's no real reason to
                // report, so this deliberately falls through to `None` rather than guessing.
                _ => None,
            };
        }
    }
    None
}

/// Real `git diff --name-only --diff-filter=U` in `worktree_path`, pinned to
/// `core.quotePath=false` - exactly [`crate::merge`]'s own convention (see that module's docs
/// for why: an unresolved conflicted path containing non-ASCII/space characters would otherwise
/// come back quoted/octal-escaped, and this module would then either silently mismatch it
/// against the real on-disk file or misreport it to a caller).
///
/// Performs blocking I/O.
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

/// Interpret the result of driving one `git rebase`/`--continue`/`--skip` invocation into a real
/// [`RebaseOutcome`] - shared by [`start_interactive_rebase`], [`continue_rebase`], and
/// [`skip_rebase_commit`]. See the module docs for the real conflict-vs-deliberate-stop
/// detection this implements.
///
/// Deliberately does **not** branch on `output.status` first: confirmed empirically that a
/// native `edit` stop (and, distinctly, a message-less `reword`'s deliberate stop, since
/// `edit`/`reword` are meant to behave identically - see the module docs) has different exit
/// codes depending on *how* the stop was reached - a sole/final `edit` row exits `0`, while a
/// `reword` stopped via the `GIT_EDITOR` script's own non-zero exit exits `1` - so `output.status
/// == success` is not a reliable "did this really finish" signal on its own. The real, reliable
/// signal is whether `.git/rebase-merge/` still exists at all: git only ever removes it on a
/// genuine completion (or abort, handled separately by [`abort_rebase`]).
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
        // No rebase state left at all, yet the command failed: a genuine, unexpected error
        // (bad `onto`, not a repository, etc.), not any kind of stop.
        return Err(Error::GitCommand {
            args: format_args(args),
            exit: GitExit::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    // Still genuinely stopped mid-rebase, regardless of exit code - see this function's docs.
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

    // Rebase state exists but neither a known conflict nor a known deliberate stop could be
    // read from it: a genuine, unexpected failure rather than a fabricated outcome.
    Err(Error::GitCommand {
        args: format_args(args),
        exit: GitExit::from_status(&output.status),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Every real commit id `git rebase -i <onto>` would default to `pick`-ing, oldest first (`git
/// rev-list --reverse <onto>..HEAD`) - exactly the commits reachable from `HEAD` but not from
/// `onto`. GitHub issue #242 phase B: the graph pane's "Rebase onto this commit" row needs
/// this to build its initial plan - [`start_interactive_rebase`] never reads git's own
/// autogenerated todo at all (it overwrites it outright with the caller's own plan - see the
/// module docs), so computing "what would the default plan even contain" is the caller's job,
/// not something this module derives internally.
///
/// `onto` must be a real commit-ish git accepts (a full/short object id, or a ref name).
///
/// Performs blocking I/O.
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

/// Start a real interactive rebase of `worktree_path`'s current branch onto `onto`, driving
/// `plan` through to completion or the first real stop - see the module docs for exactly how.
///
/// `plan` is given oldest-first (the same order git's own generated todo uses); this function
/// does not reverse it.
///
/// Performs blocking I/O.
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

/// Real `git commit --amend -m <message>` against `worktree_path`'s current `HEAD` - GitHub
/// issue #242 phase B's own real need: when a rebase is genuinely stopped on a message-less
/// `reword` row (a [`RebaseOutcome::StoppedForEdit`] with
/// [`StopReason::RewordNeedsMessage`]) and the caller has since obtained a real message from the
/// user, this is how it actually gets applied to the stopped commit before
/// [`continue_rebase`] resumes - exactly the same real `git commit --amend` step this module's
/// own tests use to drive that scenario (see `reword_with_no_message_stops_and_reports_the_right_commit_and_reason`).
/// This module's `GIT_EDITOR` script has already fixed the reword-message queue for this rebase
/// at [`start_interactive_rebase`] time (see the module docs); a message obtained *after* a
/// message-less stop can only be applied this way, not by retroactively feeding the queue.
///
/// Two real guards, both refusals rather than silent corruption:
/// - `expected_head_original` must be the real, full commit id `HEAD` is still expected to be
///   pointing at (the stopped row's own commit) - a live `git rev-parse HEAD` mismatch means the
///   rebase already moved on (a double-`Continue`, a resumed-then-re-amended stale caller) and
///   this refuses with [`Error::RebaseAmendHeadMoved`] rather than amending whatever commit
///   `HEAD` now happens to be.
/// - The real index must have no staged changes (`git diff --cached --quiet`) - refuses with
///   [`Error::RebaseAmendIndexDirty`] rather than silently folding unrelated staged content
///   (something else - an unpaused agent, a stray `git add` - added while the rebase was
///   stopped) into the amended commit alongside the real message change.
///
/// Performs blocking I/O.
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
        // `git diff --cached --quiet` exits 0 when there is no staged diff, 1 when there is -
        // confirmed exit-code convention for `--quiet`, mirroring `crate::merge`'s own use of
        // `git diff --name-only --diff-filter=U` conventions elsewhere in this crate for reading
        // real git state through exit codes rather than parsing output.
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

/// Drive one `git rebase <extra_arg>` (`--continue` or `--skip`) with this rebase's own
/// persisted `GIT_EDITOR` script active (needed in case a later plan row still requires it -
/// `reword`/squash message combination can recur after a resumed step), falling back to
/// `GIT_EDITOR=true` (accept whatever's pre-filled, never block) if this rebase's own state was
/// already cleaned up or was never created by this module in the first place - either way, this
/// call must never be left waiting on a real interactive editor that can never open.
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

/// Resume a stopped interactive rebase in `worktree_path` (real `git rebase --continue`),
/// driving it through to completion or the next real stop.
///
/// Performs blocking I/O.
pub fn continue_rebase(worktree_path: &Path) -> Result<RebaseOutcome, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    run_rebase_step(worktree_path, &git_dir, "--continue")
}

/// Skip the commit a stopped interactive rebase in `worktree_path` is currently stopped at (real
/// `git rebase --skip`), driving it through to completion or the next real stop.
///
/// Performs blocking I/O.
pub fn skip_rebase_commit(worktree_path: &Path) -> Result<RebaseOutcome, Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    run_rebase_step(worktree_path, &git_dir, "--skip")
}

/// Abort an in-progress interactive rebase in `worktree_path` (real `git rebase --abort`),
/// restoring it to exactly the state it was in before [`start_interactive_rebase`] ran, and
/// cleaning up this module's own state directory.
///
/// Performs blocking I/O.
pub fn abort_rebase(worktree_path: &Path) -> Result<(), Error> {
    let git_dir = worktree_git_dir(worktree_path)?;
    let args: Vec<OsString> = vec!["rebase".into(), "--abort".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    cleanup_state(&git_dir);
    Ok(())
}

/// Read the real, on-disk state of an interactive rebase in `worktree_path`, if one is currently
/// stopped mid-flight - `Ok(None)` if no rebase is in progress at all. Reads git's own
/// `.git/rebase-merge/` directly (not any in-memory record), so this reflects reality even after
/// a real process restart - see [`RebaseStatus`]'s own docs for why each field is best-effort.
///
/// Performs blocking I/O.
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

        // Refused, not partially applied - the real message must be untouched.
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(commit_message(repo.path(), &head), "original message");
    }

    #[test]
    fn amend_head_message_refuses_when_the_real_index_has_staged_changes() {
        let repo = seed_empty_repo();
        commit(repo.path(), "a.txt", "1", "original message");
        let head = git_output(repo.path(), &["rev-parse", "HEAD"]);

        // A real staged change unrelated to the amend - the exact hazard the guard exists to
        // catch (an unpaused agent, or a stray `git add`, staging something while the rebase is
        // stopped).
        fs::write(repo.path().join("sneaky.txt"), "sneaky").expect("write sneaky.txt");
        git(repo.path(), &["add", "sneaky.txt"]);

        let err = amend_head_message(repo.path(), &head, "a real new message")
            .expect_err("real staged changes must refuse the amend");
        assert!(
            matches!(err, Error::RebaseAmendIndexDirty { .. }),
            "expected RebaseAmendIndexDirty, got a different error: {err:?}"
        );

        // Refused, not partially applied - neither the message nor the tree changed.
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

        // Amend for real, then continue - completes with the amended message applied.
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

    /// Sets up a real conflict: `base` sets `file.txt`, `v1` and `v2` each change it
    /// differently, and the plan replays `v1` directly onto `base` (skipping `v2`'s own base),
    /// which really conflicts.
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

    /// Reproduces a real bug found during development: a plain `pick` that hits a real conflict
    /// invokes `GIT_EDITOR` too (git's ordinary `git commit` codepath for a conflict-resumed
    /// step, confirmed empirically - see the module docs). If the `GIT_EDITOR` script
    /// misclassified that as a `reword` invocation, it would consume a queue slot meant for a
    /// *later* real `reword`, applying the wrong message (or stopping when it shouldn't).
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
