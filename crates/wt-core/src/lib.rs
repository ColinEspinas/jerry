//! `wt-core`: git worktree management.
//!
//! This crate has three responsibilities:
//!
//! 1. Enumerate the worktrees of a git repository into a typed model, reading the
//!    repository via [`gix`] (never shelling out for reads, per project convention).
//! 2. Create new worktrees by shelling out to the real `git` CLI.
//! 3. Remove worktrees by shelling out to the real `git` CLI, refusing to discard
//!    uncommitted work unless the caller explicitly opts in with `force`.
//!
//! Every git invocation (both `gix` calls and `git` CLI calls) uses explicit argument
//! vectors; nothing is ever built as an interpolated shell string.
//!
//! ## Blocking
//!
//! Every public function in this crate performs blocking I/O: `gix` reads the object
//! database from disk, and the CLI-backed functions spawn a `git` child process and wait
//! on it. None of this is async. Callers embedding this in a GUI event loop (this crate
//! exists to back one) must offload calls to a background thread/executor rather than
//! invoking them from a UI main thread.

// `.expect()`/`.unwrap()` are the documented, accepted pattern in test modules (see
// `CLAUDE.md`'s Rust standards) - only production code is held to `clippy::unwrap_used`/
// `expect_used`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod blame;
pub mod checkout;
pub mod diff;
mod error;
pub mod graph;
pub mod merge;
pub mod rebase;
pub mod remote;
pub mod review;
pub mod rewrite;
pub mod run_drift;
pub mod stage;
pub mod undo;
pub mod worktree_files;

pub use error::{Error, GitExit};

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A single git worktree: either the main working tree of a repository, or one of the
/// linked worktrees created by `git worktree add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Absolute path to the worktree's working directory.
    pub path: PathBuf,
    /// The short name of the branch checked out in this worktree (e.g. `"main"`), or
    /// `None` if `HEAD` is detached.
    pub branch: Option<String>,
    /// The full commit id `HEAD` currently resolves to, or `None` if the branch is
    /// "unborn" (a freshly initialized repository with no commits yet).
    pub head_commit: Option<String>,
    /// Whether this is the main worktree (the one created by `git init` or `git clone`),
    /// as opposed to a linked worktree created by `git worktree add`.
    pub is_main: bool,
    /// Whether the worktree is locked (see `git worktree lock`), which signals that it
    /// should not be pruned, moved, or deleted, typically because it lives on removable
    /// storage. Always `false` for the main worktree: git has no `worktree lock` concept
    /// for it (only linked worktrees can be locked), so this isn't something we forgot to
    /// check.
    pub is_locked: bool,
    /// The reason given when the worktree was locked, if any and if it could be read.
    /// `None` both when the worktree isn't locked and when it's locked with an empty
    /// reason (git allows `git worktree lock` with no `--reason`). Always `None` for the
    /// main worktree, for the same reason `is_locked` is always `false` for it.
    pub lock_reason: Option<String>,
}

/// The outcome of describing a single worktree: `Ok` on success, or the specific `Error`
/// that made this one worktree's metadata unreadable (e.g. a corrupt `gitdir` file, or a
/// checkout whose `HEAD` couldn't be resolved). A problem with one worktree should not
/// hide the others, so [`list_worktrees`] returns one of these per worktree instead of
/// aborting the whole listing on the first error.
pub type WorktreeResult = Result<Worktree, Error>;

/// List every worktree (the main worktree, if any, plus any linked worktrees) belonging
/// to the repository at `repo_path`.
///
/// `repo_path` may point at the main worktree or at any linked worktree; either way, all
/// worktrees sharing the same repository are returned.
///
/// If the repository is bare, it has no main worktree to report (a bare repository has no
/// checkout of its own; it's only ever a `git worktree add` target), so in that case only
/// linked worktrees, if any, are returned.
///
/// Each entry is independent: a problem reading one worktree is reported as an `Err`
/// inside that entry rather than failing the whole call. The outer `Result` is only for
/// failures that make listing impossible altogether, such as `repo_path` not being a git
/// repository.
///
/// Performs blocking I/O; see the crate-level docs.
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeResult>, Error> {
    let repo = open_repo(repo_path)?;
    // Normalize to the repository owning the main worktree so that `worktrees()` below
    // enumerates every linked worktree regardless of which worktree `repo_path` pointed at.
    let main_repo = repo.main_repo().map_err(|source| Error::Open {
        path: repo_path.to_path_buf(),
        source: Box::new(source),
    })?;

    let mut worktrees = Vec::new();

    if let Some(main_path) = main_repo.work_dir() {
        let main_path = main_path.to_path_buf();
        worktrees.push(describe_worktree(&main_repo, main_path, true, false, None));
    }

    let proxies = main_repo.worktrees().map_err(Error::WorktreeIo)?;
    for proxy in proxies {
        worktrees.push(describe_linked_worktree(proxy));
    }

    Ok(worktrees)
}

