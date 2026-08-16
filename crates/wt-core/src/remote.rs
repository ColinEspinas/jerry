//! Remote operations: fetch, pull, and push with its force variants.
//!
//! Every failure surfaces git's own stderr via [`Error::GitCommand`] rather than a paraphrase.
//!
//! Known gap: [`pull`] does not resolve conflicts. `crate::merge` is built for the worktree-add
//! flow, not a mid-session pull, so a conflicted pull leaves the worktree exactly where the
//! command line would.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{check_success, run_git};

/// `git fetch` for whatever remote a bare fetch resolves to. Updates remote-tracking refs only.
pub fn fetch(worktree_path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["fetch".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// `git pull` into the current branch. Conflicts, a detached `HEAD`, and changes that would be
/// overwritten all surface as git's own stderr.
pub fn pull(worktree_path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["pull".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushForce {
    /// Refuses outright on any non-fast-forward.
    None,
    /// Overwrites the remote branch only if it still points where remote-tracking last saw it.
    WithLease,
    /// Overwrites unconditionally - the one unguarded way to lose someone else's pushed work.
    /// Confirming that is the caller's job; this performs none.
    Force,
}

/// A branch with no upstream gets `--set-upstream origin <branch>` folded into the same push,
/// rather than failing and needing a second recovery step.
pub fn push(worktree_path: &Path, force: PushForce) -> Result<(), Error> {
    let branch = current_branch_name(worktree_path)?;
    let has_upstream = has_configured_upstream(worktree_path, None)?;

    let mut args = push_args(force);
    if !has_upstream {
        args.push("--set-upstream".into());
        args.push("origin".into());
        args.push(branch.into());
    }
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Pushes an explicit `branch`, which need not be the one checked out.
///
/// Both the upstream check and the push are scoped to that branch (`<branch>@{upstream}`, and an
/// explicit `origin <branch>` refspec); a bare `git push` would send `HEAD`'s branch instead.
/// Naming a not-checked-out branch requires naming the remote too, so it is always `origin` -
/// this crate assumes a single remote throughout.
///
/// The `--` terminator is mandatory: `git push origin --evil` is otherwise read as an option
/// (`error: unknown option 'evil'`) rather than refused as a refspec.
pub fn push_branch(worktree_path: &Path, branch: &str, force: PushForce) -> Result<(), Error> {
    let has_upstream = has_configured_upstream(worktree_path, Some(branch))?;

    let mut args = push_args(force);
    if !has_upstream {
        args.push("--set-upstream".into());
    }
    args.push("origin".into());
    args.push("--".into());
    args.push(branch.into());
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

fn push_args(force: PushForce) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["push".into()];
    match force {
        PushForce::None => {}
        PushForce::WithLease => args.push("--force-with-lease".into()),
        PushForce::Force => args.push("--force".into()),
    }
    args
}

fn current_branch_name(worktree_path: &Path) -> Result<String, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether a branch has a configured upstream; a non-zero exit is the "no upstream" signal, not
/// an error to propagate.
///
/// `branch` is `None` for `HEAD`'s own branch and `Some(name)` for an explicit one, which needs
/// `<name>@{upstream}` - the bare form would answer for whatever is checked out instead.
fn has_configured_upstream(worktree_path: &Path, branch: Option<&str>) -> Result<bool, Error> {
    let upstream = match branch {
        Some(branch) => format!("{branch}@{{upstream}}"),
        None => "@{upstream}".to_string(),
    };
    let args: Vec<OsString> = vec!["rev-parse".into(), "--abbrev-ref".into(), upstream.into()];
    let output = run_git(worktree_path, &args)?;
    Ok(output.status.success())
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

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_bare_remote() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "--bare", "-b", "main"]);
        dir
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
        fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
    }

    /// A clone of `remote` with a committer identity configured, which a bare clone lacks.
    fn clone_of(remote: &Path) -> TempDir {
        let local = TempDir::new().expect("tempdir");
        git(
            local.path(),
            &["clone", remote.to_str().expect("utf8"), "."],
        );
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);
        local
    }

    #[test]
    fn fetch_really_updates_the_remote_tracking_ref() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "b.txt", "1", "second");
        git(seed.path(), &["push", "origin", "main"]);

        fetch(local.path()).expect("fetch");

        let remote_tracking_subject =
            git_output(local.path(), &["log", "-1", "--format=%s", "origin/main"]);
        assert_eq!(
            remote_tracking_subject, "second",
            "fetch must have updated the real remote-tracking ref to the remote's new tip"
        );
        let head_subject = git_output(local.path(), &["log", "-1", "--format=%s", "HEAD"]);
        assert_eq!(
            head_subject, "base",
            "fetch must never touch the local branch/working tree itself"
        );
    }

    #[test]
    fn pull_really_fast_forwards_the_local_branch() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "b.txt", "1", "second");
        git(seed.path(), &["push", "origin", "main"]);

        pull(local.path()).expect("pull");

        let head_subject = git_output(local.path(), &["log", "-1", "--format=%s", "HEAD"]);
        assert_eq!(
            head_subject, "second",
            "pull must have really fast-forwarded the local branch to the remote's new tip"
        );
    }

    #[test]
    fn pull_surfaces_a_real_conflict_as_a_real_error_not_a_panic_or_silent_success() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "a.txt", "remote change", "remote diverges");
        git(seed.path(), &["push", "origin", "main"]);
        commit(local.path(), "a.txt", "local change", "local diverges");

        let result = pull(local.path());
        assert!(
            result.is_err(),
            "a genuinely conflicting pull must surface as a real error, not a silent success"
        );
        match result.unwrap_err() {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    !stderr.is_empty(),
                    "the real git stderr must be preserved for the user to actually read"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
    }

    #[test]
    fn push_with_no_force_really_updates_the_remote_branch() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(local.path(), "b.txt", "1", "local work");

        push(local.path(), PushForce::None).expect("push");

        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "local work",
            "a plain push must really update the real remote branch"
        );
    }

    #[test]
    fn push_with_no_force_refuses_a_real_non_fast_forward() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "c.txt", "1", "diverged upstream work");
        git(seed.path(), &["push", "origin", "main"]);
        commit(local.path(), "b.txt", "1", "local work");

        let result = push(local.path(), PushForce::None);
        assert!(
            result.is_err(),
            "a real non-fast-forward must be refused, not silently forced through"
        );

        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "diverged upstream work",
            "the real remote branch must be completely untouched by the refused push"
        );
    }

    #[test]
    fn push_force_with_lease_overwrites_when_the_remote_matches_the_last_known_state() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        // Amend so a plain push is a non-fast-forward, while the remote has not moved - the
        // case force-with-lease exists for.
        commit(local.path(), "b.txt", "1", "local work");
        git(
            local.path(),
            &["commit", "--amend", "-m", "amended local work"],
        );

        push(local.path(), PushForce::WithLease).expect("push with lease");

        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(remote_subject, "amended local work");
    }

    #[test]
    fn push_force_with_lease_refuses_when_the_remote_moved_since_the_last_fetch() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "c.txt", "1", "someone else's push");
        git(seed.path(), &["push", "origin", "main"]);
        commit(local.path(), "b.txt", "1", "local work");
        git(
            local.path(),
            &["commit", "--amend", "-m", "amended local work"],
        );

        let result = push(local.path(), PushForce::WithLease);
        assert!(
            result.is_err(),
            "force-with-lease must refuse when the remote moved since this worktree last saw \
             it, not blindly overwrite someone else's already-pushed work"
        );
        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "someone else's push",
            "the real remote branch must be untouched by the refused lease push"
        );
    }

    #[test]
    fn push_force_unconditionally_overwrites_even_when_the_remote_moved() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "c.txt", "1", "someone else's push");
        git(seed.path(), &["push", "origin", "main"]);
        commit(local.path(), "b.txt", "1", "local work");
        git(
            local.path(),
            &["commit", "--amend", "-m", "amended local work"],
        );

        push(local.path(), PushForce::Force).expect("force push");

        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "amended local work",
            "a real --force push must overwrite the remote unconditionally"
        );
    }

    #[test]
    fn push_with_no_upstream_configures_one_via_set_upstream() {
        let remote = init_bare_remote();
        let local = TempDir::new().expect("tempdir");
        git(local.path(), &["init", "-b", "main"]);
        git(local.path(), &["config", "user.email", "test@example.com"]);
        git(local.path(), &["config", "user.name", "Test User"]);
        git(
            local.path(),
            &[
                "remote",
                "add",
                "origin",
                remote.path().to_str().expect("utf8"),
            ],
        );
        commit(local.path(), "a.txt", "1", "first commit, no upstream yet");

        assert!(
            !has_configured_upstream(local.path(), None).expect("has_configured_upstream"),
            "premise: a freshly `remote add`-ed repo has no upstream configured yet"
        );

        push(local.path(), PushForce::None).expect("push");

        assert!(
            has_configured_upstream(local.path(), None).expect("has_configured_upstream"),
            "push must have configured a real upstream via --set-upstream, not merely \
             succeeded without one"
        );
        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(remote_subject, "first commit, no upstream yet");
    }

    #[test]
    fn push_branch_with_no_upstream_pushes_that_branch_and_configures_one_for_it() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        git(local.path(), &["checkout", "-b", "side-branch"]);
        commit(local.path(), "b.txt", "1", "side work");
        git(local.path(), &["checkout", "main"]);

        assert!(
            !has_configured_upstream(local.path(), Some("side-branch"))
                .expect("has_configured_upstream"),
            "premise: the new branch has no upstream configured yet"
        );

        push_branch(local.path(), "side-branch", PushForce::None).expect("push_branch");

        assert!(
            has_configured_upstream(local.path(), Some("side-branch"))
                .expect("has_configured_upstream"),
            "pushing a branch with no upstream must configure a real one via --set-upstream \
             origin <branch>"
        );
        let remote_subject =
            git_output(remote.path(), &["log", "-1", "--format=%s", "side-branch"]);
        assert_eq!(
            remote_subject, "side work",
            "the named branch must really exist on the real remote now"
        );
        assert_eq!(
            git_output(local.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main",
            "pushing an explicit branch must never switch what is checked out here"
        );
    }

    #[test]
    fn push_branch_that_is_already_up_to_date_succeeds_as_a_real_no_op() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        // Nothing to transfer, so `--set-upstream`'s trace is the only thing separating
        // "succeeded as a no-op" from "did nothing at all".
        git(local.path(), &["branch", "already-there", "main"]);
        let remote_main_before = git_output(remote.path(), &["rev-parse", "main"]);
        assert!(
            !has_configured_upstream(local.path(), Some("already-there"))
                .expect("has_configured_upstream"),
            "premise: the branch has no upstream yet"
        );

        push_branch(local.path(), "already-there", PushForce::None)
            .expect("pushing a branch whose commits the remote already has must succeed");

        assert!(
            has_configured_upstream(local.path(), Some("already-there"))
                .expect("has_configured_upstream"),
            "a real push really ran: it configured the upstream, even with nothing to transfer"
        );
        assert_eq!(
            git_output(remote.path(), &["rev-parse", "already-there"]),
            remote_main_before,
            "and the branch really is on the remote, at the commit it already had"
        );
        assert_eq!(
            git_output(remote.path(), &["rev-parse", "main"]),
            remote_main_before,
            "without disturbing any other branch there"
        );
    }

    #[test]
    fn push_branch_refuses_a_real_non_fast_forward_without_force() {
        let remote = init_bare_remote();
        let seed = TempDir::new().expect("tempdir");
        git(
            seed.path(),
            &["clone", remote.path().to_str().expect("utf8"), "."],
        );
        git(seed.path(), &["config", "user.email", "test@example.com"]);
        git(seed.path(), &["config", "user.name", "Test User"]);
        commit(seed.path(), "a.txt", "1", "base");
        git(seed.path(), &["push", "origin", "main"]);

        let local = clone_of(remote.path());
        commit(seed.path(), "c.txt", "1", "diverged upstream work");
        git(seed.path(), &["push", "origin", "main"]);
        commit(local.path(), "b.txt", "1", "local work");
        // Pushed from a worktree on a different branch, so the refusal proves it targeted the
        // named branch rather than `HEAD`.
        git(local.path(), &["checkout", "-b", "elsewhere"]);

        let result = push_branch(local.path(), "main", PushForce::None);
        let err = result.expect_err("a real non-fast-forward must be refused, never forced");
        match err {
            Error::GitCommand { stderr, .. } => {
                assert!(
                    stderr.contains("rejected") || stderr.contains("non-fast-forward"),
                    "git's own real non-fast-forward refusal must be preserved: {stderr}"
                );
            }
            other => panic!("expected Error::GitCommand, got {other:?}"),
        }
        let remote_subject = git_output(remote.path(), &["log", "-1", "--format=%s", "main"]);
        assert_eq!(
            remote_subject, "diverged upstream work",
            "the real remote branch must be completely untouched by the refused push"
        );
    }
}
