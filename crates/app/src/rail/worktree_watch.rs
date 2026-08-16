//! Real OS-level filesystem watching that backs the worktree sidebar's live refresh (GitHub
//! issue #12: "the worktrees panel is populated once and never invalidated").
//!
//! `git worktree add`/`remove`/`lock` all mutate files under `$GIT_COMMON_DIR/worktrees/**`
//! (creating/removing each linked worktree's own admin subdirectory, or its `locked` file), and
//! a branch switch inside *any* worktree rewrites that worktree's own `HEAD` file. For every
//! linked worktree, that `HEAD` lives inside its own admin subdirectory under
//! `$GIT_COMMON_DIR/worktrees/<name>/HEAD` - already covered by a single recursive watch on
//! `worktrees/` itself (`notify`'s recursive mode picks up newly created subdirectories as they
//! appear, so this doesn't need to be re-armed every time a worktree is added). The *main*
//! worktree is the one exception: its `HEAD` lives directly at `$GIT_COMMON_DIR/HEAD`, one level
//! above `worktrees/`, so it needs its own separate, explicit watch.
//!
//! Both watches are set up **once**, from `repo_path` alone, when
//! `crate::root::AdeApp::start_worktree_watch` first runs - multi-repo support is explicitly out
//! of scope for this feature (see the issue), so `repo_path` never changes over the app's
//! lifetime and neither does `$GIT_COMMON_DIR`; there is no dynamic per-worktree watch set to
//! maintain.
//!
//! What this can't catch: a worktree's own working-directory being deleted by hand (`rm -rf`)
//! touches nothing under `$GIT_COMMON_DIR` at all - `crate::root::AdeApp::start_worktree_watch`'s
//! 5s poll fallback is what notices that case (and is also the sole backstop if watcher setup
//! itself failed - see [`spawn_worktree_watcher`]'s docs).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Set by the watcher's real OS-thread event callback whenever it observes *any* change under
/// the watched paths, and cleared by the refresh loop once it has acted on it.
///
/// A plain `AtomicBool` rather than a channel of individual events: the refresh loop only ever
/// asks "has anything changed since I last looked", never *what* changed or *how many* events
/// fired - which is exactly the coalescing the issue's "a single `git worktree add` touches
/// several files; collapse to one refresh" debounce requirement wants. Many events between two
/// checks collapse into the identical single `true`.
pub type DirtyFlag = Arc<AtomicBool>;