fn open_repo(repo_path: &Path) -> Result<gix::Repository, Error> {
    gix::open(repo_path).map_err(|source| Error::Open {
        path: repo_path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Read a linked worktree's location and lock state from its [`gix::worktree::Proxy`],
/// then open it and read its `HEAD` to fill in the rest of a [`Worktree`].
fn describe_linked_worktree(proxy: gix::worktree::Proxy<'_>) -> Result<Worktree, Error> {
    let path = proxy.base()?;
    let is_locked = proxy.is_locked();
    // An empty `locked` file (i.e. `git worktree lock` with no `--reason`) must surface as
    // `None`, not `Some("")`.
    let lock_reason = proxy
        .lock_reason()
        .map(|reason| reason.to_string())
        .filter(|reason| !reason.is_empty());
    let linked_repo = proxy
        .into_repo_with_possibly_inaccessible_worktree()
        .map_err(|source| Error::Open {
            path: path.clone(),
            source: Box::new(source),
        })?;
    describe_worktree(&linked_repo, path, false, is_locked, lock_reason)
}

/// Read the branch name and resolved `HEAD` commit id of `repo`, and assemble a
/// [`Worktree`] from it plus the already-known metadata.
fn describe_worktree(
    repo: &gix::Repository,
    path: PathBuf,
    is_main: bool,
    is_locked: bool,
    lock_reason: Option<String>,
) -> Result<Worktree, Error> {
    let mut head = repo
        .head()
        .map_err(|source| Error::Head(Box::new(source)))?;
    let branch = head.referent_name().map(|name| name.shorten().to_string());
    let head_commit = head
        .try_peel_to_id_in_place()
        .map_err(|source| Error::PeelHead(Box::new(source)))?
        .map(|id| id.to_string());

    Ok(Worktree {
        path,
        branch,
        head_commit,
        is_main,
        is_locked,
        lock_reason,
    })
}

/// A single worktree as reported by `git worktree list --porcelain`, used by the sidebar's
/// live-refresh watcher (`app::rail::worktree_watch`, GitHub issue #12) rather than
/// [`Worktree`]/[`list_worktrees`].
///
/// This is a deliberate, narrow exception to this crate's usual "never shell out for reads"
/// convention (see the crate-level docs): `git` itself already computes locked/prunable state
/// (including the "administrative gitdir points at a now-missing working tree" case a plain
/// [`Path::exists`] check on our side can't fully replicate - e.g. a `gitdir` file that's
/// itself corrupt or missing) and hands it back in one cheap call, which is exactly what a
/// refresh loop that runs every few seconds wants: a single source of truth to diff against the
/// previous snapshot, not a second bespoke traversal reimplementing git's own prunability rules.
/// [`list_worktrees`] remains the one true source for every other consumer in this crate
/// (`blame`/`merge`/`undo`), which need real `gix` repository access anyway and have no reason
/// to shell out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStatus {
    /// Absolute path to the worktree's working directory (or where it used to be, if
    /// [`Self::is_prunable`]).
    pub path: PathBuf,
    /// Whether this is the main worktree - true for the first entry `git worktree list`
    /// reports, unless that entry is itself [`Self::is_bare`] (a bare repository has no main
    /// *worktree* to report, mirroring [`list_worktrees`]'s own doc for the gix-backed path).
    pub is_main: bool,
    /// Whether this entry is the bare repository itself (only ever the first entry, and only
    /// for a bare repository) rather than a real checkout.
    pub is_bare: bool,
    /// The full commit id `HEAD` resolves to, or `None` for an unborn branch.
    pub head_commit: Option<String>,
    /// The short branch name if `HEAD` is a real branch ref, `None` if detached or bare.
    pub branch: Option<String>,
    /// Whether `HEAD` is detached (a real commit checked out directly, not via a branch ref).
    pub is_detached: bool,
    /// Whether the worktree is locked (`git worktree lock`).
    pub is_locked: bool,
    /// The reason given when locked, if any and non-empty.
    pub lock_reason: Option<String>,
    /// Whether `git` itself considers this worktree prunable: its administrative metadata
    /// points at a working tree that's no longer there (e.g. the directory was deleted by hand
    /// outside of `git worktree remove`).
    pub is_prunable: bool,
    /// The reason `git` gives for prunability, if any and non-empty (typically names the
    /// missing path).
    pub prunable_reason: Option<String>,
}

/// Lists every worktree of the repository at `repo_path` by shelling out to
/// `git worktree list --porcelain` and parsing its stable, machine-readable output (see
/// [`parse_worktree_list_porcelain`]'s docs for why the porcelain form specifically, not the
/// human-readable default). See [`WorktreeStatus`]'s docs for why this exists alongside the
/// `gix`-backed [`list_worktrees`] rather than replacing it.
///
/// Returns `Err` if `repo_path` is not inside a git repository at all (or any other case `git
/// worktree list` itself fails for) - there is no per-entry fallibility the way
/// [`list_worktrees`] has, since `git` itself already resolved every entry by the time this
/// output exists.
///
/// Performs blocking I/O; see the crate-level docs.
pub fn list_worktrees_porcelain(repo_path: &Path) -> Result<Vec<WorktreeStatus>, Error> {
    let args: Vec<OsString> = vec!["worktree".into(), "list".into(), "--porcelain".into()];
    let output = run_git(repo_path, &args)?;
    check_success(&args, &output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_worktree_list_porcelain(&stdout))
}

/// Parses `git worktree list --porcelain`'s output into [`WorktreeStatus`] rows.
///
/// Deliberately parses the porcelain form, not the human-readable default table: the plain
/// format is genuinely ambiguous for a path containing spaces (nothing marks where the path
/// ends and the next column begins), while porcelain gives one `key`-prefixed line per fact,
/// blank-line-separated per worktree, with the path (and lock/prunable reasons) as the *entire*
/// remainder of their line - so this only ever splits each line on its first space
/// (`str::split_once(' ')`), never on whitespace generally, and never truncates a
/// space-containing value.
///
/// Unrecognized keys (a future `git` version adding a new porcelain field) are silently
/// ignored rather than treated as a parse error - forward compatible with any output field this
/// type doesn't (yet) model, matching git's porcelain contract that consumers may see and must
/// tolerate new fields it hasn't documented here.
///
/// CRLF line endings are normalized to LF first, so this parses identically on Windows.
pub fn parse_worktree_list_porcelain(text: &str) -> Vec<WorktreeStatus> {
    let text = text.replace("\r\n", "\n");
    let mut items = Vec::new();

    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }

        let mut path: Option<PathBuf> = None;
        let mut head_commit = None;
        let mut branch = None;
        let mut is_bare = false;
        let mut is_detached = false;
        let mut is_locked = false;
        let mut lock_reason = None;
        let mut is_prunable = false;
        let mut prunable_reason = None;

        for line in block.lines() {
            if line.is_empty() {
                continue;
            }
            let (key, rest) = match line.split_once(' ') {
                Some((key, rest)) => (key, Some(rest)),
                None => (line, None),
            };
            match key {
                "worktree" => path = rest.map(PathBuf::from),
                "HEAD" => head_commit = rest.map(str::to_string),
                "branch" => {
                    branch = rest.map(|full_ref| {
                        full_ref
                            .strip_prefix("refs/heads/")
                            .unwrap_or(full_ref)
                            .to_string()
                    })
                }
                "bare" => is_bare = true,
                "detached" => is_detached = true,
                "locked" => {
                    is_locked = true;
                    lock_reason = rest.map(str::to_string).filter(|reason| !reason.is_empty());
                }
                "prunable" => {
                    is_prunable = true;
                    prunable_reason = rest.map(str::to_string).filter(|reason| !reason.is_empty());
                }
                _ => {}
            }
        }

        let Some(path) = path else {
            // A block with no `worktree` line at all isn't a real entry (shouldn't happen for
            // real `git` output, but a stray blank run must not fabricate a row).
            continue;
        };
        // Only the very first *real* entry can be the main worktree - and a bare repository's
        // own entry (always first, if present) never is one, mirroring `list_worktrees`'s own
        // "a bare repo has no main worktree" rule.
        let is_main = items.is_empty() && !is_bare;
        items.push(WorktreeStatus {
            path,
            is_main,
            is_bare,
            head_commit,
            branch,
            is_detached,
            is_locked,
            lock_reason,
            is_prunable,
            prunable_reason,
        });
    }

    items
}

