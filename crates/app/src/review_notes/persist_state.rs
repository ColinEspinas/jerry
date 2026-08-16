//! `review-notes.toml` - the sibling file that makes a review note survive scrolling past it,
//! closing the file, and closing the app.

use super::{NoteAnchor, NoteStore, ReviewNote};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};

/// The file's own name, next to `settings.toml`.
pub const REVIEW_NOTES_FILE_NAME: &str = "review-notes.toml";

/// Where the notes file lives, given where this instance's real `settings.toml` is.
pub fn review_notes_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(REVIEW_NOTES_FILE_NAME),
        None => PathBuf::from(REVIEW_NOTES_FILE_NAME),
    }
}

/// The whole file: worktree key -> its files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewNotesState {
    pub worktrees: BTreeMap<String, PersistedWorktree>,
}

/// One worktree's files, keyed by `/`-joined relative path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedWorktree {
    pub paths: BTreeMap<String, PersistedFile>,
}

/// One file's notes, keyed by [`NoteAnchor::encode`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedFile {
    pub notes: BTreeMap<String, PersistedNote>,
}

/// One note.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedNote {
    pub text: String,
    /// Absent for a note that has never been delivered. `Option`, not an empty string: "never
    /// sent" and "sent while empty" would otherwise be the same record, and only one of them is
    /// reachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<String>,
}

impl ReviewNotesState {
    /// Reads the file, or an empty state - a missing file is the ordinary first-run case, and a
    /// corrupt one costs the notes rather than the launch.
    pub fn load_at(path: &Path) -> ReviewNotesState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return ReviewNotesState::default();
        };
        match toml::from_str::<ReviewNotesState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from no saved review notes",
                    path.display()
                );
                ReviewNotesState::default()
            }
        }
    }

    /// Writes `self` to `path` atomically - a process-unique sibling `*.tmp`, `sync_all`, rename,
    /// then a parent-directory sync. Same as
    /// `crate::provenance::persist_state::LineProvenanceState::save_at`; see
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
            .unwrap_or_else(|| REVIEW_NOTES_FILE_NAME.to_string());
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

    /// The app's real write path: replaces only the worktree keys this window has actually
    /// touched, so a second window reviewing a different worktree cannot erase notes it knows
    /// nothing about.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = ReviewNotesState::load_at(path);
            for key in owned {
                match self.worktrees.get(key) {
                    Some(entry) => merged.worktrees.insert(key.clone(), entry.clone()),
                    None => merged.worktrees.remove(key),
                };
            }
            merged.save_at(path)
        })
    }

    /// Snapshots a live store into persistable form.
    pub fn capture(store: &NoteStore) -> ReviewNotesState {
        let mut state = ReviewNotesState::default();
        for worktree in store.worktrees() {
            let Some(files) = store.files(worktree) else {
                continue;
            };
            let mut paths: BTreeMap<String, PersistedFile> = BTreeMap::new();
            for (relative, notes) in files {
                let Some(key) = relative_key(relative) else {
                    continue;
                };
                let mut persisted: BTreeMap<String, PersistedNote> = BTreeMap::new();
                for (anchor, note) in notes {
                    if note.is_blank() {
                        continue;
                    }
                    persisted.insert(
                        anchor.encode(),
                        PersistedNote {
                            text: note.text.clone(),
                            sent: note.sent.clone(),
                        },
                    );
                }
                if persisted.is_empty() {
                    continue;
                }
                paths.insert(key, PersistedFile { notes: persisted });
            }
            if paths.is_empty() {
                continue;
            }
            state.worktrees.insert(
                crate::review::state::encode_worktree(worktree),
                PersistedWorktree { paths },
            );
        }
        state
    }

    /// Loads this state into a live store. Returns `(restored, discarded)` - a hand-edited or
    /// future-version file costs the rows this build cannot read, never the launch.
    pub fn restore_into(&self, store: &mut NoteStore) -> (usize, usize) {
        let mut restored = 0usize;
        let mut discarded = 0usize;
        for (worktree_key, worktree) in &self.worktrees {
            let Some(worktree_path) = crate::review::state::decode_worktree(worktree_key) else {
                discarded += worktree
                    .paths
                    .values()
                    .map(|file| file.notes.len())
                    .sum::<usize>();
                continue;
            };
            for (path_key, file) in &worktree.paths {
                let Some(relative) = path_from_key(path_key) else {
                    discarded += file.notes.len();
                    continue;
                };
                for (anchor_key, note) in &file.notes {
                    let Some(anchor) = NoteAnchor::decode(anchor_key) else {
                        discarded += 1;
                        continue;
                    };
                    if note.text.trim().is_empty() {
                        discarded += 1;
                        continue;
                    }
                    store.restore(
                        &worktree_path,
                        &relative,
                        anchor,
                        ReviewNote {
                            text: note.text.clone(),
                            sent: note.sent.clone(),
                        },
                    );
                    restored += 1;
                }
            }
        }
        (restored, discarded)
    }
}

