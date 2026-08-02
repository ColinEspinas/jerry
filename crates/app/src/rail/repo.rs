//! A **repo**: one git repository the user has added to Jerry - the top level of the rail's
//! two-level grouping, `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.0 ("What
//! a repo is here, and why the rail groups by one"). A real checkout on disk (`~/code/
//! jerry-core`); every worktree belongs to exactly one repo and carries it (`repo:` on the
//! worktree record, a later phase's concern - see [`Repo::worktrees`]'s own docs for why that
//! field can't just be derived by walking up a worktree's path).
//!
//! Kept separate from [`super::worktrees`] (which maps *one* repo's `wt_core::WorktreeResult`
//! list into display rows) the same way [`super::status`] is separate from [`super::state`]:
//! [`Repo`] is the identity/persistence layer multiple worktrees hang off of, not another
//! worktree-shaped thing itself.
//!
//! ## Persistence
//!
//! Which repos are currently added must survive an app restart (this revision's Phase 0 scope -
//! see the revision doc's introduction). [`RepoState`] is `crate::sidebar::fold_state::FoldState`'s
//! own shape and safety properties, copied rather than reinvented: a sibling
//! `~/.config/jerry/repos.toml`, written via the identical crash-safe temp-file-then-rename
//! sequence ([`RepoState::save_at`]), and merged rather than clobbered when more than one `jerry`
//! process is running ([`RepoState::save_merged_at`]) - see that module's docs for the full
//! reasoning, which applies here unchanged. The one difference: a repo has no per-worktree
//! sub-structure to merge *within* one key the way fold state's `expanded` set does, so there is
//! no `*_with_key` fast path here - [`repo_key`] is only ever called from real add/remove/save
//! points (a user action, or startup), never a render or per-keystroke path, so its blocking
//! `std::fs::canonicalize` call is cheap enough to call directly rather than caching it the way
//! `crate::root::AdeApp::fold_state_root_key` caches [`super::worktrees`]'s equivalent.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::rail::worktrees::WorktreeItem;

/// Stable identity for one added repo. A plain, process-local monotonic counter
/// (`crate::root::AdeApp::next_repo_id`) - **not** derived from the path, since a repo can be
/// removed and re-added (or its directory renamed) without the identity a worktree's `repo:`
/// reference points at needing to change mid-agent. Never persisted itself: [`RepoState`]
/// re-derives a fresh id per entry on every load (see that type's docs), since nothing durable
/// outside one running process needs to reference a specific numeric id across a restart yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId(pub u64);

/// One git repository the user has added - the rail's group header, and eventually the owner of
/// every worktree under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub id: RepoId,
    /// The real checkout's path on disk. Jerry's own worktrees for this repo live *outside* this
    /// path entirely (`~/.jerry/wt/<name>`, per the revision doc) - this is only ever the main
    /// checkout's own location.
    pub path: PathBuf,
    /// What the rail's group header shows - the directory's own basename by default (see
    /// [`display_name`]), persisted alongside the path so a repo whose directory has since been
    /// renamed or gone missing still has something honest to call itself.
    pub name: String,
    /// Every worktree belonging to this repo, main checkout included (`design_handoff_jerry_ade/
    /// revision 3/REVISION-2026-07-31.md` §2.0: "The repo's main checkout is itself a worktree
    /// row in its group"). Always empty today - **not** wired to `wt_core::list_worktrees` by
    /// this phase, which is data-model-and-persistence only (see `crate::root::AdeApp::worktrees`
    /// for where the single-focused-repo worktree list still lives while that wiring is pending).
    /// Real per-repo population is a later rail-rendering phase's job; this field exists now so
    /// that phase has a real place to write into rather than bolting one on afterwards.
    pub worktrees: Vec<WorktreeItem>,
}

impl Repo {
    /// Builds a fresh, worktree-less `Repo` for `path`, deriving its display name the same way
    /// [`super::worktrees::build_worktree_items`] derives a worktree's own label when it has no
    /// branch: the path's basename, falling back to the whole path for a root-only path (`/`) or
    /// one that ends in `..`/`.`.
    pub fn new(id: RepoId, path: PathBuf) -> Self {
        let name = display_name(&path);
        Repo {
            id,
            path,
            name,
            worktrees: Vec::new(),
        }
    }
}

