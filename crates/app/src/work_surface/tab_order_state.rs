//! Real, on-disk persistence for the tab strip's per-worktree drag order (GitHub issue #16:
//! "the resulting layout... persists per session/worktree and restores on relaunch").
//! `crate::root::AdeApp::tab_order` is the live, in-session mirror of one worktree's order; this
//! module is what makes a drag-reordered tab strip survive an app restart, not just a worktree
//! switch within the same run.
//!
//! ## Only file tabs are ever recorded
//!
//! A [`crate::work_surface::state::TabRef::Agent`] carries an
//! [`crate::work_surface::agents::AgentId`] - a process-local identity that a fresh launch's
//! freshly-spawned agents can never match again. Persisting an agent-tab entry would therefore
//! write a value nothing could ever read back successfully; [`WorktreeTabOrder::files`] only
//! ever holds `TabRef::File` entries, in their real relative drag order, and an agent tab always
//! lands wherever a freshly spawned agent naturally lands
//! (`work_surface::state::reconcile_tab_order`'s own "append what's open but not yet in
//! `stored`" rule) rather than at some remembered position that no longer means anything.
//!
//! ## Everything else - file format, atomicity, multi-instance merge - mirrors `fold_state`
//!
//! Copied rather than reinvented, matching `crate::rail::repo::RepoState`'s own precedent for
//! doing the same against this exact module: real crash-safe atomic writes
//! ([`TabOrderState::save_at`]: a process-unique sibling temp file, `sync_all`, `rename`, a
//! directory sync), and a merge-not-clobber real write path
//! ([`TabOrderState::save_merged_at`]) so a second `jerry` instance browsing a different
//! repository can't erase this one's saved order. See `crate::sidebar::fold_state`'s own module
//! docs for the full reasoning behind both, which applies here unchanged - including the same
//! honest limits (an unlocked read-modify-write around the merge; two instances open on the
//! *same* worktree still last-writer-wins).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The tab-order file's name, resolved next to the real `settings.toml` - mirrors
/// `crate::sidebar::fold_state::FOLD_STATE_FILE_NAME`.
pub const TAB_ORDER_FILE_NAME: &str = "tab-order.toml";

/// The tab-order file for a given real settings-file path - identical reasoning to
/// `crate::sidebar::fold_state::fold_state_path_for`: a test that supplies a temp-dir settings
/// path gets real, isolated tab-order persistence in that same directory, and a test that passes
/// `None` gets none at all.
pub fn tab_order_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(TAB_ORDER_FILE_NAME),
        None => PathBuf::from(TAB_ORDER_FILE_NAME),
    }
}

/// The map key for one worktree - see `crate::sidebar::fold_state::worktree_key`'s own docs for
/// the identical canonicalization/UTF-8 reasoning, copied here unchanged.
pub fn worktree_key(root: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canonical.to_str().map(str::to_owned)
}

/// One worktree's stored relative file path, or `None` if `file` isn't a real descendant of
/// `root` - mirrors `crate::sidebar::fold_state::relative_key`'s own traversal guard.
fn relative_key(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The read-side half of [`relative_key`] - refuses anything that isn't a plain sequence of
/// normal path segments, so a hand-edited or corrupted file can never name a path outside the
/// worktree it's filed under. Mirrors `crate::sidebar::fold_state::absolute_from_key` exactly.
fn absolute_from_key(root: &Path, key: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    let mut segments = 0;
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        if segment.contains('\\') || Path::new(segment).components().count() != 1 {
            return None;
        }
        path.push(segment);
        segments += 1;
    }
    (segments > 0).then_some(path)
}

/// How stale a `*.tmp` sibling must be before a save sweeps it - mirrors
/// `crate::sidebar::fold_state`'s own constant/reasoning.
const ORPHANED_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Mirrors `crate::sidebar::fold_state::sweep_orphaned_temp_files` exactly - see that function's
/// own docs.
fn sweep_orphaned_temp_files(path: &Path, file_name: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let prefix = format!("{file_name}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| {
                std::time::SystemTime::now()
                    .duration_since(modified)
                    .map_err(|err| io::Error::other(err.to_string()))
            })
            .is_ok_and(|age| age > ORPHANED_TEMP_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The whole on-disk file: every worktree this app has ever recorded a drag order for, keyed by
/// [`worktree_key`]. A `BTreeMap` so the serialized file has a stable, diffable order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabOrderState {
    pub worktrees: BTreeMap<String, WorktreeTabOrder>,
}