/// Resolves `$GIT_COMMON_DIR` for the repository at `repo_path`: the one directory shared by
/// the main worktree and every linked worktree (`<repo>/.git` for a non-bare repo, the bare
/// repo's own directory for a bare one). `app::rail::worktree_watch` watches this directory's
/// `worktrees` subdirectory and its own `HEAD` file directly, rather than re-deriving the same
/// path with ad hoc string logic.
///
/// Shells out to `git rev-parse --git-common-dir` and resolves a relative result (`git` prints
/// one when run from inside the repository, e.g. `.git`) against `repo_path` - mirroring
/// [`absolutize`]'s reasoning for every other path this crate hands back.
///
/// Performs blocking I/O; see the crate-level docs.
pub fn git_common_dir(repo_path: &Path) -> Result<PathBuf, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "--git-common-dir".into()];
    let output = run_git(repo_path, &args)?;
    check_success(&args, &output)?;
    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim();
    Ok(absolutize(Path::new(trimmed), repo_path))
}

/// If `path` is relative, resolve it against `base` instead of the process's current
/// working directory - every function in this module that runs `git` uses
/// `current_dir(repo_path)` (or the worktree-local equivalent), so a relative
/// `worktree_path` must resolve the same way `git` itself would.
fn absolutize(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Create a new worktree at `worktree_path` for the repository at `repo_path`.
///
/// Shells out to `git worktree add`. If `branch` is given, a new branch by that name is
/// created and checked out (`-b <branch>`); otherwise the existing branch or commit named
/// by `commit_ish` is checked out directly. If both are `None`, `git` picks its own
/// default (typically `HEAD`).
///
/// `worktree_path` and `commit_ish` are always passed as positional arguments after a `--`
/// terminator, so a value that happens to look like a flag (e.g. a `commit_ish` of
/// `"--detach"`) can never be misparsed as one.
///
/// Performs blocking I/O; see the crate-level docs.
pub fn add_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    branch: Option<&str>,
    commit_ish: Option<&str>,
) -> Result<(), Error> {
    let worktree_path = absolutize(worktree_path, repo_path);

    let mut args: Vec<OsString> = vec!["worktree".into(), "add".into()];
    if let Some(branch) = branch {
        args.push("-b".into());
        args.push(branch.into());
    }
    args.push("--".into());
    args.push(worktree_path.into_os_string());
    if let Some(commit_ish) = commit_ish {
        args.push(commit_ish.into());
    }

    let output = run_git(repo_path, &args)?;
    check_success(&args, &output)
}