fn display_name(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

/// The repo-list file's name, resolved next to the real `settings.toml` - mirrors
/// `crate::sidebar::fold_state::FOLD_STATE_FILE_NAME`.
pub const REPO_STATE_FILE_NAME: &str = "repos.toml";

/// The repo-list file for a given real settings-file path - identical reasoning to
/// `crate::sidebar::fold_state::fold_state_path_for`: a test that supplies a temp-dir settings
/// path gets real, isolated repo-list persistence in that same directory, and a test that passes
/// `None` gets none at all.
pub fn repo_state_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(REPO_STATE_FILE_NAME),
        None => PathBuf::from(REPO_STATE_FILE_NAME),
    }
}

/// The map key for one repo - its real path, canonicalized so the same repo reached through a
/// symlink (or a `.`-relative invocation) resolves to one entry rather than two. Falls back to
/// the path as given when it can't be canonicalized (doesn't exist yet, or is a pure in-memory
/// path in a unit test) - still a stable key for that path. `None` for a path that isn't valid
/// UTF-8, refused outright for the identical reason `crate::sidebar::fold_state::worktree_key`
/// refuses one: a lossy `to_string_lossy` key could collide two genuinely different repos onto
/// one TOML key.
pub fn repo_key(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.to_str().map(str::to_owned)
}

/// The whole on-disk repo-list file: every repo this user has ever added, keyed by [`repo_key`].
/// A `BTreeMap` (not a `HashMap`) so the serialized file has a stable, diffable order rather than
/// reshuffling itself on every write - the identical reasoning
/// `crate::sidebar::fold_state::FoldState` gives for its own map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoState {
    pub repos: BTreeMap<String, RepoRecord>,
    /// GitHub issue #90's "remembers the last-opened folder" - the [`repo_key`] of whichever
    /// repo [`crate::root::AdeApp::focus_repo`] most recently focused, so a fresh launch with no
    /// CLI argument can reopen it automatically. `None` for a user who has never focused a repo
    /// at all (or an on-disk file predating this field - `#[serde(default)]` on the struct
    /// covers that the same way every other field here already does). Deliberately a bare key,
    /// not a `PathBuf`: it is only ever compared against [`Self::repos`]' own keys
    /// ([`Self::last_focused_existing_path`]), never read as a path directly.
    pub last_focused: Option<String>,
}

/// One repo's persisted record - just its display name today. The map key (a [`repo_key`]) is
/// the path; nothing about `Repo::worktrees` is persisted here (see that field's own docs for
/// why - it isn't populated yet at all).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoRecord {
    pub name: String,
}

/// How stale a `*.tmp` sibling must be before [`sweep_orphaned_temp_files`] deletes it - the
/// identical constant/reasoning `crate::sidebar::fold_state` uses for its own file.
const ORPHANED_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Deletes leftover `<file-name>.<pid>.<n>.tmp` siblings from a save that was killed between
/// creating its temp file and renaming it - see `crate::sidebar::fold_state::
/// sweep_orphaned_temp_files`'s own docs, which this mirrors exactly for `repos.toml`'s own
/// sibling temp files rather than sharing one function across two otherwise-unrelated file
/// formats.
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

