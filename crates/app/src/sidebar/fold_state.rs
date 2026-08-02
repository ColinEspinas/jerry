//! Real, on-disk persistence for the Files tree's expand/collapse state (GitHub issue #18).
//!
//! ## What is stored, and why it's *expanded* rather than *collapsed*
//!
//! The tree opens **fully collapsed** on the first visit to a worktree, so "expanded" is the
//! exceptional state and the one worth recording: an empty (or missing) entry for a worktree
//! means "nothing is expanded", which is exactly the right default for a worktree this file has
//! never seen - a freshly created worktree therefore inherits nothing and starts collapsed, with
//! no special case needed anywhere. `crate::root::AdeApp::expanded_dirs` is the live mirror of
//! one worktree's entry here.
//!
//! ## Why a separate file rather than a `[file_tree]` section in `settings.toml`
//!
//! `crate::settings::store`'s own module docs are explicit that every field of `Settings` is a
//! value some settings *page* reads and writes, and the config banner/snippet widgets render
//! those sections back to the user as hand-editable config. This is not that: it's machine-
//! managed UI state, potentially hundreds of paths across every worktree ever opened, that no
//! settings page shows and nobody would hand-edit. It lives in its own
//! `~/.config/jerry/file-tree-state.toml`, resolved as a sibling of the real settings path
//! ([`fold_state_path_for`]) so the two always share a directory and a test that supplies a
//! temp-dir settings path automatically gets a temp-dir fold-state path too.
//!
//! The second, load-bearing reason is crash-safety, which the issue asks for by name.
//! `Settings::save_at` is documented as a deliberately non-atomic truncate-then-write; a crash
//! mid-write there loses at worst a settings edit. [`FoldState::save_at`] instead writes a
//! process-unique sibling temp file, `File::sync_all`s it, `std::fs::rename`s it over the
//! target, and syncs the parent directory - so neither a killed process nor a power loss can
//! leave a half-written (and therefore unparseable) file behind. What survives a crash is
//! always either the previous complete state or the new complete state.
//!
//! ## Two running instances share this file, so writes merge rather than clobber
//!
//! One `jerry` process opens one window against one repository (`crate::run`), so running it
//! against two repositories at once means two processes writing this one file. The temp file's
//! name therefore includes the process id and a per-process counter (two writers can't scribble
//! over each other's temp file), and the app writes through [`FoldState::save_merged_at`], which
//! re-reads whatever is on disk and replaces only the worktree keys *this* instance actually
//! owns. Without that merge, the last process to save would silently erase every other
//! repository's fold state, since each holds a whole-file copy read at its own startup.
//!
//! Honest about the two things that does *not* buy, both real:
//!
//! 1. The merge is an unlocked read-modify-write, so two instances owning *different* worktrees
//!    whose saves genuinely interleave can still lose one update. That narrows the exposure from
//!    "every save clobbers every other repository" to "a few microseconds around each save".
//! 2. Two instances that own the **same** worktree key - the same worktree open twice - are not
//!    merged at all, and this is not a narrow window: each save replaces that key's whole entry
//!    with its own copy, so the two instances permanently revert each other's expansions for
//!    that worktree, last-writer-wins, for as long as both are running. Fixing it properly means
//!    a delta-based merge (record what changed, not what the whole entry now is), which is a
//!    materially bigger design than this feature justifies; the sibling-worktree case above is
//!    the one that actually happens (one `jerry` per repository), and it *is* fixed. This is
//!    written down rather than left to be discovered.
//!
//! ## Nothing is ever pruned because a path merely looks absent
//!
//! Stale entries are pruned against a real, completed directory walk
//! ([`FoldState::prune_missing_dirs`]) and nothing else. An earlier draft also dropped whole
//! worktrees at startup whose root `Path::exists()` reported gone; that was removed as
//! destructive-on-a-false-negative - an unmounted volume, or a parent directory that is briefly
//! unreadable, would have permanently deleted that worktree's state. The cost of not doing it is
//! a few hundred bytes per worktree that no longer exists.
//!
//! ## Identity: keyed by real worktree path, never by relative path alone
//!
//! Two worktrees of the same repository share every relative path in the tree, so a fold-state
//! entry keyed only by `src/app` would apply to both. The map is therefore keyed by the
//! worktree's real, canonicalized absolute path ([`worktree_key`]) with worktree-relative paths
//! stored *inside* that entry - the same "identity guard" discipline the rest of this codebase
//! documents. A path that doesn't sit under the worktree root at all is refused outright rather
//! than stored with a surprising key ([`relative_key`]).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The fold-state file's name, resolved next to the real `settings.toml` - see the module docs.
pub const FOLD_STATE_FILE_NAME: &str = "file-tree-state.toml";

