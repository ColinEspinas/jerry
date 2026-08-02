//! Real OS-level filesystem watching that backs the Files tab's live refresh (GitHub issue #13:
//! "the file list is currently a snapshot taken at load time... drifts out of sync with the
//! actual state on disk"). The same split `crate::rail::worktree_watch` already established for
//! the worktree list: this module only ever sets a flag from a real OS watcher callback; the
//! `gpui`-side poll loop that reads it and actually reloads the tree is
//! `crate::root::AdeApp::start_file_tree_watch`.
//!
//! ## One watch, re-armed per worktree
//!
//! Unlike `worktree_watch` (set up once, for the repository's own `$GIT_COMMON_DIR`, which never
//! changes over the app's lifetime), the *active worktree* changes constantly - so this watch is
//! re-armed every time [`crate::root::AdeApp::set_file_tree_root`] points at a new root, watching
//! exactly the worktree currently shown rather than every worktree the repo has.
//!
//! ## `.git/` is filtered out, not merely unwatched
//!
//! A recursive `notify` watch has no path-exclusion option of its own - filtering happens in the
//! event callback, not the watch registration itself. Without it, git's own constant internal
//! writes under `<root>/.git/` (the index, `HEAD`, loose objects, `refs/`) would mark the file
//! tree dirty on every commit, stage, and status check `wt_core` itself performs - exactly the
//! "recursive watch fires on every object-database write" noise
//! [`crate::rail::worktree_watch::spawn_worktree_watcher`]'s own docs already call out avoiding
//! for its own, separate `.git`-only watch.
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::rail::worktree_watch::DirtyFlag;