impl RepoState {
    /// Loads `path`, falling back to an empty state for *any* failure - missing file (the
    /// overwhelmingly common first-run case), unreadable file, or unparseable contents. Exactly
    /// `crate::sidebar::fold_state::FoldState::load_at`'s own contract: the repo list is never
    /// important enough to fail startup over.
    pub fn load_at(path: &Path) -> RepoState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return RepoState::default();
        };
        match toml::from_str::<RepoState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from an empty repo list",
                    path.display()
                );
                RepoState::default()
            }
        }
    }

    /// Writes `self` to `path` **atomically**: a sibling `*.tmp` file first, `File::sync_all`,
    /// then a same-directory `std::fs::rename` over the target, then a best-effort parent-
    /// directory sync - identical mechanics and identical reasoning to
    /// `crate::sidebar::fold_state::FoldState::save_at` (see that method's own docs for exactly
    /// why each step exists); repeated here rather than shared because the two persisted files
    /// have unrelated shapes and this crash-safety sequence is the only part they have in common.
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
            .unwrap_or_else(|| REPO_STATE_FILE_NAME.to_string());
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

    /// The app's real write path: merges `self`'s entries for the repos this instance actually
    /// touched (`owned`) into whatever is currently on disk, then writes the result via
    /// [`Self::save_at`] - `crate::sidebar::fold_state::FoldState::save_merged_at`'s own contract,
    /// copied because two `jerry` processes each open against a different repo are exactly the
    /// same "two writers, one file" situation fold state already solved: a plain whole-file write
    /// here would let the second process's save silently erase the first process's repo from the
    /// list the moment it saves anything of its own.
    ///
    /// `owned` is the set of [`repo_key`]s this instance has recorded anything for (added or
    /// removed) - taken from `self`, *including absence* (that's how removing a repo deletes its
    /// entry); every other key already on disk is passed through untouched.
    pub fn save_merged_at(
        &self,
        path: &Path,
        owned: &std::collections::BTreeSet<String>,
    ) -> io::Result<()> {
        // GitHub issue #90: "New Window" made this file's own load-merge-save cycle reachable
        // *concurrently within one process* for the first time - two independent `AdeApp`
        // instances, each with its own async writer loop (`crate::root::AdeApp::
        // persist_repo_state`), can now both be mid-`save_merged_at` against the exact same real
        // `repos.toml` at once. The `owned`-scoped merge below already protects against two
        // separate *processes* stomping each other's keys (see this method's own docs above), but
        // that protection assumes each `load_at` genuinely observes the other's already-completed
        // write - which two truly concurrent calls do not guarantee: both could `load_at` the same
        // pre-write state, merge their own `owned` keys into their own independent copy, and then
        // whichever `save_at` lands second would silently overwrite the first's freshly-written
        // keys with its own now-stale copy of everything else. `persisted_state_lock::
        // with_locked_merge` closes that window - see its own module docs for why one shared,
        // process-wide (not per-path) lock is enough, and why `crate::sidebar::fold_state`/
        // `crate::work_surface::tab_order_state`'s own `save_merged_at` methods share the exact
        // same lock rather than each rolling an independent copy.
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = RepoState::load_at(path);
            for key in owned {
                match self.repos.get(key) {
                    Some(entry) => merged.repos.insert(key.clone(), entry.clone()),
                    None => merged.repos.remove(key),
                };
            }
            // `last_focused` is a single global value, not per-repo, so it doesn't fit the
            // per-key `owned` merge loop above - `self.last_focused` (this process's own current
            // idea of "which repo did I last focus") always wins over whatever another process
            // last wrote, the same last-writer-wins tradeoff this file's own multi-instance design
            // already accepts for everything else it persists. Only overwritten when `self`
            // genuinely has an opinion: `Self::repo_state_snapshot`'s only real caller derives
            // this from a live `AdeApp::focused_repo()`, which is `None` only when `Self::repos`
            // is itself empty - a state `persist_repo_state`'s own callers (`add_repo`/
            // `focus_repo`) never reach, but guarded here anyway rather than let some future
            // caller silently blank out a real remembered repo another process just wrote.
            if self.last_focused.is_some() {
                merged.last_focused = self.last_focused.clone();
            }
            merged.save_at(path)
        })
    }

    /// GitHub issue #90's own real "still valid" check for [`Self::last_focused`]: `None` unless
    /// it names a repo genuinely present in [`Self::repos`] *and* that repo's own path still
    /// exists on disk right now. A remembered repo that was since deleted or moved must fall back
    /// to a genuinely empty startup rather than a broken/error one - see
    /// `crate::root::AdeApp::new_with_settings`'s own docs for how the caller uses this.
    pub fn last_focused_existing_path(&self) -> Option<PathBuf> {
        let key = self.last_focused.as_ref()?;
        if !self.repos.contains_key(key) {
            return None;
        }
        let path = PathBuf::from(key);
        // `is_dir`, not `exists` - an independent audit's real finding: a repo's own real
        // checkout is a directory, so a real path that now names a plain file (replaced by hand,
        // or by some other program, after this was last remembered) must fall back to empty the
        // same way a fully deleted path already does, rather than being treated as "still a real
        // repo" and failing downstream once something tries to actually read it as one.
        path.is_dir().then_some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_state_lives_next_to_the_real_settings_file() {
        let path = repo_state_path_for(Path::new("/home/someone/.config/jerry/settings.toml"));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.config/jerry/repos.toml")
        );
    }

    #[test]
    fn repo_new_derives_a_display_name_from_the_path_basename() {
        let repo = Repo::new(RepoId(0), PathBuf::from("/home/user/code/jerry-core"));
        assert_eq!(repo.name, "jerry-core");
        assert!(repo.worktrees.is_empty());
    }

    #[test]
    fn repo_new_falls_back_to_the_whole_path_when_there_is_no_basename() {
        let repo = Repo::new(RepoId(0), PathBuf::from("/"));
        assert_eq!(repo.name, "/");
    }

    #[test]
    fn a_missing_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = RepoState::load_at(&dir.path().join("does-not-exist.toml"));
        assert_eq!(state, RepoState::default());
    }

    #[test]
    fn a_corrupted_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repos.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write");
        assert_eq!(RepoState::load_at(&path), RepoState::default());
    }

    #[test]
    fn saving_and_loading_round_trips_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repos.toml");
        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
            },
        );
        state.repos.insert(
            "/repo/b".to_string(),
            RepoRecord {
                name: "b".to_string(),
            },
        );
        state.save_at(&path).expect("save");

        let reloaded = RepoState::load_at(&path);
        assert_eq!(reloaded, state);
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("repos.toml");
        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
            },
        );
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
        assert_eq!(siblings, vec!["repos.toml".to_string()]);
    }

    /// The multi-instance guarantee, mirroring `crate::sidebar::fold_state`'s own identical test:
    /// a second `jerry` instance saving its own repo must not erase the first's.
    #[test]
    fn saving_merges_with_another_instances_repo_instead_of_erasing_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repos.toml");

        let mut instance_a = RepoState::default();
        instance_a.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
            },
        );
        let owned_a: std::collections::BTreeSet<String> =
            ["/repo/a".to_string()].into_iter().collect();
        instance_a.save_merged_at(&path, &owned_a).expect("save a");

        // Instance B started before A wrote anything, so its in-memory copy knows nothing of A.
        let mut instance_b = RepoState::default();
        instance_b.repos.insert(
            "/repo/b".to_string(),
            RepoRecord {
                name: "b".to_string(),
            },
        );
        let owned_b: std::collections::BTreeSet<String> =
            ["/repo/b".to_string()].into_iter().collect();
        instance_b.save_merged_at(&path, &owned_b).expect("save b");

        let on_disk = RepoState::load_at(&path);
        assert!(
            on_disk.repos.contains_key("/repo/a"),
            "instance B's save must not have erased instance A's repo"
        );
        assert!(on_disk.repos.contains_key("/repo/b"));
    }

    /// The other half of the merge contract: for a repo this instance *does* own, absence is a
    /// real deletion (how removing a repo removes its entry), not something merged back in from
    /// disk.
    #[test]
    fn a_merged_save_really_deletes_an_owned_repos_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repos.toml");
        let owned: std::collections::BTreeSet<String> =
            ["/repo/a".to_string()].into_iter().collect();

        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
            },
        );
        state.save_merged_at(&path, &owned).expect("save");
        assert!(RepoState::load_at(&path).repos.contains_key("/repo/a"));

        state.repos.remove("/repo/a");
        state.save_merged_at(&path, &owned).expect("save");

        assert!(
            !RepoState::load_at(&path).repos.contains_key("/repo/a"),
            "removing a repo must survive the merge as a real deletion"
        );
    }

    /// GitHub issue #90's own real "still valid" check - the four cases
    /// [`RepoState::last_focused_existing_path`]'s own docs enumerate.
    #[test]
    fn last_focused_existing_path_is_none_when_nothing_was_ever_remembered() {
        let state = RepoState::default();
        assert_eq!(state.last_focused_existing_path(), None);
    }

    #[test]
    fn last_focused_existing_path_is_none_when_the_key_is_not_a_known_repo() {
        // `last_focused` names a key that was never actually added to `repos` (e.g. a hand-
        // edited file, or a repo since removed) - must not resolve to a path anyway.
        let state = RepoState {
            last_focused: Some("/repo/never-added".to_string()),
            ..RepoState::default()
        };
        assert_eq!(state.last_focused_existing_path(), None);
    }

    #[test]
    fn last_focused_existing_path_is_none_when_the_remembered_directory_no_longer_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("deleted-repo");
        std::fs::create_dir(&gone).expect("mkdir");
        let key = gone.to_str().expect("utf8 path").to_string();

        let mut state = RepoState::default();
        state.repos.insert(
            key.clone(),
            RepoRecord {
                name: "deleted-repo".to_string(),
            },
        );
        state.last_focused = Some(key);
        // The directory is removed *after* being recorded - simulating a repo that was open,
        // remembered, and has since been deleted or moved.
        std::fs::remove_dir(&gone).expect("rmdir");

        assert_eq!(
            state.last_focused_existing_path(),
            None,
            "a deleted/moved remembered folder must fall back to empty, not a broken path"
        );
    }

    /// The other real "no longer a usable repo" case an independent audit found this method
    /// missing: the remembered path still `exists()` (so a plain `exists()` check would wrongly
    /// accept it), but a real directory was replaced by a plain file - `is_dir()` is the real
    /// check that must reject this, not merely "something is there".
    #[test]
    fn last_focused_existing_path_is_none_when_the_remembered_path_was_replaced_by_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let replaced = dir.path().join("was-a-repo");
        std::fs::create_dir(&replaced).expect("mkdir");
        let key = replaced.to_str().expect("utf8 path").to_string();

        let mut state = RepoState::default();
        state.repos.insert(
            key.clone(),
            RepoRecord {
                name: "was-a-repo".to_string(),
            },
        );
        state.last_focused = Some(key);

        // The directory is removed and replaced with a plain file of the same name - the path
        // still `exists()`, but is no longer a real, usable repo checkout.
        std::fs::remove_dir(&replaced).expect("rmdir");
        std::fs::write(&replaced, "not a repo").expect("write");
        assert!(replaced.exists(), "sanity check: the path still exists");
        assert!(
            !replaced.is_dir(),
            "sanity check: it is a file now, not a directory"
        );

        assert_eq!(
            state.last_focused_existing_path(),
            None,
            "a remembered path replaced by a plain file must fall back to empty, not be treated \
             as a still-usable repo"
        );
    }

    #[test]
    fn last_focused_existing_path_resolves_a_real_known_and_still_existing_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().to_str().expect("utf8 path").to_string();

        let mut state = RepoState::default();
        state.repos.insert(
            key.clone(),
            RepoRecord {
                name: "repo".to_string(),
            },
        );
        state.last_focused = Some(key);

        assert_eq!(
            state.last_focused_existing_path(),
            Some(dir.path().to_path_buf())
        );
    }

    /// [`RepoState::save_merged_at`] must persist `last_focused` too, not just `repos` - the
    /// real mechanism GitHub issue #90's "remembers the last-opened folder" needs.
    #[test]
    fn save_merged_at_persists_last_focused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repos.toml");
        let owned: std::collections::BTreeSet<String> =
            ["/repo/a".to_string()].into_iter().collect();

        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
            },
        );
        state.last_focused = Some("/repo/a".to_string());
        state.save_merged_at(&path, &owned).expect("save");

        let reloaded = RepoState::load_at(&path);
        assert_eq!(reloaded.last_focused, Some("/repo/a".to_string()));
    }

    /// `last_focused` is a real, single global value - a later save from the same instance with
    /// a genuine new focus must overwrite whatever the previous save left, even across two
    /// separate `save_merged_at` calls simulating two real `AdeApp::focus_repo` calls.
    #[test]
    fn save_merged_at_overwrites_a_previous_last_focused_with_a_newer_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repos.toml");
        let owned: std::collections::BTreeSet<String> =
            ["/repo/a".to_string(), "/repo/b".to_string()]
                .into_iter()
                .collect();

        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
            },
        );
        state.repos.insert(
            "/repo/b".to_string(),
            RepoRecord {
                name: "b".to_string(),
            },
        );
        state.last_focused = Some("/repo/a".to_string());
        state.save_merged_at(&path, &owned).expect("save a focused");
        assert_eq!(
            RepoState::load_at(&path).last_focused,
            Some("/repo/a".to_string())
        );

        state.last_focused = Some("/repo/b".to_string());
        state.save_merged_at(&path, &owned).expect("save b focused");
        assert_eq!(
            RepoState::load_at(&path).last_focused,
            Some("/repo/b".to_string()),
            "focusing repo B afterwards must really overwrite the previously persisted repo A"
        );
    }
}