/// Remove the worktree at `worktree_path` from the repository at `repo_path`.
///
/// Unless `force` is `true`, this refuses to remove a worktree with uncommitted changes:
/// modifications to tracked files, staged changes, or the presence of untracked files are
/// all treated conservatively as "dirty" and cause this to return
/// [`Error::DirtyWorktree`] without touching anything.
///
/// Passing `force: true` passes `--force` to git *twice*. A single `--force` is enough to
/// override a dirty worktree, but git additionally requires it twice to override a
/// *locked* worktree ("`use 'remove -f -f' to override or unlock first`"); passing it
/// twice unconditionally means `force: true` really does mean "remove regardless" for both
/// cases, rather than silently still refusing locked worktrees.
///
/// `worktree_path` is always passed as a positional argument after a `--` terminator, so a
/// path starting with `-` can never be misparsed as a flag.
///
/// Performs blocking I/O; see the crate-level docs.
pub fn remove_worktree(repo_path: &Path, worktree_path: &Path, force: bool) -> Result<(), Error> {
    let worktree_path = absolutize(worktree_path, repo_path);

    if !force && worktree_path.is_dir() && is_dirty(&worktree_path)? {
        return Err(Error::DirtyWorktree {
            path: worktree_path,
        });
    }

    let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
    if force {
        args.push("--force".into());
        args.push("--force".into());
    }
    args.push("--".into());
    args.push(worktree_path.into_os_string());

    let output = run_git(repo_path, &args)?;
    check_success(&args, &output)
}

/// Conservatively determine whether `worktree_dir` has uncommitted changes: any modified
/// tracked file (staged or not) or any untracked file counts as dirty.
///
/// Reads at most one byte of `git status --porcelain` output - any output at all means
/// dirty - so a huge untracked tree is never buffered in memory. Since the child may still
/// be writing when we stop reading, we kill it rather than risk blocking on a full pipe,
/// then wait it to reap the process.
///
/// Public because the session rail's "by project" mode needs the same clean/dirty signal
/// too (see `app::rail`'s docs), not just [`remove_worktree`] internally.
///
/// Performs blocking I/O; see the crate-level docs.
pub fn is_dirty(worktree_dir: &Path) -> Result<bool, Error> {
    let args: Vec<OsString> = vec![
        "status".into(),
        "--porcelain".into(),
        "--untracked-files=normal".into(),
    ];
    let mut child = git_command(worktree_dir, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::GitSpawn {
            args: format_args(&args),
            source,
        })?;

    let mut stdout = child.stdout.take().ok_or_else(|| Error::GitSpawn {
        args: format_args(&args),
        source: std::io::Error::other("git status stdout was not piped"),
    })?;

    let mut probe = [0u8; 1];
    let found_output = loop {
        match stdout.read(&mut probe) {
            Ok(0) => break false,
            Ok(_) => break true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::WorktreeIo(err));
            }
        }
    };
    drop(stdout);

    if found_output {
        // We already have our answer; don't risk blocking on a full pipe by waiting for
        // the rest of a potentially large status listing to be written.
        let _ = child.kill();
        let _ = child.wait();
        return Ok(true);
    }

    let mut stderr = String::new();
    if let Some(mut stderr_pipe) = child.stderr.take() {
        let _ = stderr_pipe.read_to_string(&mut stderr);
    }
    let status = child.wait().map_err(|source| Error::GitSpawn {
        args: format_args(&args),
        source,
    })?;
    if status.success() {
        Ok(false)
    } else {
        Err(Error::GitCommand {
            args: format_args(&args),
            exit: GitExit::from_status(&status),
            stderr,
        })
    }
}

/// Environment variables that could redirect a `git` invocation to a different repository,
/// worktree, or index than the one we explicitly select via `current_dir`. Cleared so a
/// value inherited from the parent process's environment (plausible for a GUI app launched
/// from an arbitrary shell or tool) can't silently make us read or write the wrong repo.
const GIT_ENV_OVERRIDES: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