/// The fold-state file for a given real settings-file path (`~/.config/jerry/settings.toml` in
/// production, a temp dir in tests). Deriving it from the settings path rather than resolving
/// `$HOME` a second time means a test that opts into a real settings path
/// (`crate::root::AdeApp::new_with_settings`) gets real fold-state persistence in the same temp
/// directory for free, and a test that passes `None` gets no persistence at all - never a write
/// to whatever machine happens to run `cargo test`.
pub fn fold_state_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(FOLD_STATE_FILE_NAME),
        None => PathBuf::from(FOLD_STATE_FILE_NAME),
    }
}

/// The map key for one worktree - its real path, canonicalized so the same worktree reached
/// through a symlink (or a `.`-relative invocation) resolves to one entry rather than two.
/// Falls back to the path as given when it can't be canonicalized (it may not exist yet, or be
/// a pure in-memory path in a unit test), which is still a stable key for that path.
///
/// **Calls `std::fs::canonicalize`, so it must never be called from a render or per-event
/// path.** On a stale or slow mount (NFS, FUSE, a briefly-disconnected network drive) that syscall can
/// block for the mount's full timeout, which on the foreground thread is a frozen window. Every
/// hot caller instead uses the `*_with_key` variants below against
/// `crate::root::AdeApp::fold_state_root_key`, which is resolved exactly once per worktree
/// change; this function is called only from those few real change points (and from tests).
///
/// `None` for a path that isn't valid UTF-8. TOML keys are strings, and the obvious shortcut -
/// `to_string_lossy` - would map every undecodable byte to the same U+FFFD, so two genuinely
/// different worktrees could collapse onto one key: the exact cross-worktree leak this module
/// exists to prevent. Refusing outright means such a worktree simply doesn't persist fold state
/// (and says so in the log, see `crate::root::AdeApp::set_dir_expanded`), which is a far smaller
/// failure than silently sharing another worktree's.
pub fn worktree_key(root: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canonical.to_str().map(str::to_owned)
}

/// One worktree's stored relative directory path, or `None` if `dir` isn't a real descendant of
/// `root`. `/`-joined `Component::Normal` segments only: a `..`, a drive prefix, or a root
/// component would let a stored entry escape the worktree it's filed under, which is exactly the
/// cross-worktree leak this whole module is keyed to prevent.
fn relative_key(root: &Path, dir: &Path) -> Option<String> {
    let relative = dir.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            // `to_str`, not `to_string_lossy` - see [`worktree_key`]'s own docs for why a
            // lossy conversion here would be an identity bug rather than a cosmetic one.
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        // The root itself is never a fold-state entry: it has no row to expand or collapse.
        return None;
    }
    Some(parts.join("/"))
}

/// Rebuilds an absolute path from a stored relative key, refusing anything that isn't a plain
/// sequence of normal path segments - the read-side half of [`relative_key`]'s guard, since the
/// file on disk is a real file a user (or a corrupted write) can put anything into.
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

/// How stale a `*.tmp` sibling must be before [`sweep_orphaned_temp_files`] deletes it. A real
/// in-flight temp file lives for microseconds; an hour is far beyond any plausible write, so
/// this can never race a live writer in another process.
const ORPHANED_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Deletes leftover `<file-name>.<pid>.<n>.tmp` siblings from a save that was killed between
/// creating its temp file and renaming it. Without this, making the temp name process-unique
/// (which is what stops two instances from tearing each other's writes) would turn one
/// reusable orphan into an unbounded pile of them, since a save runs on every single
/// expand/collapse. Entirely best-effort: every error is ignored, and a failure here has no
/// bearing on whether the save itself succeeded.
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

