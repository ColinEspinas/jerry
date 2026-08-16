//! Real OS-level filesystem watching that backs the worktree sidebar's live refresh (GitHub
//! issue #12: "the worktrees panel is populated once and never invalidated").

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Set by the watcher's real OS-thread event callback whenever it observes *any* change under
/// the watched paths, and cleared by the refresh loop once it has acted on it.
pub type DirtyFlag = Arc<AtomicBool>;

/// Starts a real filesystem watcher for `repo_path`'s worktree admin state (see this module's
/// own docs for exactly which paths, and why those two are sufficient) and returns the live
/// [`RecommendedWatcher`] - it must be kept alive by the caller (see
/// `crate::root::AdeApp::_worktree_watcher`'s own docs) for the OS-level watch to keep running;
/// dropping the returned value silently stops all notifications with no error of any kind, since
/// that's simply how the `notify` crate's `Watcher` works.
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
    use std::fs;
    use std::process::Command;
    use std::time::{Duration, Instant};
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

    /// Real-time bounded wait for the watcher's async, OS-thread-delivered callback to fire -
    /// there is no deterministic/simulated clock to advance here (unlike `gpui`'s test
    /// executor), since `notify`'s events come from the real kernel on a real background
    /// thread. A generous 3s ceiling, polled every 10ms; real inotify delivery is normally
    /// sub-millisecond, so this only ever times out on a genuine failure to detect the change.
    fn wait_until_dirty(dirty: &DirtyFlag) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if dirty.swap(false, Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_real_worktree_add_is_noticed() {
        let repo = init_repo();
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        let container = TempDir::new().expect("tempdir");
        let linked_path = container.path().join("added-wt");
        drop(container);
        git(
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
        let repo = init_repo();
        let container = TempDir::new().expect("tempdir");
        let linked_path = container.path().join("removed-wt");
        drop(container);
        git(
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

        git(
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
        let repo = init_repo();
        let container = TempDir::new().expect("tempdir");
        let linked_path = container.path().join("locked-wt");
        drop(container);
        git(
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

        assert!(
            wait_until_dirty(&dirty),
            "a real `git worktree lock` must be observed within the real-time budget"
        );
    }

    #[test]
    fn a_branch_switch_in_the_main_worktree_is_noticed() {
        let repo = init_repo();
        git(repo.path(), &["branch", "other"]);

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        git(repo.path(), &["checkout", "other"]);

        assert!(
            wait_until_dirty(&dirty),
            "a real branch switch in the main worktree must rewrite its HEAD and be observed"
        );
    }

    #[test]
    fn a_branch_switch_in_a_linked_worktree_is_noticed() {
        let repo = init_repo();
        let container = TempDir::new().expect("tempdir");
        let linked_path = container.path().join("switch-wt");
        drop(container);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "switch-feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );
        git(&linked_path, &["branch", "switch-target"]);

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo.path(), dirty.clone()).expect("spawn_worktree_watcher");

        git(&linked_path, &["checkout", "switch-target"]);

        assert!(
            wait_until_dirty(&dirty),
            "a real branch switch inside a linked worktree must be observed"
        );
    }

    #[test]
    fn a_non_repository_yields_no_watcher_rather_than_panicking() {
        let dir = TempDir::new().expect("tempdir");
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        assert!(spawn_worktree_watcher(dir.path(), dirty).is_none());
    }

    #[test]
    fn the_very_first_worktree_add_in_a_repo_is_still_noticed_via_the_fallback_watch() {
        let repo = init_repo();
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

        let container = TempDir::new().expect("tempdir");
        let linked_path = container.path().join("first-ever-wt");
        drop(container);
        git(
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