/// Build a `git` [`Command`] running in `dir` with `args`: environment scrubbed of
/// [`GIT_ENV_OVERRIDES`], and stdin closed so an interactive prompt (credentials, GPG,
/// askpass) fails fast instead of hanging forever with no way to cancel it.
fn git_command(dir: &Path, args: &[OsString]) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir).args(args).stdin(Stdio::null());
    for var in GIT_ENV_OVERRIDES {
        command.env_remove(var);
    }
    command
}

/// Run `git` with `args` in `dir` to completion, returning its raw [`Output`] regardless
/// of exit status.
fn run_git(dir: &Path, args: &[OsString]) -> Result<Output, Error> {
    git_command(dir, args)
        .output()
        .map_err(|source| Error::GitSpawn {
            args: format_args(args),
            source,
        })
}

/// Turn a successfully-spawned but possibly-failed `git` invocation into a `Result`.
fn check_success(args: &[OsString], output: &Output) -> Result<(), Error> {
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::GitCommand {
            args: format_args(args),
            exit: GitExit::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn format_args(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Run `git` with `args` in `dir` and panic with full output on failure. Test-only
    /// helper, not part of the library's public error handling.
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

    /// Turn a freshly-created directory into a repository with one commit on branch
    /// `main`.
    fn init_repo_at(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
        fs::write(dir.join("file.txt"), "hello\n").expect("write file");
        git(dir, &["add", "file.txt"]);
        git(dir, &["commit", "-m", "initial commit"]);
    }

    /// Initialize a fresh repository in a tempdir with one commit on branch `main`.
    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        init_repo_at(dir.path());
        dir
    }

    /// `list_worktrees`, asserting the outer call succeeds and every per-worktree entry
    /// succeeds too, for tests where every entry is expected to be readable.
    fn list_ok(repo_path: &Path) -> Vec<Worktree> {
        list_worktrees(repo_path)
            .expect("list_worktrees")
            .into_iter()
            .map(|entry| entry.expect("worktree entry"))
            .collect()
    }

    #[test]
    fn lists_main_worktree_only() {
        let repo = init_repo();
        let worktrees = list_ok(repo.path());
        assert_eq!(worktrees.len(), 1);
        let main = &worktrees[0];
        assert!(main.is_main);
        assert!(!main.is_locked);
        assert_eq!(main.lock_reason, None);
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(main.head_commit.is_some());
        assert_eq!(
            fs::canonicalize(&main.path).expect("canonicalize"),
            fs::canonicalize(repo.path()).expect("canonicalize")
        );
    }

    #[test]
    fn lists_main_and_linked_worktrees() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("linked-wt");
        // Remove the dir itself; `git worktree add` creates it.
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

        let worktrees = list_ok(repo.path());
        assert_eq!(worktrees.len(), 2);

        let main = worktrees.iter().find(|w| w.is_main).expect("main worktree");
        assert_eq!(main.branch.as_deref(), Some("main"));

        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("linked worktree");
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        assert!(!linked.is_locked);
        assert_eq!(
            fs::canonicalize(&linked.path).expect("canonicalize"),
            fs::canonicalize(&linked_path).expect("canonicalize")
        );
        // Both worktrees point at the same commit, since `feature` branched off `main`
        // without any new commits.
        assert_eq!(main.head_commit, linked.head_commit);
    }

    #[test]
    fn add_worktree_creates_branch_and_checkout() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("new-wt");
        drop(linked_dir);

        add_worktree(repo.path(), &linked_path, Some("feature-a"), None)
            .expect("add_worktree should succeed");

        assert!(linked_path.join("file.txt").is_file());
        let worktrees = list_ok(repo.path());
        assert_eq!(worktrees.len(), 2);
        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("linked worktree");
        assert_eq!(linked.branch.as_deref(), Some("feature-a"));
    }

    #[test]
    fn add_worktree_checks_out_existing_branch_via_commit_ish() {
        let repo = init_repo();
        git(repo.path(), &["branch", "existing-branch"]);

        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("commit-ish-wt");
        drop(linked_dir);

        add_worktree(repo.path(), &linked_path, None, Some("existing-branch"))
            .expect("add_worktree with commit_ish should check out the existing branch");

        let worktrees = list_ok(repo.path());
        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("linked worktree");
        assert_eq!(linked.branch.as_deref(), Some("existing-branch"));
    }

    #[test]
    fn add_worktree_rejects_flag_like_commit_ish_as_positional() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("dashy-wt");
        drop(linked_dir);

        // `--detach` looks like a flag but, thanks to the `--` terminator, must be treated
        // as a (nonexistent) commit-ish instead: git reports it as a bad revision rather
        // than accepting or misinterpreting it as an actual `--detach` option.
        let err = add_worktree(repo.path(), &linked_path, None, Some("--detach"))
            .expect_err("a flag-shaped commit-ish must fail as a bad revision");
        match err {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.to_lowercase().contains("invalid reference")
                        || stderr.to_lowercase().contains("--detach"),
                    "unexpected stderr: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        assert!(!linked_path.exists());
    }

    #[test]
    fn detached_head_worktree_has_no_branch() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("detached-wt");
        drop(linked_dir);

        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--detach",
                "--",
                linked_path.to_str().expect("utf8 path"),
                "main",
            ],
        );

        let worktrees = list_ok(repo.path());
        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("linked worktree");
        assert_eq!(linked.branch, None);
        assert!(linked.head_commit.is_some());
    }

    #[test]
    fn list_reports_lock_state_with_reason() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("locked-wt");
        drop(linked_dir);
        add_worktree(repo.path(), &linked_path, Some("locked-branch"), None).expect("add_worktree");
        git(
            repo.path(),
            &[
                "worktree",
                "lock",
                "--reason",
                "external disk",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        let worktrees = list_ok(repo.path());
        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("linked worktree");
        assert!(linked.is_locked);
        assert_eq!(linked.lock_reason.as_deref(), Some("external disk"));
    }

    #[test]
    fn list_reports_lock_without_reason_as_none() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("locked-no-reason-wt");
        drop(linked_dir);
        add_worktree(
            repo.path(),
            &linked_path,
            Some("locked-no-reason-branch"),
            None,
        )
        .expect("add_worktree");
        git(
            repo.path(),
            &["worktree", "lock", linked_path.to_str().expect("utf8 path")],
        );

        let worktrees = list_ok(repo.path());
        let linked = worktrees
            .iter()
            .find(|w| !w.is_main)
            .expect("linked worktree");
        assert!(linked.is_locked);
        assert_eq!(
            linked.lock_reason, None,
            "an empty `locked` file must surface as None, not Some(\"\")"
        );
    }

    #[test]
    fn remove_force_removes_locked_worktree() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("locked-remove-wt");
        drop(linked_dir);
        add_worktree(
            repo.path(),
            &linked_path,
            Some("locked-remove-branch"),
            None,
        )
        .expect("add_worktree");
        git(
            repo.path(),
            &[
                "worktree",
                "lock",
                "--reason",
                "external disk",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        remove_worktree(repo.path(), &linked_path, true).expect(
            "force=true must remove a locked worktree too (git requires --force twice for this)",
        );
        assert!(!linked_path.exists());
    }

    #[test]
    fn list_worktrees_on_bare_repo_skips_main_entry() {
        let source = init_repo();
        let bare_dir = TempDir::new().expect("tempdir");
        git(
            bare_dir.path(),
            &[
                "clone",
                "--bare",
                source.path().to_str().expect("utf8"),
                ".",
            ],
        );

        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("bare-linked-wt");
        drop(linked_dir);
        add_worktree(bare_dir.path(), &linked_path, None, Some("main"))
            .expect("add_worktree on a bare repo");

        let worktrees = list_ok(bare_dir.path());
        assert_eq!(
            worktrees.len(),
            1,
            "a bare repo has no main worktree to report"
        );
        assert!(!worktrees[0].is_main);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn remove_clean_worktree_without_force_succeeds() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("clean-wt");
        drop(linked_dir);

        add_worktree(repo.path(), &linked_path, Some("clean-branch"), None).expect("add_worktree");
        assert!(linked_path.is_dir());

        remove_worktree(repo.path(), &linked_path, false).expect("remove_worktree");
        assert!(!linked_path.exists());
    }

    #[test]
    fn remove_refuses_dirty_tracked_file_without_force() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("dirty-wt");
        drop(linked_dir);

        add_worktree(repo.path(), &linked_path, Some("dirty-branch"), None).expect("add_worktree");

        // Dirty a tracked file without committing.
        fs::write(linked_path.join("file.txt"), "changed\n").expect("write");

        let err = remove_worktree(repo.path(), &linked_path, false)
            .expect_err("remove should refuse a dirty worktree");
        match err {
            Error::DirtyWorktree { path } => {
                assert_eq!(
                    fs::canonicalize(&path).expect("canonicalize"),
                    fs::canonicalize(&linked_path).expect("canonicalize")
                );
            }
            other => panic!("expected Error::DirtyWorktree, got {other:?}"),
        }
        assert!(
            linked_path.is_dir(),
            "worktree directory must still exist after a refused removal"
        );

        // Now force it.
        remove_worktree(repo.path(), &linked_path, true).expect("forced remove should succeed");
        assert!(!linked_path.exists());
    }

    #[test]
    fn remove_refuses_untracked_file_without_force() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("untracked-wt");
        drop(linked_dir);

        add_worktree(repo.path(), &linked_path, Some("untracked-branch"), None)
            .expect("add_worktree");

        // An untracked file must also count as dirty.
        fs::write(linked_path.join("new_untracked.txt"), "surprise\n").expect("write");

        let err = remove_worktree(repo.path(), &linked_path, false)
            .expect_err("remove should refuse a worktree with untracked files");
        assert!(matches!(err, Error::DirtyWorktree { .. }));
        assert!(linked_path.is_dir());

        remove_worktree(repo.path(), &linked_path, true).expect("forced remove should succeed");
        assert!(!linked_path.exists());
    }

    #[test]
    fn remove_detects_dirty_worktree_via_relative_path() {
        // `worktree_path` given to `remove_worktree` here is *relative*. The dirty check
        // and the actual `git worktree remove` call must resolve it against the same base
        // (`repo_path`), not against the test process's own (unrelated) current working
        // directory, or the dirty check would silently look at the wrong directory (or a
        // nonexistent one) while the real removal still hit the correct worktree.
        let container = TempDir::new().expect("tempdir");
        let repo_path = container.path().join("repo");
        fs::create_dir(&repo_path).expect("mkdir repo");
        init_repo_at(&repo_path);

        let linked_path = container.path().join("linked");
        add_worktree(&repo_path, &linked_path, Some("relative-branch"), None)
            .expect("add_worktree");
        fs::write(linked_path.join("file.txt"), "changed\n").expect("write");

        let relative = Path::new("../linked");

        let err = remove_worktree(&repo_path, relative, false)
            .expect_err("a dirty worktree reached via a relative path must still be refused");
        assert!(matches!(err, Error::DirtyWorktree { .. }));
        assert!(linked_path.is_dir());

        remove_worktree(&repo_path, relative, true)
            .expect("forced remove via a relative path should succeed");
        assert!(!linked_path.exists());
    }

    // --- `parse_worktree_list_porcelain` (pure, no git process needed) -----------------------

    #[test]
    fn parses_a_single_worktree_on_a_real_branch() {
        let text = "worktree /repo\nHEAD deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\nbranch refs/heads/main\n";
        let items = parse_worktree_list_porcelain(text);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, PathBuf::from("/repo"));
        assert_eq!(
            items[0].head_commit.as_deref(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
        assert_eq!(items[0].branch.as_deref(), Some("main"));
        assert!(items[0].is_main, "the first entry is the main worktree");
        assert!(!items[0].is_bare);
        assert!(!items[0].is_detached);
        assert!(!items[0].is_locked);
        assert!(!items[0].is_prunable);
    }

    #[test]
    fn parses_multiple_worktrees_separated_by_blank_lines() {
        let text = "worktree /repo\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /repo-wt/feature\nHEAD bbbb\nbranch refs/heads/feature\n";
        let items = parse_worktree_list_porcelain(text);
        assert_eq!(items.len(), 2);
        assert!(items[0].is_main);
        assert!(
            !items[1].is_main,
            "only the first entry is ever the main worktree"
        );
        assert_eq!(items[1].path, PathBuf::from("/repo-wt/feature"));
        assert_eq!(items[1].branch.as_deref(), Some("feature"));
    }

    #[test]
    fn a_path_containing_spaces_is_not_truncated() {
        // The whole point of parsing the porcelain form: naively splitting on whitespace would
        // cut this path off after "My".
        let text =
            "worktree /Users/dev/My Projects/feature wt\nHEAD aaaa\nbranch refs/heads/feature\n";
        let items = parse_worktree_list_porcelain(text);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].path,
            PathBuf::from("/Users/dev/My Projects/feature wt")
        );
    }

    #[test]
    fn detached_head_has_no_branch_and_is_flagged_detached() {
        let text = "worktree /repo-wt/detached\nHEAD cccc\ndetached\n";
        let items = parse_worktree_list_porcelain(text);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_detached);
        assert_eq!(items[0].branch, None);
        assert_eq!(items[0].head_commit.as_deref(), Some("cccc"));
    }

    #[test]
    fn locked_with_reason_is_captured() {
        let text =
            "worktree /repo-wt/locked\nHEAD aaaa\nbranch refs/heads/locked\nlocked external disk\n";
        let items = parse_worktree_list_porcelain(text);
        assert!(items[0].is_locked);
        assert_eq!(items[0].lock_reason.as_deref(), Some("external disk"));
    }

    #[test]
    fn locked_with_no_reason_surfaces_as_none() {
        let text = "worktree /repo-wt/locked\nHEAD aaaa\nbranch refs/heads/locked\nlocked\n";
        let items = parse_worktree_list_porcelain(text);
        assert!(items[0].is_locked);
        assert_eq!(items[0].lock_reason, None);
    }

    #[test]
    fn prunable_with_reason_is_captured() {
        let text = "worktree /repo-wt/gone\nHEAD aaaa\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n";
        let items = parse_worktree_list_porcelain(text);
        assert!(items[0].is_prunable);
        assert_eq!(
            items[0].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
    }

    #[test]
    fn bare_entry_is_flagged_bare_and_is_never_main() {
        let text = "worktree /bare-repo\nbare\n\nworktree /bare-repo-wt/linked\nHEAD aaaa\nbranch refs/heads/linked\n";
        let items = parse_worktree_list_porcelain(text);
        assert_eq!(items.len(), 2);
        assert!(items[0].is_bare);
        assert!(!items[0].is_main, "a bare entry is never the main worktree");
        assert!(!items[1].is_bare);
        assert!(
            !items[1].is_main,
            "the linked worktree after a bare entry still isn't main"
        );
    }

    #[test]
    fn empty_output_maps_to_empty_list() {
        assert_eq!(parse_worktree_list_porcelain(""), Vec::new());
    }

    #[test]
    fn crlf_line_endings_parse_identically_to_lf() {
        let text = "worktree /repo\r\nHEAD aaaa\r\nbranch refs/heads/main\r\n";
        let items = parse_worktree_list_porcelain(text);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].branch.as_deref(), Some("main"));
    }

    // --- `list_worktrees_porcelain` / `git_common_dir` (real git process) --------------------

    #[test]
    fn list_worktrees_porcelain_reports_main_and_linked() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("porcelain-linked-wt");
        drop(linked_dir);
        add_worktree(repo.path(), &linked_path, Some("porcelain-feature"), None)
            .expect("add_worktree");

        let items = list_worktrees_porcelain(repo.path()).expect("list_worktrees_porcelain");
        assert_eq!(items.len(), 2);

        let main = items.iter().find(|item| item.is_main).expect("main entry");
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(!main.is_locked);
        assert!(!main.is_prunable);

        let linked = items
            .iter()
            .find(|item| !item.is_main)
            .expect("linked entry");
        assert_eq!(linked.branch.as_deref(), Some("porcelain-feature"));
        assert_eq!(
            fs::canonicalize(&linked.path).expect("canonicalize"),
            fs::canonicalize(&linked_path).expect("canonicalize")
        );
    }

    #[test]
    fn list_worktrees_porcelain_reports_detached_head() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("porcelain-detached-wt");
        drop(linked_dir);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--detach",
                "--",
                linked_path.to_str().expect("utf8 path"),
                "main",
            ],
        );

        let items = list_worktrees_porcelain(repo.path()).expect("list_worktrees_porcelain");
        let linked = items
            .iter()
            .find(|item| !item.is_main)
            .expect("linked entry");
        assert!(linked.is_detached);
        assert_eq!(linked.branch, None);
        assert!(linked.head_commit.is_some());
    }

    #[test]
    fn list_worktrees_porcelain_reports_lock_reason() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("porcelain-locked-wt");
        drop(linked_dir);
        add_worktree(repo.path(), &linked_path, Some("porcelain-locked"), None)
            .expect("add_worktree");
        git(
            repo.path(),
            &[
                "worktree",
                "lock",
                "--reason",
                "on a USB drive",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        let items = list_worktrees_porcelain(repo.path()).expect("list_worktrees_porcelain");
        let linked = items
            .iter()
            .find(|item| !item.is_main)
            .expect("linked entry");
        assert!(linked.is_locked);
        assert_eq!(linked.lock_reason.as_deref(), Some("on a USB drive"));
    }

    /// The real, honest reproduction of the issue's "prunable / missing worktree" case: the
    /// worktree's own directory is deleted by hand (not via `git worktree remove`), leaving
    /// git's own administrative metadata pointing at nothing. `git worktree list --porcelain`
    /// is expected to flag this itself - this test asserts that real git behavior, not an
    /// assumption about it.
    #[test]
    fn list_worktrees_porcelain_flags_a_manually_deleted_worktree_as_prunable() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("porcelain-prunable-wt");
        drop(linked_dir);
        add_worktree(repo.path(), &linked_path, Some("porcelain-prunable"), None)
            .expect("add_worktree");

        fs::remove_dir_all(&linked_path).expect("manually delete the worktree directory");

        let items = list_worktrees_porcelain(repo.path()).expect("list_worktrees_porcelain");
        let linked = items
            .iter()
            .find(|item| !item.is_main)
            .expect("linked entry");
        assert!(
            linked.is_prunable,
            "git itself must flag a worktree whose directory vanished as prunable"
        );
    }

    #[test]
    fn list_worktrees_porcelain_on_a_non_repository_returns_err() {
        let dir = TempDir::new().expect("tempdir");
        let err = list_worktrees_porcelain(dir.path())
            .expect_err("a plain directory is not a git repository");
        assert!(matches!(err, Error::GitCommand { .. }));
    }

    #[test]
    fn git_common_dir_resolves_to_the_dot_git_directory() {
        let repo = init_repo();
        let common_dir = git_common_dir(repo.path()).expect("git_common_dir");
        assert_eq!(
            fs::canonicalize(&common_dir).expect("canonicalize"),
            fs::canonicalize(repo.path().join(".git")).expect("canonicalize")
        );
    }

    #[test]
    fn git_common_dir_from_a_linked_worktree_resolves_to_the_same_shared_directory() {
        let repo = init_repo();
        let linked_dir = TempDir::new().expect("tempdir");
        let linked_path = linked_dir.path().join("common-dir-linked-wt");
        drop(linked_dir);
        add_worktree(repo.path(), &linked_path, Some("common-dir-feature"), None)
            .expect("add_worktree");

        let from_main = git_common_dir(repo.path()).expect("git_common_dir from main");
        let from_linked = git_common_dir(&linked_path).expect("git_common_dir from linked");
        assert_eq!(
            fs::canonicalize(&from_main).expect("canonicalize"),
            fs::canonicalize(&from_linked).expect("canonicalize"),
            "every worktree of the same repository shares one common dir"
        );
    }
}