/// A worktree-relative path as a `/`-joined key. `None` for anything that is not a plain relative
/// path, which is the write-side half of the traversal guard below.
fn relative_key(relative: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The read-side inverse, with the traversal guard a hand-editable file needs - identical to
/// `crate::provenance::persist_state`'s own, and load-bearing for the same reason: a key naming
/// `..` must never resolve to something outside the worktree it is filed under.
fn path_from_key(key: &str) -> Option<PathBuf> {
    if key.is_empty() || key.contains('\\') {
        return None;
    }
    let mut path = PathBuf::new();
    for part in key.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt() -> PathBuf {
        PathBuf::from("/repo/wt-a")
    }

    fn file() -> PathBuf {
        PathBuf::from("src/api/users.rs")
    }

    fn seeded() -> NoteStore {
        let mut store = NoteStore::default();
        store.set_text(&wt(), &file(), NoteAnchor::New(13), "needs tenant_id");
        store.set_text(&wt(), &file(), NoteAnchor::Old(7), "why did this go?");
        store.mark_sent(&wt(), &file());
        store.set_text(
            &wt(),
            &file(),
            NoteAnchor::New(5),
            "drops the page argument",
        );
        store
    }

    #[test]
    fn a_store_round_trips_through_a_real_file_with_its_draft_and_sent_states_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = review_notes_path_for(&dir.path().join("settings.toml"));

        ReviewNotesState::capture(&seeded())
            .save_at(&path)
            .expect("save");
        assert!(path.exists(), "a real file, next to settings.toml");

        let mut restored = NoteStore::default();
        let (ok, dropped) = ReviewNotesState::load_at(&path).restore_into(&mut restored);
        assert_eq!((ok, dropped), (3, 0));
        assert_eq!(
            restored,
            seeded(),
            "including which notes had been delivered and exactly what they said when they were"
        );
        assert!(
            restored.file_state(&wt(), &file()).count == 3
                && !restored.file_state(&wt(), &file()).all_sent,
            "and the derived bar state comes back with it"
        );
    }

    #[test]
    fn a_blank_card_is_never_written_out() {
        let mut store = NoteStore::default();
        store.begin(&wt(), &file(), NoteAnchor::New(5));
        let state = ReviewNotesState::capture(&store);
        assert!(
            state.worktrees.is_empty(),
            "a worktree whose only card is blank has nothing to persist"
        );
    }

    #[test]
    fn unreadable_rows_are_discarded_rather_than_trusted() {
        let mut state = ReviewNotesState::default();
        let mut paths = BTreeMap::new();
        paths.insert(
            "../../etc/passwd".to_string(),
            PersistedFile {
                notes: BTreeMap::from([(
                    "new:1".to_string(),
                    PersistedNote {
                        text: "escape".to_string(),
                        sent: None,
                    },
                )]),
            },
        );
        paths.insert(
            "src/api/users.rs".to_string(),
            PersistedFile {
                notes: BTreeMap::from([
                    (
                        "sideways:3".to_string(),
                        PersistedNote {
                            text: "unreadable anchor".to_string(),
                            sent: None,
                        },
                    ),
                    (
                        "new:5".to_string(),
                        PersistedNote {
                            text: "readable".to_string(),
                            sent: None,
                        },
                    ),
                ]),
            },
        );
        state.worktrees.insert(
            crate::review::state::encode_worktree(&wt()),
            PersistedWorktree { paths },
        );

        let mut store = NoteStore::default();
        let (ok, dropped) = state.restore_into(&mut store);
        assert_eq!((ok, dropped), (1, 2));
        assert_eq!(store.anchors(&wt(), &file()), vec![NoteAnchor::New(5)]);
        assert!(
            store
                .files(&wt())
                .expect("the worktree")
                .keys()
                .all(|path| path == &file()),
            "the traversal key must resolve to nothing at all, not to something outside the \
             worktree"
        );
    }

    #[test]
    fn merging_leaves_another_windows_worktree_alone_and_still_lets_a_deletion_stick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = review_notes_path_for(&dir.path().join("settings.toml"));

        let mut other = NoteStore::default();
        other.set_text(
            Path::new("/repo/wt-b"),
            &file(),
            NoteAnchor::New(1),
            "another window's note",
        );
        ReviewNotesState::capture(&other)
            .save_at(&path)
            .expect("seed the other window's notes");

        let owned = BTreeSet::from([crate::review::state::encode_worktree(&wt())]);
        ReviewNotesState::capture(&seeded())
            .save_merged_at(&path, &owned)
            .expect("merge");

        let mut merged = NoteStore::default();
        ReviewNotesState::load_at(&path).restore_into(&mut merged);
        assert_eq!(merged.file_state(&wt(), &file()).count, 3);
        assert_eq!(
            merged.file_state(Path::new("/repo/wt-b"), &file()).count,
            1,
            "the other window's worktree is untouched"
        );

        ReviewNotesState::capture(&NoteStore::default())
            .save_merged_at(&path, &owned)
            .expect("merge an emptied store");
        let mut after = NoteStore::default();
        ReviewNotesState::load_at(&path).restore_into(&mut after);
        assert_eq!(after.file_state(&wt(), &file()).count, 0);
        assert_eq!(after.file_state(Path::new("/repo/wt-b"), &file()).count, 1);
    }

    #[test]
    fn the_file_sits_next_to_settings_toml() {
        assert_eq!(
            review_notes_path_for(Path::new("/home/x/.config/jerry/settings.toml")),
            PathBuf::from("/home/x/.config/jerry/review-notes.toml")
        );
    }
}
