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
    /// row in its group"). Populated by a real `wt_core::list_worktrees_porcelain` fetch for
    /// *every* repo, not just the focused one - `crate::root::AdeApp::load_repo_worktrees`
    /// (a one-shot fetch, run once per newly [`crate::root::AdeApp::add_repo`]-ed repo and once
    /// per repo restored from `repos.toml` at startup) and `crate::root::AdeApp::
    /// start_repo_worktrees_polling` (the periodic keep-fresh sweep for every non-focused repo)
    /// are the only two writers. The currently focused repo is the one exception: its own entry
    /// is instead mirrored straight from `crate::root::AdeApp::load_worktrees`'s real fetch (see
    /// that method's own docs) rather than independently fetched a second time, so this repo's
    /// git is never queried twice in parallel for the same data.
    ///
    /// Empty until [`Self::worktrees_loaded`] is `true` - see that field's own docs for why an
    /// empty `Vec` alone can't be trusted as "this repo really has zero worktrees".
    pub worktrees: Vec<WorktreeItem>,
    /// Whether [`Self::worktrees`] reflects a real, completed fetch attempt for this repo - not
    /// merely "non-empty", since a genuinely empty result (an inaccessible path;
    /// `wt_core::list_worktrees_porcelain`'s own `Err` case) is still a real, definitive answer
    /// that must be told apart from "never even asked yet" (this repo added a moment ago, its
    /// first fetch still in flight). `false` only ever means the latter. See
    /// [`crate::rail::state::RepoWorktrees::rows_loaded`], which this feeds directly, for how the
    /// rail render side uses the same distinction.
    pub worktrees_loaded: bool,
}

impl Repo {
    /// Builds a fresh, worktree-less `Repo` for `path`, deriving its display name the same way
    /// [`super::worktrees::build_worktree_items`] derives a worktree's own label when it has no
    /// branch: the path's basename, falling back to the whole path for a root-only path (`/`) or
    /// one that ends in `..`/`.`. [`Self::worktrees_loaded`] starts `false` - the caller
    /// (`crate::root::AdeApp::add_repo`) always follows this with a real
    /// `crate::root::AdeApp::load_repo_worktrees` call to populate it.
    pub fn new(id: RepoId, path: PathBuf) -> Self {
        let name = display_name(&path);
        Repo {
            id,
            path,
            name,
            worktrees: Vec::new(),
            worktrees_loaded: false,
        }
    }
}

