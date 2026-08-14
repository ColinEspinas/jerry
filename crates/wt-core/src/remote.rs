//! Real git remote operations: fetch, pull, and push (with force/force-with-lease variants) -
//! GitHub issue #1's own acceptance criteria ("push (force with lease, force, no force)",
//! "pull"). Every mutation shells out to a real `git` subprocess and surfaces git's own real
//! stderr on failure ([`Error::GitCommand`]) rather than inventing a parsed/paraphrased message -
//! a merge conflict, a rejected non-fast-forward push, or a missing upstream all read as git's
//! own honest words, the same discipline every other `wt-core` module already follows.
//!
//! [`pull`] does not attempt to resolve a real merge conflict itself - it shells out to `git
//! pull` and surfaces whatever git reports, honestly, rather than leaving the working tree in a
//! conflicted state with no way back into this app's own conflict-resolution surface
//! (`crate::merge`, built for a worktree-add flow, not a mid-session pull). A conflicted pull is
//! a real, stated gap: the caller sees git's own real error text, and the worktree is left
//! exactly where a real `git pull` on the command line would leave it.

use std::ffi::OsString;
use std::path::Path;

use crate::error::Error;
use crate::{check_success, run_git};

/// Real `git fetch` for `worktree_path`'s configured remote (whatever a bare `git fetch` itself
/// resolves to - almost always `origin`). Updates remote-tracking refs only; never touches the
/// working tree or the current branch.
///
/// Performs blocking I/O.
pub fn fetch(worktree_path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["fetch".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// Real `git pull` (fetch + merge into the current branch) for `worktree_path`. A merge
/// conflict, a detached HEAD, or uncommitted changes that would be overwritten all surface as
/// git's own real stderr via [`Error::GitCommand`] - see this module's own docs on why a
/// conflicted pull is not resolved here.
///
/// Performs blocking I/O.
pub fn pull(worktree_path: &Path) -> Result<(), Error> {
    let args: Vec<OsString> = vec!["pull".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// How hard [`push`] should push - `git push`'s own three real postures (GitHub issue #1's own
/// "push (force with lease, force, no force)").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushForce {
    /// A plain `git push` - refuses outright on any non-fast-forward.
    None,
    /// `git push --force-with-lease` - overwrites the remote branch, but only if it still
    /// points where this worktree's own remote-tracking ref last saw it (aborts if someone
    /// else pushed in between).
    WithLease,
    /// `git push --force` - overwrites the remote branch unconditionally, even if someone else
    /// pushed in between. The one real, unguarded way to lose someone else's already-pushed
    /// work; the UI layer (`app::graph_view`) is responsible for a real, explicit two-step
    /// confirmation before this variant ever reaches here - this function itself performs no
    /// confirmation of its own, matching [`crate::undo::discard_worktree`]'s own "the caller
    /// already confirmed, this is the real mutation" division of responsibility.
    Force,
}

/// Real `git push` for `worktree_path`'s current branch, per `force`. A branch with no
/// configured upstream yet gets `--set-upstream origin <branch>` folded into the same push -
/// `origin` specifically, matching this codebase's own single-remote assumption
/// (`wt_core::graph::ahead_behind_against_upstream`'s own upstream detection already assumes a
/// clone has exactly one remote, so this isn't inventing a new one) - rather than making a
/// first-ever push fail with git's own comparatively opaque "no upstream" error and requiring a
/// second, separate recovery step.
///
/// Performs blocking I/O.
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

/// Real `git push` of an **explicit** `branch` from `worktree_path`, per `force` - the Branches
/// panel's own branch context menu "Push Branch…" (GitHub issue #241), which pushes whichever
/// branch was right-clicked rather than whatever happens to be checked out.
///
/// The only differences from [`push`] are that the branch is named rather than derived from
/// `HEAD`, and that both the upstream check and the push itself are therefore scoped to *that*
/// branch (`<branch>@{upstream}` and an explicit `origin <branch>` refspec) - a plain `git push`
/// would otherwise push `HEAD`'s branch instead, silently pushing something the user never
/// clicked. The `--set-upstream origin <branch>` fallback for a branch with no upstream yet is
/// identical, for the identical reason (see [`push`]'s own docs).
///
/// The remote is always `origin`, whether or not an upstream is already configured - unlike
/// [`push`], which (having no refspec at all when an upstream exists) lets git route the push to
/// whatever remote that branch's upstream names. Pushing a branch that is *not* checked out
/// requires naming a remote and a branch explicitly, so there is no "let git decide" form
/// available here, and `origin` is this module's own already-documented single-remote assumption
/// (see [`push`]'s docs). In a repository with exactly one remote - which is what
/// `crate::graph::ahead_behind_against_upstream` already assumes across this crate - the two are
/// the same push.
///
/// A non-fast-forward, a missing remote, or a branch name that doesn't exist all surface as git's
/// own real stderr via [`Error::GitCommand`] - nothing is pre-checked. Still accepts the full
/// [`PushForce`] even though the branch context menu only ever passes [`PushForce::None`]: the
/// posture is git's own, and there is no reason for this function to be able to do less than
/// [`push`] already can.
///
/// Performs blocking I/O.
pub fn push_branch(worktree_path: &Path, branch: &str, force: PushForce) -> Result<(), Error> {
    let has_upstream = has_configured_upstream(worktree_path, Some(branch))?;

    let mut args = push_args(force);
    if !has_upstream {
        args.push("--set-upstream".into());
    }
    args.push("origin".into());
    args.push(branch.into());
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)
}

/// The `git push` argument prefix both [`push`] and [`push_branch`] start from - `push` plus
/// whatever flag `force` really means. Shared so the two can never drift into disagreeing about
/// what a given [`PushForce`] does.
fn push_args(force: PushForce) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["push".into()];
    match force {
        PushForce::None => {}
        PushForce::WithLease => args.push("--force-with-lease".into()),
        PushForce::Force => args.push("--force".into()),
    }
    args
}

/// The real, current branch's short name (`git rev-parse --abbrev-ref HEAD`) - used only for
/// [`push`]'s own `--set-upstream origin <branch>` fallback.
fn current_branch_name(worktree_path: &Path) -> Result<String, Error> {
    let args: Vec<OsString> = vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()];
    let output = run_git(worktree_path, &args)?;
    check_success(&args, &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether a branch already has a configured upstream - the exact same real `git rev-parse
/// --abbrev-ref @{upstream}` resolution
/// [`crate::graph::ahead_behind_against_upstream`] already uses for the identical question, so
/// the two never independently disagree about what counts as "has an upstream". A non-zero exit
/// here is the expected, honest "no upstream configured" signal, not a real error to propagate.
///
/// `branch` is `None` for `HEAD`'s own branch (the bare `@{upstream}` [`push`] has always used,
/// unchanged) and `Some(name)` for an explicit branch ([`push_branch`]), which needs
/// `<name>@{upstream}` instead - the bare form would answer for whatever is checked out, not for
/// the branch actually being pushed.
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

    /// A real clone of `remote` with a real committer identity configured (a bare `git init`
    /// clone has none) - every test in this module needs exactly this starting point.
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
        // Advance the remote again after the local clone, so `fetch` has something real to do.
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
        // Diverge both sides on the same file/line, so the merge really conflicts.
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
        // Rewrite local history (amend) so a plain push would be a non-fast-forward, but the
        // remote itself has not moved since the clone - the real case force-with-lease exists
        // for.
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
        // Someone else pushes to the remote after the local clone's last real knowledge of it.
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
        // A second, never-pushed branch - and, critically, `main` is what stays checked out, so a
        // push that silently used `HEAD` instead of the named branch would be visible here.
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
        // A second local branch at the very same commit the remote already has, with no upstream
        // of its own - so this push has genuinely nothing to transfer, and yet a real invocation
        // still leaves an observable trace (`--set-upstream`). Without that, "succeeded as a
        // no-op" would be indistinguishable from "did nothing at all".
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
        // The remote moves on, and the local branch rewrites its own history - a genuine
        // divergence a plain push must refuse.
        commit(seed.path(), "c.txt", "1", "diverged upstream work");
        git(seed.path(), &["push", "origin", "main"]);
        commit(local.path(), "b.txt", "1", "local work");
        // Push the *named* branch from a worktree sitting on a different branch entirely, so the
        // refusal proves it really targeted `main` rather than `HEAD`.
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