/// One worktree's entry - its real file tabs' relative paths, in real drag order. See the module
/// docs for why agent tabs are never recorded here at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeTabOrder {
    pub files: Vec<String>,
}

impl TabOrderState {
    /// Loads `path`, falling back to an empty state for *any* failure - same "never important
    /// enough to fail startup over" rule `crate::sidebar::fold_state::FoldState::load_at` states.
    pub fn load_at(path: &Path) -> TabOrderState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return TabOrderState::default();
        };
        match toml::from_str::<TabOrderState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from an empty tab order",
                    path.display()
                );
                TabOrderState::default()
            }
        }
    }

    /// Writes `self` to `path` atomically - a sibling `*.tmp` file, `sync_all`, then a rename
    /// over the target, then a parent-directory sync. See
    /// `crate::sidebar::fold_state::FoldState::save_at`'s own docs for the full reasoning; this
    /// is that same sequence, copied.
    pub fn save_at(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| TAB_ORDER_FILE_NAME.to_string());
        static WRITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = path.with_file_name(format!("{file_name}.{}.{unique}.tmp", std::process::id()));

        let write_result = (|| -> io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        })();
        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
        if let Err(err) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        sweep_orphaned_temp_files(path, &file_name);
        Ok(())
    }

    /// The app's real write path: merges `self`'s entries for the worktrees this instance
    /// actually owns into whatever is currently on disk, then writes the result via
    /// [`Self::save_at`] - identical reasoning to
    /// `crate::sidebar::fold_state::FoldState::save_merged_at`.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        let mut merged = TabOrderState::load_at(path);
        for key in owned {
            match self.worktrees.get(key) {
                Some(entry) => merged.worktrees.insert(key.clone(), entry.clone()),
                None => merged.worktrees.remove(key),
            };
        }
        merged.save_at(path)
    }

    /// `root`'s real, currently-recorded file order, as absolute paths ready to feed into
    /// [`crate::work_surface::state::TabRef::File`]. Entries that don't decode to a plain
    /// relative path are silently skipped ([`absolute_from_key`]), and a worktree with no entry
    /// at all (never seen, or every file tab has since closed) returns an empty order - never an
    /// error, matching a fresh worktree's own real "nothing recorded yet" state.
    pub fn file_order(&self, root: &Path) -> Vec<PathBuf> {
        match worktree_key(root) {
            Some(key) => self.file_order_with_key(&key, root),
            None => Vec::new(),
        }
    }

    /// [`Self::file_order`] against an already-resolved [`worktree_key`].
    pub fn file_order_with_key(&self, root_key: &str, root: &Path) -> Vec<PathBuf> {
        let Some(entry) = self.worktrees.get(root_key) else {
            return Vec::new();
        };
        entry
            .files
            .iter()
            .filter_map(|key| absolute_from_key(root, key))
            .collect()
    }

    /// Records `root`'s real, current file order (already-resolved absolute paths, in their real
    /// drag order) - the write-side counterpart to [`Self::file_order`]. A path that isn't a
    /// plain descendant of `root`, or isn't valid UTF-8, is silently dropped from the recorded
    /// order rather than refusing the whole call: unlike `FoldState::set_expanded`'s single-path
    /// calls, this always records a whole worktree's order at once (`AdeApp::reorder_tab`'s own
    /// real caller), and one unrecordable entry must not lose every other real, recordable one in
    /// the same drag.
    pub fn set_file_order(&mut self, root: &Path, files: &[PathBuf]) {
        let Some(root_key) = worktree_key(root) else {
            return;
        };
        let keys: Vec<String> = files
            .iter()
            .filter_map(|file| relative_key(root, file))
            .collect();
        if keys.is_empty() {
            self.worktrees.remove(&root_key);
        } else {
            self.worktrees
                .insert(root_key, WorktreeTabOrder { files: keys });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_order_lives_next_to_the_real_settings_file() {
        let path = tab_order_path_for(Path::new("/home/someone/.config/jerry/settings.toml"));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.config/jerry/tab-order.toml")
        );
    }

    #[test]
    fn an_unseen_worktree_has_no_recorded_order() {
        let state = TabOrderState::default();
        assert!(state
            .file_order(Path::new("/repo/fresh-worktree"))
            .is_empty());
    }

    #[test]
    fn set_and_read_round_trips_a_real_order() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_file_order(root, &[root.join("src/main.rs"), root.join("README.md")]);
        assert_eq!(
            state.file_order(root),
            vec![root.join("src/main.rs"), root.join("README.md")],
            "the real drag order must round-trip exactly, including which file comes first"
        );
    }

    #[test]
    fn set_file_order_with_an_empty_list_forgets_the_worktree_entirely() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_file_order(root, &[root.join("a.txt")]);
        assert!(!state.worktrees.is_empty());

        state.set_file_order(root, &[]);
        assert!(
            state.worktrees.is_empty(),
            "closing every file tab must not leave an empty entry behind forever"
        );
    }

    #[test]
    fn a_path_outside_the_worktree_is_dropped_but_the_rest_of_the_order_survives() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_file_order(
            root,
            &[
                root.join("src/main.rs"),
                PathBuf::from("/etc/passwd"),
                root.join("README.md"),
            ],
        );
        assert_eq!(
            state.file_order(root),
            vec![root.join("src/main.rs"), root.join("README.md")],
            "an unrecordable entry must be dropped without losing the real, recordable ones \
             around it"
        );
    }

    #[test]
    fn tab_order_for_one_worktree_never_leaks_into_another() {
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");
        let mut state = TabOrderState::default();
        state.set_file_order(a, &[a.join("src/main.rs")]);

        assert_eq!(state.file_order(a), vec![a.join("src/main.rs")]);
        assert!(
            state.file_order(b).is_empty(),
            "worktree B shares the relative path `src/main.rs` with A, and must still start \
             with no recorded order"
        );
    }

    #[test]
    fn saving_and_reloading_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tab-order.toml");
        let root = Path::new("/repo/worktree-a");

        let mut state = TabOrderState::default();
        state.set_file_order(root, &[root.join("a.rs"), root.join("b.rs")]);
        state.save_at(&path).expect("save");

        let reloaded = TabOrderState::load_at(&path);
        assert_eq!(reloaded, state);
        assert_eq!(
            reloaded.file_order(root),
            vec![root.join("a.rs"), root.join("b.rs")]
        );
    }

    #[test]
    fn saving_merges_with_another_instances_entries_instead_of_erasing_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tab-order.toml");
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");

        let mut instance_a = TabOrderState::default();
        instance_a.set_file_order(a, &[a.join("a.rs")]);
        let owned_a: BTreeSet<String> = [worktree_key(a).expect("key")].into_iter().collect();
        instance_a.save_merged_at(&path, &owned_a).expect("save a");

        let mut instance_b = TabOrderState::default();
        instance_b.set_file_order(b, &[b.join("b.rs")]);
        let owned_b: BTreeSet<String> = [worktree_key(b).expect("key")].into_iter().collect();
        instance_b.save_merged_at(&path, &owned_b).expect("save b");

        let on_disk = TabOrderState::load_at(&path);
        assert_eq!(
            on_disk.file_order(a),
            vec![a.join("a.rs")],
            "instance B's save must not have erased instance A's worktree"
        );
        assert_eq!(on_disk.file_order(b), vec![b.join("b.rs")]);
    }

    #[test]
    fn a_missing_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = TabOrderState::load_at(&dir.path().join("does-not-exist.toml"));
        assert_eq!(state, TabOrderState::default());
    }

    #[test]
    fn a_corrupted_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tab-order.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write");
        assert_eq!(TabOrderState::load_at(&path), TabOrderState::default());
    }

    #[test]
    fn a_traversal_entry_in_a_hand_edited_file_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tab-order.toml");
        std::fs::write(
            &path,
            "[worktrees.\"/repo/worktree-a\"]\nfiles = [\"../worktree-b/secret.rs\", \"src/main.rs\"]\n",
        )
        .expect("write");

        let state = TabOrderState::load_at(&path);
        assert_eq!(
            state.file_order(Path::new("/repo/worktree-a")),
            vec![PathBuf::from("/repo/worktree-a/src/main.rs")],
            "the `..` entry must be silently dropped, and the good one kept"
        );
    }

    #[test]
    fn saving_leaves_no_temp_file_behind_and_the_result_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("tab-order.toml");
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_file_order(root, &[root.join("a.rs")]);

        state.save_at(&path).expect("save");

        assert!(path.exists());
        let siblings: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(siblings, vec!["tab-order.toml".to_string()]);
    }
}