/// Starts a real filesystem watcher rooted at `worktree_root` (the Files tab's current root -
/// see the module docs on why this is re-armed per worktree rather than set up once) and returns
/// the live [`RecommendedWatcher`] - as with `worktree_watch::spawn_worktree_watcher`, it must be
/// kept alive by the caller for the OS-level watch to keep running.
///
/// Returns `None` if `worktree_root` doesn't exist, isn't really part of a git worktree, or if
/// the underlying OS watcher couldn't be constructed or armed - all honestly-reported "there is
/// nothing to watch" cases, not fatal: `crate::root::AdeApp::start_file_tree_watch`'s own poll
/// fallback still runs regardless. A very large tree (e.g. an unignored `node_modules`) can also
/// exhaust the OS's real watch-descriptor budget (inotify's `max_user_watches` on Linux) - the
/// same honest degrade applies there too, not a special case.
///
/// The `wt_core::git_common_dir` check below is a real gate, not just an optimization: every
/// real [`crate::root::AdeApp::file_tree_root`] this app ever sets is a real git worktree's path
/// (production has no other kind), so a directory that fails it genuinely has nothing this app
/// would ever watch in real use - mirroring
/// [`crate::rail::worktree_watch::spawn_worktree_watcher`]'s own identical gate. It matters
/// operationally too: without it, every test that constructs an `AdeApp` against a plain,
/// non-git temp directory (the overwhelming majority of this crate's own GPUI tests) would arm a
/// real OS-level `notify` watcher it never needed, and a real, reproduced regression from an
/// earlier version of this function did exactly that - exhausting the sandbox's `inotify`
/// instance budget partway through a full `cargo test` run and taking
/// `crate::rail::worktree_watch`'s own, unrelated tests down with it.
///
/// Performs blocking I/O (`git_common_dir`'s own `git rev-parse`, and the initial recursive
/// directory walk `notify` does to arm the watch).
pub fn spawn_file_tree_watcher(
    worktree_root: &Path,
    dirty: DirtyFlag,
) -> Option<RecommendedWatcher> {
    if !worktree_root.is_dir() {
        return None;
    }
    wt_core::git_common_dir(worktree_root).ok()?;
    let git_dir: PathBuf = worktree_root.join(".git");

    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        // Only a change with at least one real path outside `.git/` counts - see the module docs
        // on why `.git/`'s own churn must never mark the tree dirty.
        let outside_git = event.paths.iter().any(|path| !path.starts_with(&git_dir));
        if outside_git {
            dirty.store(true, Ordering::SeqCst);
        }
    })
    .ok()?;

    watcher
        .watch(worktree_root, RecursiveMode::Recursive)
        .ok()?;
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
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
            "git {:?} failed in {:?}",
            args,
            dir
        );
    }

    /// A real git repository - see [`spawn_file_tree_watcher`]'s own docs on why its
    /// `wt_core::git_common_dir` gate means every one of this module's tests needs a real repo,
    /// not just a plain temp directory.
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

    /// Real-time bounded wait for the watcher's async, OS-thread-delivered callback to fire - see
    /// `crate::rail::worktree_watch::tests::wait_until_dirty`'s identical own docs for why this
    /// can't be a simulated/deterministic clock.
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

    /// The inverse of [`wait_until_dirty`]: asserts the flag stays clear for a real, bounded
    /// window - used to prove `.git/` churn is genuinely filtered out, not just "usually" missed
    /// by a race.
    fn assert_stays_clean(dirty: &DirtyFlag) {
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !dirty.load(Ordering::SeqCst),
            "a change confined to .git/ must never mark the file tree dirty"
        );
    }

    #[test]
    fn a_real_new_file_is_noticed() {
        let dir = init_repo();
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_file_tree_watcher(dir.path(), dirty.clone()).expect("spawn_file_tree_watcher");

        fs::write(dir.path().join("new.txt"), "content").expect("write");

        assert!(
            wait_until_dirty(&dirty),
            "creating a real file must be observed within the real-time budget"
        );
    }

    #[test]
    fn a_real_file_edit_in_a_nested_directory_is_noticed() {
        let dir = init_repo();
        fs::create_dir_all(dir.path().join("src/nested")).expect("mkdir");
        let file = dir.path().join("src/nested/a.rs");
        fs::write(&file, "fn main() {}").expect("write");

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_file_tree_watcher(dir.path(), dirty.clone()).expect("spawn_file_tree_watcher");

        fs::write(&file, "fn main() { println!(\"hi\"); }").expect("write");

        assert!(
            wait_until_dirty(&dirty),
            "editing a real file nested several directories deep must be observed"
        );
    }

    #[test]
    fn a_real_deletion_is_noticed() {
        let dir = init_repo();
        let file = dir.path().join("gone.txt");
        fs::write(&file, "content").expect("write");

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_file_tree_watcher(dir.path(), dirty.clone()).expect("spawn_file_tree_watcher");

        fs::remove_file(&file).expect("remove");

        assert!(
            wait_until_dirty(&dirty),
            "deleting a real file must be observed"
        );
    }

    #[test]
    fn a_change_confined_to_dot_git_is_filtered_out() {
        let dir = init_repo();

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_file_tree_watcher(dir.path(), dirty.clone()).expect("spawn_file_tree_watcher");

        // Real churn genuinely confined to `.git/` alone - unlike `git checkout` (which can
        // legitimately touch tracked working-tree files' own mtimes even when their content is
        // unchanged, a real git behavior this test must not mistake for a filter bug), `git
        // update-ref` only ever writes inside `.git/` itself.
        git(dir.path(), &["update-ref", "refs/heads/other", "HEAD"]);

        assert_stays_clean(&dirty);
    }

    #[test]
    fn a_real_change_alongside_dot_git_churn_is_still_noticed() {
        let dir = init_repo();

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_file_tree_watcher(dir.path(), dirty.clone()).expect("spawn_file_tree_watcher");

        git(dir.path(), &["update-ref", "refs/heads/other", "HEAD"]);
        fs::write(dir.path().join("real.txt"), "real content").expect("write");

        assert!(
            wait_until_dirty(&dirty),
            "a real change outside .git/ must still be observed even alongside .git/ churn"
        );
    }

    #[test]
    fn a_missing_directory_yields_no_watcher_rather_than_panicking() {
        let dir = init_repo();
        let missing = dir.path().join("does-not-exist");
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        assert!(spawn_file_tree_watcher(&missing, dirty).is_none());
    }

    #[test]
    fn a_non_git_directory_yields_no_watcher() {
        let dir = TempDir::new().expect("tempdir");
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        assert!(
            spawn_file_tree_watcher(dir.path(), dirty).is_none(),
            "a plain, non-git directory must never arm a real watcher - see \
             `spawn_file_tree_watcher`'s own docs on why this gate is real, not incidental"
        );
    }
}