/// Splits `ids` into fixed-size batches of at most `concurrency` each, preserving relative
/// order - `crate::root::AdeApp::start_repo_worktrees_polling`'s own bound on how many real `git
/// worktree list` subprocesses its non-focused-repo refresh sweep lets run at once: it fully
/// awaits one batch's real subprocess calls (spawned concurrently on the background executor)
/// before moving on to the next, so no more than `concurrency` are ever in flight for that sweep
/// simultaneously, regardless of how many repos are due for a refresh. A user with dozens of
/// added repos never causes a single tick to fire dozens of `git` child processes at once.
///
/// `concurrency == 0` is treated as `1` - a defensive floor, since `[T]::chunks` itself panics on
/// a zero chunk size, and the caller's own constant is never expected to be zero anyway.
pub(crate) fn batch_repos_for_refresh(ids: &[RepoId], concurrency: usize) -> Vec<Vec<RepoId>> {
    ids.chunks(concurrency.max(1))
        .map(<[RepoId]>::to_vec)
        .collect()
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

/// The one real normalization every repo path stored in [`Repo::path`] goes through: fully
/// resolved (symlinks followed, `.`/`..` and a relative invocation made absolute), falling back to
/// the path exactly as given when it can't be resolved at all - a directory that doesn't exist
/// yet, or a pure in-memory path in a unit test, which must still be usable rather than an error.
///
/// This is load-bearing, not cosmetic. Every worktree path this app displays comes from
/// `wt_core::list_worktrees_porcelain`, i.e. from git, which always reports **fully resolved**
/// paths (git derives them from `getcwd`, which resolves symlinks). Every one of this app's real
/// per-worktree lookups is an exact `PathBuf` comparison against those - `crate::rail::state::
/// build_worktree_rows_with_history` folding an agent into its worktree row,
/// `crate::work_surface::agents::Agents::iter_for_cwd`/`activate_for_worktree`,
/// `crate::root::AdeApp::diff_cache`/`worktree_notes`/`open_files_by_worktree`/`edit_buffers`.
/// A [`Repo::path`] kept verbatim from `jerry .`, `jerry ~/link-to-repo`, or any relative
/// argument therefore never equals git's own answer for the same directory, and
/// `crate::root::AdeApp::current_worktree_path`'s repo-root fallback hands exactly that unresolved
/// path to `Agents::spawn` as an agent's `cwd`. The real, reproduced consequence: an agent
/// spawned that way matches no worktree row at all, and - because `build_worktree_rows_with_
/// history` maps over *worktrees* and folds agents into them - is dropped from the rail
/// silently, with no row of its own and no error anywhere.
///
/// Normalizing once, where a repo path enters this app ([`crate::root::AdeApp::add_repo`],
/// [`crate::root::AdeApp::open_repo_in_current_window`], and startup's own resolved CLI path), is
/// what keeps that whole family of exact-path comparisons meaningful - as opposed to
/// canonicalizing at each comparison, which would put a blocking `std::fs::canonicalize` on a
/// per-row, per-render path and still leave the *spawned process's* real cwd unresolved.
pub fn canonical_repo_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The map key for one repo - its real path, canonicalized ([`canonical_repo_path`]) so the same
/// repo reached through a symlink (or a `.`-relative invocation) resolves to one entry rather
/// than two. `None` for a path that isn't valid UTF-8, refused outright for the identical reason
/// `crate::sidebar::fold_state::worktree_key` refuses one: a lossy `to_string_lossy` key could
/// collide two genuinely different repos onto one TOML key.
pub fn repo_key(path: &Path) -> Option<String> {
    canonical_repo_path(path).to_str().map(str::to_owned)
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

/// One repo's persisted record. The map key (a [`repo_key`]) is the path; the repo's *worktree
/// list* is deliberately not persisted at all (see [`Repo::worktrees`]'s own docs - it is always
/// re-fetched from real git), only which one of them was last worked in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoRecord {
    pub name: String,
    /// The real path of the worktree of this repo that `crate::root::AdeApp::select_worktree` most
    /// recently selected, so relaunching Jerry lands back in it rather than always in the main
    /// checkout - the per-repo counterpart to [`RepoState::last_focused`], and the half that makes
    /// "everything reopens" true at launch rather than only after the user clicks the right rail
    /// row (a worktree's own tabs are restored when it is genuinely selected - see
    /// `crate::work_surface::session`).
    ///
    /// Stored as a plain path string rather than a [`PathBuf`] for the same reason every other
    /// field in this file is a string: it is a TOML value that must survive a build that has never
    /// heard of it. `None` for a repo whose worktrees were never selected in, and for a record
    /// written before this field existed (`#[serde(default)]`). Never trusted blindly on read -
    /// [`RepoState::remembered_worktree`] checks it still names a real directory, and
    /// `crate::rail::worktrees::selection_for_opened_repo` independently checks it still names a
    /// real worktree of the repo, falling back to the main checkout if not.
    pub selected_worktree: Option<String>,
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

    /// [`RepoRecord::selected_worktree`] for `key`, if it still names a real directory on disk -
    /// [`Self::last_focused_existing_path`]'s own "remembered, and still there" check, applied one
    /// level down. `None` for an unknown repo, a repo never selected in, or a worktree since
    /// removed/pruned; every one of those falls back to
    /// `crate::rail::worktrees::selection_for_opened_repo`'s ordinary main-checkout answer rather
    /// than to a broken selection.
    pub fn remembered_worktree(&self, key: &str) -> Option<PathBuf> {
        let path = PathBuf::from(self.repos.get(key)?.selected_worktree.as_ref()?);
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

    /// A repo's display name is its path's own basename, falling back to the whole path when
    /// there is no basename to take - and a freshly constructed repo starts with nothing loaded,
    /// so nothing renders a worktree list it has not actually fetched yet.
    #[test]
    fn repo_new_names_itself_from_its_path_and_starts_with_nothing_loaded() {
        let repo = Repo::new(RepoId(0), PathBuf::from("/home/user/code/jerry-core"));
        assert_eq!(repo.name, "jerry-core");
        assert!(repo.worktrees.is_empty());
        assert!(!repo.worktrees_loaded);

        assert_eq!(Repo::new(RepoId(0), PathBuf::from("/")).name, "/");
    }

    /// Neither a file that is not there nor one that is not valid TOML may fail the load - a
    /// hand-edited or half-written `repos.toml` degrades to the empty state, never an error the
    /// caller has to handle at startup.
    #[test]
    fn a_missing_or_corrupted_file_loads_as_empty_state_rather_than_failing() {
        let dir = crate::test_support::temp_root();
        assert_eq!(
            RepoState::load_at(&dir.path().join("does-not-exist.toml")),
            RepoState::default()
        );

        let path = dir.path().join("repos.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write");
        assert_eq!(RepoState::load_at(&path), RepoState::default());
    }

    /// The per-repo "land back where I left off" record: remembered, still on disk, and really
    /// returned - plus the three real ways it must fall back to `None` rather than to a broken
    /// selection (unknown repo, never selected in, since deleted).
    #[test]
    fn a_remembered_worktree_is_returned_only_while_it_still_exists() {
        let worktree = crate::test_support::temp_root();
        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
                selected_worktree: Some(worktree.path().to_string_lossy().into_owned()),
            },
        );
        state.repos.insert(
            "/repo/never-selected".to_string(),
            RepoRecord {
                name: "never-selected".to_string(),
                selected_worktree: None,
            },
        );

        assert_eq!(
            state.remembered_worktree("/repo/a"),
            Some(worktree.path().to_path_buf())
        );
        assert_eq!(state.remembered_worktree("/repo/never-selected"), None);
        assert_eq!(state.remembered_worktree("/repo/not-a-known-repo"), None);

        drop(worktree);
        assert_eq!(
            state.remembered_worktree("/repo/a"),
            None,
            "a worktree removed or pruned since it was remembered must fall back, not be \
             selected into a directory that no longer exists"
        );
    }

    #[test]
    fn saving_and_loading_round_trips_a_real_file() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("repos.toml");
        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
                selected_worktree: None,
            },
        );
        state.repos.insert(
            "/repo/b".to_string(),
            RepoRecord {
                name: "b".to_string(),
                selected_worktree: None,
            },
        );
        state.save_at(&path).expect("save");

        let reloaded = RepoState::load_at(&path);
        assert_eq!(reloaded, state);
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("nested").join("repos.toml");
        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
                selected_worktree: None,
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
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("repos.toml");

        let mut instance_a = RepoState::default();
        instance_a.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
                selected_worktree: None,
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
                selected_worktree: None,
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
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("repos.toml");
        let owned: std::collections::BTreeSet<String> =
            ["/repo/a".to_string()].into_iter().collect();

        let mut state = RepoState::default();
        state.repos.insert(
            "/repo/a".to_string(),
            RepoRecord {
                name: "a".to_string(),
                selected_worktree: None,
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

    /// GitHub issue #90's own real "still valid" check - every case
    /// [`RepoState::last_focused_existing_path`]'s own docs enumerate. The last is the one an
    /// independent audit found missing: the remembered path still `exists()` (so a plain
    /// `exists()` check would wrongly accept it) but the directory has been replaced by a plain
    /// file, which only `is_dir()` rejects.
    #[test]
    fn last_focused_existing_path_resolves_only_a_known_repo_that_is_still_a_real_directory() {
        assert_eq!(RepoState::default().last_focused_existing_path(), None);

        // `last_focused` naming a key that was never added to `repos` (a hand-edited file, or a
        // repo since removed) must not resolve to a path anyway.
        let unknown = RepoState {
            last_focused: Some("/repo/never-added".to_string()),
            ..RepoState::default()
        };
        assert_eq!(unknown.last_focused_existing_path(), None);

        let dir = crate::test_support::temp_root();
        let remembered = |path: &std::path::Path| {
            let key = path.to_str().expect("utf8 path").to_string();
            let mut state = RepoState::default();
            state.repos.insert(
                key.clone(),
                RepoRecord {
                    name: "repo".to_string(),
                    selected_worktree: None,
                },
            );
            state.last_focused = Some(key);
            state
        };

        let live = dir.path().join("live-repo");
        std::fs::create_dir(&live).expect("mkdir");
        assert_eq!(
            remembered(&live).last_focused_existing_path(),
            Some(live.clone())
        );

        let gone = dir.path().join("deleted-repo");
        std::fs::create_dir(&gone).expect("mkdir");
        let state = remembered(&gone);
        std::fs::remove_dir(&gone).expect("rmdir");
        assert_eq!(
            state.last_focused_existing_path(),
            None,
            "a deleted/moved remembered folder must fall back to empty, not a broken path"
        );

        let replaced = dir.path().join("was-a-repo");
        std::fs::create_dir(&replaced).expect("mkdir");
        let state = remembered(&replaced);
        std::fs::remove_dir(&replaced).expect("rmdir");
        std::fs::write(&replaced, "not a repo").expect("write");
        assert!(replaced.exists() && !replaced.is_dir());
        assert_eq!(
            state.last_focused_existing_path(),
            None,
            "a remembered path replaced by a plain file must fall back to empty, not be treated \
             as a still-usable repo"
        );
    }

    /// [`RepoState::save_merged_at`] must persist `last_focused` too, not just `repos` - the
    /// real mechanism GitHub issue #90's "remembers the last-opened folder" needs - and it is a
    /// single global value, so a later save with a genuine new focus overwrites whatever the
    /// previous save left, across two separate calls simulating two real `AdeApp::focus_repo`s.
    #[test]
    fn save_merged_at_persists_last_focused_and_a_later_save_overwrites_it() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("repos.toml");
        let owned: std::collections::BTreeSet<String> =
            ["/repo/a".to_string(), "/repo/b".to_string()]
                .into_iter()
                .collect();

        let mut state = RepoState::default();
        for name in ["a", "b"] {
            state.repos.insert(
                format!("/repo/{name}"),
                RepoRecord {
                    name: name.to_string(),
                    selected_worktree: None,
                },
            );
        }

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

    /// The concurrency cap's real shape: ten due repos with a cap of four must split into
    /// `[4, 4, 2]`, never a single batch of ten (which would let all ten real `git worktree list`
    /// subprocesses fire at once) and never one repo per batch either (which would serialize the
    /// whole sweep needlessly). A cap of zero is a defensive floor rather than a real call site -
    /// every real caller passes a positive constant - and must neither panic (`[T]::chunks`
    /// itself panics on a zero chunk size) nor silently drop every id.
    #[test]
    fn batch_repos_for_refresh_caps_every_batch_and_never_loses_an_id() {
        for (count, concurrency, expected) in [
            (10, 4, vec![4, 4, 2]),
            (3, 4, vec![3]),
            (0, 4, vec![]),
            (3, 0, vec![1, 1, 1]),
        ] {
            let ids: Vec<RepoId> = (0..count).map(RepoId).collect();
            let batches = batch_repos_for_refresh(&ids, concurrency);
            assert_eq!(
                batches.iter().map(Vec::len).collect::<Vec<_>>(),
                expected,
                "{count} ids at a concurrency of {concurrency}"
            );
            let flattened: Vec<RepoId> = batches.into_iter().flatten().collect();
            assert_eq!(
                flattened, ids,
                "every id must appear exactly once, in its original relative order"
            );
        }
    }
}
