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

    let worktrees_dir = common_dir.join("worktrees");
    let filter_root = worktrees_dir.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        if worktree_list_neutral_churn(&event, &filter_root) {
            return;
        }
        dirty.store(true, Ordering::SeqCst);
    })
    .ok()?;

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

/// True when the event is churn that can never change `git worktree list` output — most of it
/// produced by this app itself (GitHub issue #466). Without this filter each status-poll tick
/// re-dirties the watcher through its own diff spawns, and the refresh loop runs a
/// `git worktree list` at its fast ~500ms cadence forever instead of only on real changes plus
/// the 5s poll. Neutral churn is:
///
/// - by *class*: every `Access` event — reads (Linux's inotify delivers one per file *open*,
///   including git's own reads of `HEAD`/`config` on each invocation; no other backend emits
///   them at all);
/// - by *name*: the [`wt_core::diff::SHADOW_INDEX_PREFIX`] tempfiles every diff computation
///   writes beside the real index (git's `<name>.lock` sidecar shares the prefix), the real
///   `index`/`index.lock`, which `git status` opportunistically rewrites on the 3s status
///   poll, and the `objects` directory entry, whose mtime the same diffs bump by writing
///   loose objects;
/// - by *shape*: a non-rename `Modify` event on a worktree's bare admin-directory entry (a
///   direct child of `worktrees/`), which is only ever the directory's mtime rippling from
///   writes inside it. Real transitions are never lost to this arm: add/remove/lock all write
///   or delete files *inside* that directory (`HEAD`, `gitdir`, `locked`, …), whose own events
///   don't match it, and a `Create`/`Remove` of the entry itself still counts. Renames are
///   excluded even though every backend delivers them as `Modify(Name(..))`: a directory
///   moved wholesale into or out of `worktrees/` is a real transition whose *only* event is
///   that rename — and an mtime ripple is never a rename, so excluding them costs nothing.
///
/// A pathless event (a rescan notice) still counts as a real change.
fn worktree_list_neutral_churn(event: &notify::Event, worktrees_dir: &Path) -> bool {
    // Reads first: Linux's inotify backend subscribes `IN_OPEN` (notify-8.2.0/src/inotify.rs's
    // watch mask), so every git invocation that merely *opens* `HEAD` or `config` under a watch
    // surfaces as an `Access` event - the status poll's own reads would re-dirty the flag
    // forever (this is how the #466 loop survived on Linux while Windows, whose backend emits
    // no Access events, looked fixed). A real change always also emits Modify/Create/Remove/
    // rename, so dropping the whole Access class loses nothing.
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return true;
    }
    let dir_entry_mtime_bump = matches!(
        &event.kind,
        notify::EventKind::Modify(kind)
            if !matches!(kind, notify::event::ModifyKind::Name(_))
    );
    !event.paths.is_empty()
        && event.paths.iter().all(|path| {
            let neutral_name =
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(wt_core::diff::SHADOW_INDEX_PREFIX)
                            || name == "index"
                            || name == "index.lock"
                            || name == "objects"
                    });
            neutral_name || (dir_entry_mtime_bump && path.parent() == Some(worktrees_dir))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
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

    /// Arms the real production watcher plus a second collector mirroring its exact watch
    /// registrations, runs a real diff of `diff_target`, and asserts the dirty flag stays
    /// clean — printing every event the production filter did NOT drop on failure, so a
    /// platform-specific escape names itself in CI instead of leaving the assertion opaque
    /// (Linux's inotify `Access(Open)` storm was found exactly this way; GitHub issue #466).
    fn assert_diff_leaves_watcher_clean(repo_path: &Path, diff_target: &Path) {
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        let _watcher =
            spawn_worktree_watcher(repo_path, dirty.clone()).expect("spawn_worktree_watcher");

        let escaped: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let common_dir = wt_core::git_common_dir(repo_path).expect("git_common_dir");
        let worktrees_dir = common_dir.join("worktrees");
        let filter_root = worktrees_dir.clone();
        let sink = escaped.clone();
        let mut collector =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                if !worktree_list_neutral_churn(&event, &filter_root) {
                    sink.lock()
                        .expect("collector lock")
                        .push(format!("{:?} {:?}", event.kind, event.paths));
                }
            })
            .expect("collector watcher");
        if worktrees_dir.is_dir() {
            collector
                .watch(&worktrees_dir, RecursiveMode::Recursive)
                .expect("watch worktrees dir");
        } else {
            collector
                .watch(&common_dir, RecursiveMode::NonRecursive)
                .expect("watch common dir");
        }
        let head_file = common_dir.join("HEAD");
        if head_file.is_file() {
            collector
                .watch(&head_file, RecursiveMode::NonRecursive)
                .expect("watch HEAD");
        }

        wt_core::diff::diff_against_head(diff_target).expect("diff_against_head");

        assert!(
            test_support::stays_false(Duration::from_millis(500), || dirty.load(Ordering::SeqCst)),
            "index churn must never mark the worktree list dirty (GitHub issue #466); \
             events the filter let through: {:#?}",
            escaped.lock().expect("collector lock")
        );
    }

    /// One [`notify::Event`] with `kind` and `paths`, the two fields the churn filter reads.
    fn event_with(kind: notify::EventKind, paths: &[PathBuf]) -> notify::Event {
        let mut event = notify::Event::new(kind);
        for path in paths {
            event = event.add_path(path.clone());
        }
        event
    }

    #[test]
    fn the_churn_filter_recognizes_index_object_and_admin_dir_churn_and_nothing_else() {
        use notify::event::{CreateKind, ModifyKind};
        use notify::EventKind;

        let worktrees_dir = PathBuf::from(".git").join("worktrees");
        let modify = EventKind::Modify(ModifyKind::Any);
        let shadow = PathBuf::from(".git").join(".jerry-shadow-index-abc123");
        let shadow_lock = PathBuf::from(".git").join(".jerry-shadow-index-abc123.lock");
        let index = worktrees_dir.join("wt").join("index");
        let index_lock = worktrees_dir.join("wt").join("index.lock");
        let objects = PathBuf::from(".git").join("objects");
        let admin_dir = worktrees_dir.join("wt");
        let head = PathBuf::from(".git").join("HEAD");

        let neutral = event_with(
            modify,
            &[shadow.clone(), shadow_lock, index, index_lock, objects],
        );
        assert!(worktree_list_neutral_churn(&neutral, &worktrees_dir));
        assert!(
            worktree_list_neutral_churn(
                &event_with(modify, std::slice::from_ref(&admin_dir)),
                &worktrees_dir
            ),
            "a bare mtime bump on a worktree's admin dir is churn from writes inside it"
        );
        assert!(
            !worktree_list_neutral_churn(
                &event_with(
                    EventKind::Create(CreateKind::Folder),
                    std::slice::from_ref(&admin_dir)
                ),
                &worktrees_dir
            ),
            "creating a worktree admin dir is a real `git worktree add`, never filtered"
        );
        assert!(
            !worktree_list_neutral_churn(
                &event_with(
                    EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any)),
                    std::slice::from_ref(&admin_dir)
                ),
                &worktrees_dir
            ),
            "an admin dir moved into/out of worktrees/ arrives only as this rename, never filtered"
        );
        assert!(
            !worktree_list_neutral_churn(
                &event_with(modify, std::slice::from_ref(&head)),
                &worktrees_dir
            ),
            "HEAD is a real signal, never filtered"
        );
        assert!(
            worktree_list_neutral_churn(
                &event_with(
                    EventKind::Access(notify::event::AccessKind::Open(
                        notify::event::AccessMode::Any
                    )),
                    std::slice::from_ref(&head)
                ),
                &worktrees_dir
            ),
            "a mere read of HEAD (inotify delivers one per git invocation on Linux) is neutral"
        );
        assert!(
            !worktree_list_neutral_churn(&event_with(modify, &[shadow, head]), &worktrees_dir),
            "an event mixing churn with a real path must still count as a real change"
        );
        assert!(
            !worktree_list_neutral_churn(&event_with(modify, &[]), &worktrees_dir),
            "a pathless rescan notice must still count as a real change"
        );
    }

    /// GitHub issue #466, the exact feedback loop: every diff computation writes
    /// `.jerry-shadow-index-*` tempfiles beside the real index — inside the watched git
    /// directory — and used to re-trigger this watcher on the app's own churn, holding the
    /// refresh loop at its fast ~500ms cadence forever.
    #[test]
    fn a_real_diff_computation_does_not_re_trigger_the_watcher() {
        let repo = crate::test_support::temp_repo();
        std::fs::write(repo.path().join("untracked.txt"), "new content")
            .expect("write an untracked file so the diff has real shadow-index work to do");

        assert_diff_leaves_watcher_clean(repo.path(), repo.path());
    }

    /// The linked-worktree flavour of the loop above: with `worktrees/` present the watch is
    /// recursive over it, and the linked worktree's shadow index lives exactly there.
    #[test]
    fn a_real_diff_in_a_linked_worktree_does_not_re_trigger_the_watcher() {
        let repo = crate::test_support::temp_repo();
        let container = crate::test_support::temp_root();
        let linked_path = container.path().join("diffed-wt");
        drop(container);
        test_support::git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "diffed-feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );
        std::fs::write(linked_path.join("untracked.txt"), "new content")
            .expect("write an untracked file so the diff has real shadow-index work to do");

        assert_diff_leaves_watcher_clean(repo.path(), &linked_path);
    }

    #[test]
    fn a_non_repository_yields_no_watcher_rather_than_panicking() {
        let dir = crate::test_support::temp_root();
        let dirty: DirtyFlag = Arc::new(AtomicBool::new(false));
        assert!(spawn_worktree_watcher(dir.path(), dirty).is_none());
    }

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
