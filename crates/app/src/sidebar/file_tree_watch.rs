//! Real OS-level filesystem watching that backs the Files tab's live refresh (GitHub issue #13:
//! "the file list is currently a snapshot taken at load time... drifts out of sync with the
//! actual state on disk"). The same split `crate::rail::worktree_watch` already established for
//! the worktree list: this module only ever sets a flag from a real OS watcher callback; the
//! `gpui`-side poll loop that reads it and actually reloads the tree is
//! `crate::root::AdeApp::start_file_tree_watch`.
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::rail::worktree_watch::DirtyFlag;

/// Starts a real filesystem watcher rooted at `worktree_root` (the Files tab's current root -
/// see the module docs on why this is re-armed per worktree rather than set up once) and returns
/// the live [`RecommendedWatcher`] - as with `worktree_watch::spawn_worktree_watcher`, it must be
/// kept alive by the caller for the OS-level watch to keep running.
pub fn spawn_file_tree_watcher(
    worktree_root: &Path,
    dirty: DirtyFlag,
) -> Option<RecommendedWatcher> {
    if !worktree_root.is_dir() {
        return None;
    }
    wt_core::git_common_dir(worktree_root).ok()?;
    // The OS reports every event path resolved (macOS `FSEvents` hands back `/private/var/...` for
    // a watch armed on `/var/...`), so a `.git` prefix built from an unresolved root would match
    // nothing and every one of git's own internal writes would mark the tree dirty. `dunce`
    // rather than `std::fs` so the prefix carries no Windows verbatim `\\?\` spelling — the same
    // form `canonical_repo_path` keys the app by (GitHub issue #467).
    let resolved =
        dunce::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    let git_dir: PathBuf = resolved.join(".git");

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
mod file_tree_watcher_tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use test_support::{git, seed_repo, stays_false, wait_until};

    /// The watcher's callback is delivered by an OS thread, not by a GPUI executor, so there is
    /// no deterministic clock to park - a real-time bounded wait is the only option.
    fn wait_until_dirty(dirty: &DirtyFlag) -> bool {
        wait_until(Duration::from_secs(3), || {
            dirty.swap(false, Ordering::SeqCst)
        })
    }

    /// The inverse of [`wait_until_dirty`]: proves `.git/` churn is genuinely filtered out
    /// rather than just slow to arrive.
    fn assert_stays_clean(dirty: &DirtyFlag) {
        assert!(
            stays_false(Duration::from_millis(500), || dirty.load(Ordering::SeqCst)),
            "a change confined to .git/ must never mark the file tree dirty"
        );
    }

    /// Creation, a nested edit and a deletion are the three real working-tree changes the Files
    /// tab has to notice, and one recursive watch is what notices all three - so they are one
    /// test over one watcher rather than three that differ only in which `fs` call they make.
    #[test]
    fn every_real_working_tree_change_marks_the_tree_dirty() {
        let dir = seed_repo();
        fs::create_dir_all(dir.path().join("src/nested")).expect("mkdir");
        let nested = dir.path().join("src/nested/a.rs");
        fs::write(&nested, "fn main() {}").expect("write");

        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_file_tree_watcher(dir.path(), dirty.clone()).expect("spawn_file_tree_watcher");

        let created = dir.path().join("new.txt");
        fs::write(&created, "content").expect("write");
        assert!(wait_until_dirty(&dirty), "a created file must be observed");

        fs::write(&nested, "fn main() { println!(\"hi\"); }").expect("write");
        assert!(
            wait_until_dirty(&dirty),
            "an edit several directories deep must be observed - the watch is recursive"
        );

        fs::remove_file(&created).expect("remove");
        assert!(wait_until_dirty(&dirty), "a deletion must be observed");
    }

    #[test]
    fn a_change_confined_to_dot_git_is_filtered_out() {
        let dir = seed_repo();

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
        let dir = seed_repo();

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
        let dir = seed_repo();
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
