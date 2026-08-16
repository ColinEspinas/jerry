//! Real, on-disk persistence for agent review baselines (GitHub issue #225) - a sibling file
//! next to `settings.toml`, in the same shape `crate::work_surface::tab_order_state` and
//! `crate::sidebar::fold_state` already use.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use wt_core::review::UntrackedCoverage;

use super::state::{decode_worktree, encode_worktree, BaselineReason, ReviewBaseline};

/// The baseline file's name, resolved next to the real `settings.toml` - mirrors
/// `crate::work_surface::tab_order_state::TAB_ORDER_FILE_NAME`.
pub const REVIEW_BASELINE_FILE_NAME: &str = "review-baselines.toml";

/// The baseline file for a given real settings-file path - identical reasoning to
/// `crate::work_surface::tab_order_state::tab_order_path_for`: a test that supplies a temp-dir
/// settings path gets real, isolated persistence in that same directory, and a caller with no
/// settings path gets none at all.
pub fn review_baseline_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(REVIEW_BASELINE_FILE_NAME),
        None => PathBuf::from(REVIEW_BASELINE_FILE_NAME),
    }
}

/// One persisted baseline. Every field is a real, already-computed fact - nothing here is
/// derived at read time, so a future reader (GitHub issue #227) gets exactly what was captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedBaseline {
    /// The worktree this agent was running in, in `super::state::encode_worktree`'s lossless
    /// `utf8:`/`bytes:` form - recorded separately from the map key (which also encodes it) so a
    /// reader never has to parse the key apart, and losslessly so it can be turned back into a
    /// real `PathBuf` (`Self::worktree_path`) rather than a `Path::display()` string that cannot
    /// round-trip.
    pub worktree: String,
    /// `AgentKind::label`'s output, e.g. `"Claude"`.
    pub kind: String,
    /// The wall-clock second the agent was spawned in - the durable half of its identity.
    pub spawned_at_unix: i64,
    /// The real hex tree id of the snapshot.
    pub tree_id: String,
    /// The `refs/jerry/review/*` ref anchoring `tree_id`.
    pub ref_name: String,
    /// When the snapshot itself was really taken (not when the agent spawned - see
    /// [`ReviewBaseline::taken_at_unix`]).
    pub taken_at_unix: i64,
    /// [`BaselineReason::as_key`]'s stable string.
    pub reason: String,
    /// `true` when this baseline covers tracked files only, because the worktree's untracked set
    /// was too large to hash (`wt_core::review::MAX_UNTRACKED_SNAPSHOT_BYTES`). Persisted because
    /// every later diff against this tree must use the *same* coverage to produce a correct
    /// answer - a reader that assumed the default would report every untracked file as new.
    pub tracked_only: bool,
}

impl PersistedBaseline {
    /// This record's worktree as a real path - `None` for an entry this app didn't write (see
    /// `super::state::decode_worktree`).
    pub fn worktree_path(&self) -> Option<PathBuf> {
        decode_worktree(&self.worktree)
    }
}

impl Default for PersistedBaseline {
    fn default() -> Self {
        Self {
            worktree: String::new(),
            kind: String::new(),
            spawned_at_unix: 0,
            tree_id: String::new(),
            ref_name: String::new(),
            taken_at_unix: 0,
            // Not `String::new()`: `#[serde(default)]` fills this in for a hand-written entry
            // that omits it, and an empty string would fail `BaselineReason::from_key` and throw
            // the whole otherwise-valid entry away. `Spawn` is the honest default - it's what a
            // baseline is until someone marks it reviewed.
            reason: BaselineReason::Spawn.as_key().to_string(),
            // The overwhelmingly common case, and the one a pre-`tracked_only` entry was written
            // under - so an older file's entries keep meaning what they meant when written.
            tracked_only: false,
        }
    }
}

/// The whole on-disk file: every baseline this app has ever captured, keyed by
/// `super::state::baseline_key`. A `BTreeMap` so the serialized file has a stable, diffable
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewBaselineState {
    pub baselines: BTreeMap<String, PersistedBaseline>,
}