/// The whole on-disk file: every worktree this app has ever recorded fold state for, keyed by
/// [`worktree_key`]. A `BTreeMap`/`BTreeSet` (not a `HashMap`/`HashSet`) so the serialized file
/// has a stable, diffable order rather than reshuffling itself on every write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FoldState {
    pub worktrees: BTreeMap<String, WorktreeFoldState>,
}

/// One worktree's entry - the worktree-relative paths of every directory the user has expanded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeFoldState {
    pub expanded: BTreeSet<String>,
}

/// What [`FoldState::set_expanded`] did - three distinct outcomes, not a `bool`, because the
/// caller has to tell "already in that state, no write needed" apart from "refused, so the live
/// UI state and this file have genuinely diverged and somebody should hear about it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetExpanded {
    /// Recorded; the file needs writing.
    Changed,
    /// Already recorded that way; nothing to write.
    Unchanged,
    /// Not recordable at all - see [`FoldState::set_expanded`].
    Refused,
}

impl FoldState {
    /// Loads `path`, falling back to an empty state for *any* failure - missing file (the
    /// overwhelmingly common first-run case), unreadable file, or unparseable contents. Fold
    /// state is never important enough to fail startup or surface an error over; the issue asks
    /// for stale/bad entries to be "silently ignored, never an error", and this is the outermost
    /// case of that rule.
    pub fn load_at(path: &Path) -> FoldState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return FoldState::default();
        };
        match toml::from_str::<FoldState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from an empty file-tree fold state",
                    path.display()
                );
                FoldState::default()
            }
        }
    }

    /// Writes `self` to `path` **atomically**: a sibling `*.tmp` file first, then a
    /// `std::fs::rename` over the target. See the module docs for why this one is atomic where
    /// `Settings::save_at` deliberately isn't - "recorded immediately (crash-safe)" is a
    /// requirement here, and a truncate-then-write leaves a real window in which a crash
    /// produces a half-written, unparseable file. The temp file is created in the *same*
    /// directory so the rename never crosses a filesystem boundary (where it would fail).
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
            .unwrap_or_else(|| FOLD_STATE_FILE_NAME.to_string());
        // Process- *and* call-unique: two `jerry` instances saving at the same moment must not
        // write to the same temp path (they would interleave into one torn file that both then
        // rename into place), and neither must two saves from the same process.
        static WRITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = path.with_file_name(format!("{file_name}.{}.{unique}.tmp", std::process::id()));

        // `sync_all` before the rename, and a directory sync after it: a plain `write` + `rename`
        // is only atomic with respect to *metadata*, so a power loss can otherwise leave the
        // renamed inode pointing at unflushed (zero-length or truncated) data.
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
            // Best-effort: a failure to sync the directory entry costs at most this one save on
            // a power loss, and is not worth reporting the whole write as failed over.
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        sweep_orphaned_temp_files(path, &file_name);
        Ok(())
    }

    /// The app's real write path: merges `self`'s entries for the worktrees this instance
    /// actually owns into whatever is currently on disk, then writes the result via
    /// [`Self::save_at`]. See the module docs for why a plain whole-file write would silently
    /// erase a second running instance's state.
    ///
    /// `owned` is the set of [`worktree_key`]s this instance has recorded anything for. Keys in
    /// `owned` are taken from `self` (including *absence* - that's how "collapse all" deletes an
    /// entry); every other key on disk is passed through untouched.
    ///
    /// GitHub issue #90: wrapped in `crate::persisted_state_lock::with_locked_merge` - "New
    /// Window" made this load-merge-save cycle reachable *concurrently within one process*, not
    /// just across two separate `jerry` processes (which the `owned`-scoped merge above already
    /// handled) - see that module's own docs for the real race two truly concurrent callers could
    /// otherwise hit, and why one process-wide lock, shared with `crate::rail::repo::RepoState`/
    /// `crate::work_surface::tab_order_state::TabOrderState`'s own identical methods, is enough.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = FoldState::load_at(path);
            for key in owned {
                match self.worktrees.get(key) {
                    Some(entry) => merged.worktrees.insert(key.clone(), entry.clone()),
                    None => merged.worktrees.remove(key),
                };
            }
            merged.save_at(path)
        })
    }

    /// Every directory currently recorded as expanded for `root`, as absolute paths ready to be
    /// compared against `crate::sidebar::file_tree::FileTreeEntry::path`. Entries that don't
    /// decode to a plain relative path are silently skipped ([`absolute_from_key`]).
    pub fn expanded_dirs(&self, root: &Path) -> HashSet<PathBuf> {
        match worktree_key(root) {
            Some(key) => self.expanded_dirs_with_key(&key, root),
            None => HashSet::new(),
        }
    }

    /// [`Self::expanded_dirs`] against an already-resolved [`worktree_key`] - see that function's
    /// own docs for why every hot path takes this variant.
    pub fn expanded_dirs_with_key(&self, root_key: &str, root: &Path) -> HashSet<PathBuf> {
        let Some(entry) = self.worktrees.get(root_key) else {
            return HashSet::new();
        };
        entry
            .expanded
            .iter()
            .filter_map(|key| absolute_from_key(root, key))
            .collect()
    }

    /// Records `dir` as expanded (or not) under `root`. [`SetExpanded::Refused`] - rather than a
    /// silent no-op - for a `dir` that isn't a plain descendant of `root`, or for either path
    /// being non-UTF-8: those would have to be stored under a key they don't belong to, or under
    /// a lossily-mangled one that could collide with a different worktree's.
    pub fn set_expanded(&mut self, root: &Path, dir: &Path, expanded: bool) -> SetExpanded {
        match worktree_key(root) {
            Some(root_key) => self.set_expanded_with_key(&root_key, root, dir, expanded),
            None => SetExpanded::Refused,
        }
    }

    /// [`Self::set_expanded`] against an already-resolved [`worktree_key`] - the variant every
    /// real expand/collapse goes through, since [`worktree_key`] itself does blocking filesystem
    /// work. `root` is still needed to make `dir` relative, which is pure string work.
    pub fn set_expanded_with_key(
        &mut self,
        root_key: &str,
        root: &Path,
        dir: &Path,
        expanded: bool,
    ) -> SetExpanded {
        let Some(key) = relative_key(root, dir) else {
            return SetExpanded::Refused;
        };
        let root_key = root_key.to_string();
        let changed = if expanded {
            self.worktrees
                .entry(root_key)
                .or_default()
                .expanded
                .insert(key)
        } else {
            let Some(entry) = self.worktrees.get_mut(&root_key) else {
                return SetExpanded::Unchanged;
            };
            let removed = entry.expanded.remove(&key);
            if entry.expanded.is_empty() {
                // Don't leave an empty table behind - an entry with nothing expanded is
                // indistinguishable from no entry at all, and dropping it keeps the file from
                // accumulating one section per worktree ever browsed.
                self.worktrees.remove(&root_key);
            }
            removed
        };
        if changed {
            SetExpanded::Changed
        } else {
            SetExpanded::Unchanged
        }
    }

    /// Forgets everything recorded for `root` - the persisted half of the "collapse all" action,
    /// which the issue asks to reset "both the tree and the saved state in one step". Returns
    /// whether anything was actually removed.
    pub fn clear_worktree(&mut self, root: &Path) -> bool {
        worktree_key(root).is_some_and(|key| self.clear_worktree_with_key(&key))
    }

    /// [`Self::clear_worktree`] against an already-resolved [`worktree_key`].
    pub fn clear_worktree_with_key(&mut self, root_key: &str) -> bool {
        self.worktrees.remove(root_key).is_some()
    }

    /// Drops any recorded directory for `root` that isn't in `existing_dirs` - the "stale entries
    /// (folders since deleted or renamed) are silently ignored and pruned, never an error" half
    /// of the issue. Returns whether anything was pruned.
    ///
    /// Takes the real, currently-loaded directory set rather than doing its own `Path::exists`
    /// calls: the caller has just walked the tree, so this needs no syscalls at all, and - more
    /// importantly - it cannot prune an entry merely because a *slow or racing* filesystem check
    /// happened to miss it.
    pub fn prune_missing_dirs(&mut self, root: &Path, existing_dirs: &HashSet<PathBuf>) -> bool {
        match worktree_key(root) {
            Some(key) => self.prune_missing_dirs_with_key(&key, root, existing_dirs),
            None => false,
        }
    }

    /// [`Self::prune_missing_dirs`] against an already-resolved [`worktree_key`].
    pub fn prune_missing_dirs_with_key(
        &mut self,
        root_key: &str,
        root: &Path,
        existing_dirs: &HashSet<PathBuf>,
    ) -> bool {
        let worktree_key = root_key.to_string();
        let Some(entry) = self.worktrees.get_mut(&worktree_key) else {
            return false;
        };
        let before = entry.expanded.len();
        entry.expanded.retain(|key| {
            absolute_from_key(root, key).is_some_and(|path| existing_dirs.contains(&path))
        });
        let pruned = entry.expanded.len() != before;
        if entry.expanded.is_empty() {
            self.worktrees.remove(&worktree_key);
        }
        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded_set(state: &FoldState, root: &Path) -> Vec<String> {
        let mut names: Vec<String> = state
            .expanded_dirs(root)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn fold_state_lives_next_to_the_real_settings_file() {
        let path = fold_state_path_for(Path::new("/home/someone/.config/jerry/settings.toml"));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.config/jerry/file-tree-state.toml")
        );
    }

    #[test]
    fn an_unseen_worktree_starts_with_nothing_expanded() {
        let state = FoldState::default();
        assert!(state
            .expanded_dirs(Path::new("/repo/fresh-worktree"))
            .is_empty());
    }

    fn set(state: &mut FoldState, root: &Path, dir: &Path, expanded: bool) -> bool {
        match state.set_expanded(root, dir, expanded) {
            SetExpanded::Changed => true,
            SetExpanded::Unchanged => false,
            SetExpanded::Refused => panic!("unexpectedly refused {}", dir.display()),
        }
    }

    #[test]
    fn expanding_and_collapsing_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        let root = Path::new("/repo/worktree-a");

        let mut state = FoldState::default();
        assert!(set(&mut state, root, &root.join("src"), true));
        assert!(set(&mut state, root, &root.join("src/app"), true));
        state.save_at(&path).expect("save");

        let reloaded = FoldState::load_at(&path);
        assert_eq!(reloaded, state);
        assert_eq!(
            expanded_set(&reloaded, root),
            vec![
                "/repo/worktree-a/src".to_string(),
                "/repo/worktree-a/src/app".to_string()
            ]
        );
    }

    #[test]
    fn set_expanded_reports_whether_it_actually_changed_anything() {
        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        assert!(set(&mut state, root, &root.join("src"), true));
        assert!(
            !set(&mut state, root, &root.join("src"), true),
            "re-expanding an already-expanded directory changes nothing, so it must not \
             trigger a disk write"
        );
        assert!(set(&mut state, root, &root.join("src"), false));
        assert!(!set(&mut state, root, &root.join("src"), false));
    }

    /// The identity guard this module exists for: two worktrees of the same repository share
    /// every relative path, so state recorded for one must never appear in the other.
    #[test]
    fn fold_state_for_one_worktree_never_leaks_into_another() {
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");
        let mut state = FoldState::default();
        set(&mut state, a, &a.join("src"), true);

        assert_eq!(expanded_set(&state, a), vec!["/repo/worktree-a/src"]);
        assert!(
            state.expanded_dirs(b).is_empty(),
            "worktree B shares the relative path `src` with A, and must still start collapsed"
        );
    }

    #[test]
    fn a_path_outside_the_worktree_is_refused_rather_than_stored() {
        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        assert_eq!(
            state.set_expanded(root, Path::new("/etc/passwd"), true),
            SetExpanded::Refused
        );
        assert_eq!(
            state.set_expanded(root, Path::new("/repo/worktree-b/src"), true),
            SetExpanded::Refused
        );
        assert!(state.worktrees.is_empty());
    }

    #[test]
    fn the_worktree_root_itself_is_never_stored() {
        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        assert_eq!(state.set_expanded(root, root, true), SetExpanded::Refused);
        assert!(state.worktrees.is_empty());
    }

    #[test]
    fn a_stale_entry_for_a_deleted_folder_is_silently_pruned() {
        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        set(&mut state, root, &root.join("src"), true);
        set(&mut state, root, &root.join("deleted-since"), true);

        let mut existing = HashSet::new();
        existing.insert(root.join("src"));
        assert!(state.prune_missing_dirs(root, &existing));

        assert_eq!(expanded_set(&state, root), vec!["/repo/worktree-a/src"]);
    }

    #[test]
    fn pruning_reports_no_change_when_every_entry_still_exists() {
        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        set(&mut state, root, &root.join("src"), true);
        let mut existing = HashSet::new();
        existing.insert(root.join("src"));
        assert!(!state.prune_missing_dirs(root, &existing));
        assert_eq!(expanded_set(&state, root), vec!["/repo/worktree-a/src"]);
    }

    #[test]
    fn pruning_a_worktree_with_no_entry_at_all_is_not_an_error() {
        let mut state = FoldState::default();
        assert!(!state.prune_missing_dirs(Path::new("/repo/never-seen"), &HashSet::new()));
    }

    #[test]
    fn clear_worktree_forgets_only_that_worktree() {
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");
        let mut state = FoldState::default();
        set(&mut state, a, &a.join("src"), true);
        set(&mut state, b, &b.join("src"), true);

        assert!(state.clear_worktree(a));
        assert!(state.expanded_dirs(a).is_empty());
        assert_eq!(expanded_set(&state, b), vec!["/repo/worktree-b/src"]);
    }

    /// The multi-instance guarantee: a second `jerry` saving its own worktree's state must not
    /// erase the first's. Both instances hold a whole-file copy read at their own startup, so a
    /// plain whole-file write would do exactly that.
    #[test]
    fn saving_merges_with_another_instances_entries_instead_of_erasing_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");

        // Instance A saves first.
        let mut instance_a = FoldState::default();
        set(&mut instance_a, a, &a.join("src"), true);
        let owned_a: BTreeSet<String> = [worktree_key(a).expect("key")].into_iter().collect();
        instance_a.save_merged_at(&path, &owned_a).expect("save a");

        // Instance B started before A wrote anything, so its in-memory copy knows nothing of A.
        let mut instance_b = FoldState::default();
        set(&mut instance_b, b, &b.join("src"), true);
        let owned_b: BTreeSet<String> = [worktree_key(b).expect("key")].into_iter().collect();
        instance_b.save_merged_at(&path, &owned_b).expect("save b");

        let on_disk = FoldState::load_at(&path);
        assert_eq!(
            expanded_set(&on_disk, a),
            vec!["/repo/worktree-a/src"],
            "instance B's save must not have erased instance A's worktree"
        );
        assert_eq!(expanded_set(&on_disk, b), vec!["/repo/worktree-b/src"]);
    }

    /// The other half of the merge contract: for a worktree this instance *does* own, absence is
    /// a real deletion (that's how "collapse all" removes an entry), not something to merge back
    /// in from disk.
    #[test]
    fn a_merged_save_really_deletes_an_owned_worktrees_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        let root = Path::new("/repo/worktree-a");
        let owned: BTreeSet<String> = [worktree_key(root).expect("key")].into_iter().collect();

        let mut state = FoldState::default();
        set(&mut state, root, &root.join("src"), true);
        state.save_merged_at(&path, &owned).expect("save");
        assert_eq!(FoldState::load_at(&path).expanded_dirs(root).len(), 1);

        state.clear_worktree(root);
        state.save_merged_at(&path, &owned).expect("save");

        assert!(
            FoldState::load_at(&path).expanded_dirs(root).is_empty(),
            "collapse-all must survive the merge as a real deletion"
        );
    }

    #[test]
    fn a_missing_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = FoldState::load_at(&dir.path().join("does-not-exist.toml"));
        assert_eq!(state, FoldState::default());
    }

    #[test]
    fn a_corrupted_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write");
        assert_eq!(FoldState::load_at(&path), FoldState::default());
    }

    /// The non-UTF-8 refusal, exercised against a genuinely non-UTF-8 path rather than only
    /// described in the docs. Both halves matter: a directory name that isn't UTF-8 is refused
    /// (it has no honest TOML key), and - crucially - refusing it does not disturb the entries
    /// that *are* recordable for the same worktree.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_refused_and_leaves_the_rest_of_the_worktree_intact() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        set(&mut state, root, &root.join("src"), true);

        // 0xff is not valid UTF-8 in any position.
        let invalid = root.join(OsStr::from_bytes(&[b'b', b'a', 0xff, b'd']));
        assert_eq!(
            state.set_expanded(root, &invalid, true),
            SetExpanded::Refused,
            "a directory whose name isn't UTF-8 has no honest TOML key, so it must be refused \
             outright rather than stored lossily under a key that could collide"
        );
        assert_eq!(
            expanded_set(&state, root),
            vec!["/repo/worktree-a/src"],
            "and the refusal must leave every recordable entry for the same worktree untouched"
        );

        // The same refusal for a non-UTF-8 *root*, which would otherwise mangle the map key
        // itself - the collision this guard actually exists to prevent.
        let invalid_root = PathBuf::from(OsStr::from_bytes(&[b'/', b'r', 0xff, b'p']));
        assert_eq!(worktree_key(&invalid_root), None);
        assert_eq!(
            state.set_expanded(&invalid_root, &invalid_root.join("src"), true),
            SetExpanded::Refused
        );
        assert!(state.expanded_dirs(&invalid_root).is_empty());
    }

    /// A hand-corrupted (or maliciously written) file must not be able to make the app treat a
    /// path outside the worktree as an expanded directory.
    #[test]
    fn a_traversal_entry_in_a_hand_edited_file_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        std::fs::write(
            &path,
            "[worktrees.\"/repo/worktree-a\"]\nexpanded = [\"../worktree-b/src\", \"src\"]\n",
        )
        .expect("write");

        let state = FoldState::load_at(&path);
        assert_eq!(
            expanded_set(&state, Path::new("/repo/worktree-a")),
            vec!["/repo/worktree-a/src"],
            "the `..` entry must be silently dropped, and the good one kept"
        );
    }

    /// The atomic-write contract: after a save there is exactly one real file at `path` and no
    /// leftover temp file, and its contents parse.
    #[test]
    fn saving_leaves_no_temp_file_behind_and_the_result_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("file-tree-state.toml");
        let root = Path::new("/repo/worktree-a");
        let mut state = FoldState::default();
        set(&mut state, root, &root.join("src"), true);

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
        assert_eq!(siblings, vec!["file-tree-state.toml".to_string()]);
        assert_eq!(FoldState::load_at(&path), state);
    }

    /// A save killed between creating its temp file and renaming it leaves an orphan behind.
    /// Because the temp name is process-unique (so two instances can't tear each other's
    /// writes), those would otherwise accumulate forever.
    #[test]
    fn a_stale_orphaned_temp_file_is_swept_and_a_fresh_one_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        let stale = dir.path().join("file-tree-state.toml.999999.0.tmp");
        let fresh = dir.path().join("file-tree-state.toml.999998.0.tmp");
        let unrelated = dir.path().join("settings.toml");
        std::fs::write(&stale, "orphan").expect("write");
        std::fs::write(&fresh, "in flight").expect("write");
        std::fs::write(&unrelated, "keep me").expect("write");
        // Backdate the orphan well past `ORPHANED_TEMP_MAX_AGE` (`File::set_modified`, stable
        // since Rust 1.75 - no extra dev-dependency needed to age a file).
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60 * 24);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("open orphan")
            .set_modified(old)
            .expect("backdate");

        FoldState::default().save_at(&path).expect("save");

        assert!(!stale.exists(), "the aged orphan must be swept");
        assert!(
            fresh.exists(),
            "a temp file young enough to be another instance's live write must be left alone"
        );
        assert!(unrelated.exists(), "unrelated siblings are never touched");
        assert!(path.exists());
    }

    /// Overwriting an existing file must replace it wholesale, not merge into it.
    #[test]
    fn saving_over_an_existing_file_replaces_its_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file-tree-state.toml");
        let root = Path::new("/repo/worktree-a");

        let mut first = FoldState::default();
        set(&mut first, root, &root.join("src"), true);
        first.save_at(&path).expect("save");

        let second = FoldState::default();
        second.save_at(&path).expect("save");

        assert_eq!(FoldState::load_at(&path), FoldState::default());
    }
}