/// Starts a real filesystem watcher for `repo_path`'s worktree admin state (see this module's
/// own docs for exactly which paths, and why those two are sufficient) and returns the live
/// [`RecommendedWatcher`] - it must be kept alive by the caller (see
/// `crate::root::AdeApp::_worktree_watcher`'s own docs) for the OS-level watch to keep running;
/// dropping the returned value silently stops all notifications with no error of any kind, since
/// that's simply how the `notify` crate's `Watcher` works.
///
/// Returns `None` if `repo_path` isn't inside a git repository at all
/// (`wt_core::git_common_dir` failing), or if the underlying OS watcher itself couldn't be
/// constructed - both real, honestly-reported "there is nothing to watch" cases, not treated as
/// fatal: `crate::root::AdeApp::start_worktree_watch`'s poll fallback still runs regardless, so a
/// `None` here only means every refresh comes from that timer instead of also being able to fire
/// early.
///
/// A repository that has never had a linked worktree has no `worktrees/` subdirectory yet to
/// watch (a `notify` watch target must already exist) - this falls back to watching
/// `$GIT_COMMON_DIR` itself in that case, which still catches `worktrees/`'s own creation (the
/// first ever `git worktree add`), non-recursively, so it doesn't also start reporting every
/// object-database write.
pub fn spawn_worktree_watcher(repo_path: &Path, dirty: DirtyFlag) -> Option<RecommendedWatcher> {
    let common_dir = wt_core::git_common_dir(repo_path).ok()?;

    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if result.is_ok() {
            dirty.store(true, Ordering::SeqCst);
        }
    })
    .ok()?;

    let worktrees_dir = common_dir.join("worktrees");
    if worktrees_dir.is_dir() {
        let _ = watcher.watch(&worktrees_dir, RecursiveMode::Recursive);
    } else {
        // No linked worktree has ever been created - watch the common dir itself so the first
        // `git worktree add` (which creates `worktrees/`) is still noticed. Non-recursive: a
        // recursive watch here would also fire on every ordinary object-database write.
        let _ = watcher.watch(&common_dir, RecursiveMode::NonRecursive);
    }

    // The main worktree's own `HEAD` - not covered by either watch above, see this module's
    // docs. Watched independently of whether the `worktrees_dir` watch above succeeded, so a
    // branch switch in the main worktree is still caught even if e.g. `worktrees/` doesn't
    // exist yet.
    let head_file = common_dir.join("HEAD");
    if head_file.is_file() {
        let _ = watcher.watch(&head_file, RecursiveMode::NonRecursive);
    }

    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Real-time bounded wait for the watcher's async, OS-thread-delivered callback to fire -
    /// there is no deterministic/simulated clock to advance here (unlike `gpui`'s test
    /// executor), since `notify`'s events come from the real kernel on a real background thread.
    /// `test_support::wait_until` is the workspace's one sanctioned wall-clock wait
    /// (`docs/testing.md`); real inotify delivery is normally sub-millisecond, so the generous
    /// ceiling only ever elapses on a genuine failure to detect the change.
    fn wait_until_dirty(dirty: &DirtyFlag) -> bool {
        test_support::wait_until(Duration::from_secs(3), || {
            dirty.swap(false, Ordering::SeqCst)
        })
    }

    #[test]
    fn a_real_worktree_add_is_noticed() {
        let repo = crate::test_support::temp_repo();
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        let container = crate::test_support::temp_root();
        let linked_path = container.path().join("added-wt");
        drop(container);
        test_support::git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "watched-feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        assert!(
            wait_until_dirty(&dirty),
            "a real `git worktree add` must be observed within the real-time budget"
        );
    }

    #[test]
    fn a_real_worktree_remove_is_noticed() {
        let repo = crate::test_support::temp_repo();
        let container = crate::test_support::temp_root();
        let linked_path = container.path().join("removed-wt");
        drop(container);
        test_support::git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "to-be-removed",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        test_support::git(
            repo.path(),
            &[
                "worktree",
                "remove",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        assert!(
            wait_until_dirty(&dirty),
            "a real `git worktree remove` must be observed within the real-time budget"
        );
    }

    #[test]
    fn a_real_worktree_lock_is_noticed() {
        let repo = crate::test_support::temp_repo();
        let container = crate::test_support::temp_root();
        let linked_path = container.path().join("locked-wt");
        drop(container);
        test_support::git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "to-be-locked",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        test_support::git(
            repo.path(),
            &[
                "worktree",
                "lock",
                "--reason",
                "external disk",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        assert!(
            wait_until_dirty(&dirty),
            "a real `git worktree lock` must be observed within the real-time budget"
        );
    }

    /// The one case this module's docs call out as needing its own explicit watch, not just the
    /// `worktrees/` recursive one: a branch switch *in the main worktree itself*, whose `HEAD`
    /// lives one level up from `worktrees/`.
    #[test]
    fn a_branch_switch_in_the_main_worktree_is_noticed() {
        let repo = crate::test_support::temp_repo();
        test_support::git(repo.path(), &["branch", "other"]);

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        test_support::git(repo.path(), &["checkout", "other"]);

        assert!(
            wait_until_dirty(&dirty),
            "a real branch switch in the main worktree must rewrite its HEAD and be observed"
        );
    }

    /// A branch switch inside a *linked* worktree - covered by the recursive `worktrees/` watch
    /// alone, since that worktree's real `HEAD` lives under there.
    #[test]
    fn a_branch_switch_in_a_linked_worktree_is_noticed() {
        let repo = crate::test_support::temp_repo();
        let container = crate::test_support::temp_root();
        let linked_path = container.path().join("switch-wt");
        drop(container);
        test_support::git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "switch-feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );
        test_support::git(&linked_path, &["branch", "switch-target"]);

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        test_support::git(&linked_path, &["checkout", "switch-target"]);

        assert!(
            wait_until_dirty(&dirty),
            "a real branch switch inside a linked worktree must be observed"
        );
    }

    #[test]
    fn a_non_repository_yields_no_watcher_rather_than_panicking() {
        let dir = crate::test_support::temp_root();
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        assert!(spawn_worktree_watcher(dir.path(), dirty).is_none());
    }

    /// The fallback path documented on [`spawn_worktree_watcher`]: a repository that has never
    /// had a linked worktree has no `worktrees/` directory yet for `notify` to watch (it must
    /// already exist), so this falls back to watching `$GIT_COMMON_DIR` itself - and the very
    /// first `git worktree add` (which creates `worktrees/`) must still be observed through
    /// that fallback, not silently missed until the next poll.
    #[test]
    fn the_very_first_worktree_add_in_a_repo_is_still_noticed_via_the_fallback_watch() {
        let repo = crate::test_support::temp_repo();
        assert!(
            !repo.path().join(".git").join("worktrees").exists(),
            "precondition: this repo has never had a linked worktree"
        );

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let watcher = spawn_worktree_watcher(repo.path(), dirty.clone());
        assert!(
            watcher.is_some(),
            "a fresh repo with no `worktrees/` dir yet must still get a real watcher, \
             falling back to watching the common dir itself"
        );
        let _watcher = watcher;

        let container = crate::test_support::temp_root();
        let linked_path = container.path().join("first-ever-wt");
        drop(container);
        test_support::git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "first-ever-feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        assert!(
            wait_until_dirty(&dirty),
            "the very first `git worktree add` in a repo must be observed via the fallback watch"
        );
    }
}