impl ReviewBaselineState {
    /// Loads `path`, falling back to empty state for *any* failure - the same "never important
    /// enough to fail startup over" rule every sibling persisted file follows.
    pub fn load_at(path: &Path) -> ReviewBaselineState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return ReviewBaselineState::default();
        };
        match toml::from_str::<ReviewBaselineState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from no recorded review baselines",
                    path.display()
                );
                ReviewBaselineState::default()
            }
        }
    }

    /// Writes `self` to `path` atomically - a process-unique sibling `*.tmp`, `sync_all`, rename,
    /// then a parent-directory sync. Copied from
    /// `crate::work_surface::tab_order_state::TabOrderState::save_at`; see
    /// `crate::sidebar::fold_state::FoldState::save_at` for the full reasoning.
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
            .unwrap_or_else(|| REVIEW_BASELINE_FILE_NAME.to_string());
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
        Ok(())
    }

    /// The app's real write path: merges only the keys this instance owns into whatever is
    /// currently on disk, then writes the result - so a second `jerry` instance (or a second
    /// window) can't erase baselines it doesn't know about.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = ReviewBaselineState::load_at(path);
            for key in owned {
                if let Some(entry) = self.baselines.get(key) {
                    merged.baselines.insert(key.clone(), entry.clone());
                }
            }
            merged.save_at(path)
        })
    }

    /// Records (or advances) `key`'s baseline.
    pub fn set(
        &mut self,
        key: String,
        worktree: &Path,
        kind: &str,
        spawned_at_unix: i64,
        baseline: &ReviewBaseline,
    ) {
        self.baselines.insert(
            key,
            PersistedBaseline {
                worktree: encode_worktree(worktree),
                kind: kind.to_string(),
                spawned_at_unix,
                tree_id: baseline.tree_id.clone(),
                ref_name: baseline.ref_name.clone(),
                taken_at_unix: baseline.taken_at_unix,
                reason: baseline.reason.as_key().to_string(),
                tracked_only: baseline.untracked == UntrackedCoverage::Excluded,
            },
        );
    }

    /// Drops `key`'s entry outright. Deliberately **not** wired to an agent closing (see the
    /// module docs); it exists so a real "this worktree is gone" action has a way to stop
    /// recording baselines for a path that no longer exists, and so tests can exercise removal.
    pub fn forget(&mut self, key: &str) {
        self.baselines.remove(key);
    }

    /// `key`'s recorded baseline, reconstructed into the live type - `None` if there is no entry,
    /// or if the entry is unusable (an unrecognised `reason`, or an empty tree id/ref name from a
    /// hand-edited file). An unusable entry is skipped rather than repaired with guessed values,
    /// because a baseline that says the wrong thing about what a user has already reviewed is
    /// worse than no baseline at all.
    pub fn get(&self, key: &str) -> Option<ReviewBaseline> {
        let entry = self.baselines.get(key)?;
        if entry.tree_id.is_empty() || entry.ref_name.is_empty() {
            return None;
        }
        Some(ReviewBaseline {
            tree_id: entry.tree_id.clone(),
            ref_name: entry.ref_name.clone(),
            taken_at_unix: entry.taken_at_unix,
            reason: BaselineReason::from_key(&entry.reason)?,
            untracked: if entry.tracked_only {
                UntrackedCoverage::Excluded
            } else {
                UntrackedCoverage::Included
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(reason: BaselineReason) -> ReviewBaseline {
        ReviewBaseline {
            tree_id: "a".repeat(40),
            ref_name: "refs/jerry/review/6b6579".to_string(),
            taken_at_unix: 1_700_000_042,
            reason,
            untracked: UntrackedCoverage::Included,
        }
    }

    #[test]
    fn the_baseline_file_lives_next_to_the_real_settings_file() {
        assert_eq!(
            review_baseline_path_for(Path::new("/home/someone/.config/jerry/settings.toml")),
            PathBuf::from("/home/someone/.config/jerry/review-baselines.toml")
        );
    }

    #[test]
    fn a_recorded_baseline_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");
        let mut state = ReviewBaselineState::default();
        state.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1_700_000_000,
            &sample(BaselineReason::MarkedReviewed),
        );
        state.save_at(&path).expect("save");

        let reloaded = ReviewBaselineState::load_at(&path);
        assert_eq!(reloaded, state);
        assert_eq!(
            reloaded.get("key-a"),
            Some(sample(BaselineReason::MarkedReviewed)),
            "every field of a real baseline must survive the round trip, including why it was \
             taken - that's what the header wording is derived from"
        );
    }

    #[test]
    fn an_unknown_key_has_no_baseline() {
        assert_eq!(ReviewBaselineState::default().get("never-seen"), None);
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            ReviewBaselineState::load_at(&dir.path().join("nope.toml")),
            ReviewBaselineState::default()
        );

        let path = dir.path().join("review-baselines.toml");
        std::fs::write(&path, "not valid toml {{{").expect("write");
        assert_eq!(
            ReviewBaselineState::load_at(&path),
            ReviewBaselineState::default()
        );
    }

    #[test]
    fn an_entry_with_an_unrecognised_reason_is_skipped_rather_than_guessed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");
        std::fs::write(
            &path,
            "[baselines.\"key-a\"]\nworktree = \"/repo/wt-a\"\nkind = \"Claude\"\n\
             spawned_at_unix = 1\ntree_id = \"aaaa\"\nref_name = \"refs/jerry/review/6b\"\n\
             taken_at_unix = 2\nreason = \"pty-went-quiet\"\n",
        )
        .expect("write");

        let state = ReviewBaselineState::load_at(&path);
        assert!(
            state.baselines.contains_key("key-a"),
            "the raw entry is still parsed and preserved on disk"
        );
        assert_eq!(
            state.get("key-a"),
            None,
            "but it must not be handed back as a usable baseline"
        );
    }

    #[test]
    fn an_entry_missing_its_tree_or_ref_is_not_usable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");
        std::fs::write(
            &path,
            "[baselines.\"key-a\"]\nworktree = \"/repo/wt-a\"\nkind = \"Claude\"\n\
             spawned_at_unix = 1\ntree_id = \"\"\nref_name = \"refs/jerry/review/6b\"\n\
             taken_at_unix = 2\nreason = \"spawn\"\n",
        )
        .expect("write");
        assert_eq!(ReviewBaselineState::load_at(&path).get("key-a"), None);
    }

    #[test]
    fn saving_merges_with_another_instances_entries_instead_of_erasing_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");

        let mut instance_a = ReviewBaselineState::default();
        instance_a.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &sample(BaselineReason::Spawn),
        );
        let owned_a: BTreeSet<String> = ["key-a".to_string()].into_iter().collect();
        instance_a.save_merged_at(&path, &owned_a).expect("save a");

        let mut instance_b = ReviewBaselineState::default();
        instance_b.set(
            "key-b".to_string(),
            Path::new("/repo/wt-b"),
            "Codex",
            2,
            &sample(BaselineReason::Spawn),
        );
        let owned_b: BTreeSet<String> = ["key-b".to_string()].into_iter().collect();
        instance_b.save_merged_at(&path, &owned_b).expect("save b");

        let on_disk = ReviewBaselineState::load_at(&path);
        assert!(on_disk.get("key-a").is_some(), "A's baseline must survive");
        assert!(on_disk.get("key-b").is_some());
    }

    #[test]
    fn a_previous_runs_baseline_survives_a_save_that_does_not_know_about_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");

        let mut previous_run = ReviewBaselineState::default();
        previous_run.set(
            "old-key".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &sample(BaselineReason::MarkedReviewed),
        );
        previous_run.save_at(&path).expect("save");

        // A fresh launch: a brand new agent, a brand new key, and no knowledge of `old-key` at
        // all - exactly what every restart looks like today (see the module docs).
        let mut this_run = ReviewBaselineState::default();
        this_run.set(
            "new-key".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            9,
            &sample(BaselineReason::Spawn),
        );
        let owned: BTreeSet<String> = ["new-key".to_string()].into_iter().collect();
        this_run.save_merged_at(&path, &owned).expect("save");

        let on_disk = ReviewBaselineState::load_at(&path);
        assert!(
            on_disk.get("old-key").is_some(),
            "a baseline from a previous run must not be destroyed just because this run has no \
             live agent for it - that is exactly the data GitHub issue #227 needs to find"
        );
        assert!(on_disk.get("new-key").is_some());
    }

    #[test]
    fn a_persisted_worktree_round_trips_back_to_a_real_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");
        let worktree = Path::new("/repo/my worktree/feature-x");
        let mut state = ReviewBaselineState::default();
        state.set(
            "key-a".to_string(),
            worktree,
            "Claude",
            1,
            &sample(BaselineReason::Spawn),
        );
        state.save_at(&path).expect("save");

        let entry = ReviewBaselineState::load_at(&path).baselines["key-a"].clone();
        assert_eq!(
            entry.worktree_path(),
            Some(worktree.to_path_buf()),
            "the recorded worktree must decode back to the real path it was captured for"
        );
    }

    #[test]
    fn a_tracked_only_baseline_persists_its_narrower_coverage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");
        let tracked_only = ReviewBaseline {
            untracked: UntrackedCoverage::Excluded,
            ..sample(BaselineReason::Spawn)
        };
        let mut state = ReviewBaselineState::default();
        state.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &tracked_only,
        );
        state.save_at(&path).expect("save");

        assert_eq!(
            ReviewBaselineState::load_at(&path).get("key-a"),
            Some(tracked_only),
            "a tracked-only baseline must not silently come back as a full one"
        );
    }

    #[test]
    fn an_entry_without_the_coverage_field_defaults_to_full_coverage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("review-baselines.toml");
        std::fs::write(
            &path,
            "[baselines.\"key-a\"]\nworktree = \"utf8:/repo/wt-a\"\nkind = \"Claude\"\n\
             spawned_at_unix = 1\ntree_id = \"aaaa\"\nref_name = \"refs/jerry/review/6b\"\n\
             taken_at_unix = 2\nreason = \"spawn\"\n",
        )
        .expect("write");

        let baseline = ReviewBaselineState::load_at(&path)
            .get("key-a")
            .expect("an entry missing only the new field must still be usable");
        assert_eq!(baseline.untracked, UntrackedCoverage::Included);
    }

    #[test]
    fn forgetting_a_key_really_removes_it() {
        let mut state = ReviewBaselineState::default();
        state.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &sample(BaselineReason::Spawn),
        );
        assert!(state.get("key-a").is_some());
        state.forget("key-a");
        assert_eq!(state.get("key-a"), None);
    }

    #[test]
    fn advancing_a_baseline_replaces_the_recorded_entry_in_place() {
        let mut state = ReviewBaselineState::default();
        state.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &sample(BaselineReason::Spawn),
        );
        let advanced = ReviewBaseline {
            tree_id: "b".repeat(40),
            taken_at_unix: 1_700_000_999,
            reason: BaselineReason::MarkedReviewed,
            ..sample(BaselineReason::Spawn)
        };
        state.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &advanced,
        );

        assert_eq!(state.baselines.len(), 1, "advancing must not add a row");
        assert_eq!(state.get("key-a"), Some(advanced));
    }

    #[test]
    fn saving_leaves_no_temp_file_behind_and_the_result_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("review-baselines.toml");
        let mut state = ReviewBaselineState::default();
        state.set(
            "key-a".to_string(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            &sample(BaselineReason::Spawn),
        );
        state.save_at(&path).expect("save");

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
        assert_eq!(siblings, vec!["review-baselines.toml".to_string()]);
        assert!(ReviewBaselineState::load_at(&path).get("key-a").is_some());
    }
}
