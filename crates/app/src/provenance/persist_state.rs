//! Real, on-disk persistence for line provenance (GitHub issue #284) - a sibling file next to
//! `settings.toml`, in the same shape `crate::review::baseline_state` and
//! `crate::sidebar::fold_state` already use, including their atomic write, their multi-instance
//! merge and their "a corrupt file is an empty file, never a failed startup" rule.
//!
//! ## What crosses a restart, and what deliberately does not
//!
//! Attribution is *spans*, not content. This file records, per worktree and per path, which runs
//! of lines belong to which author and what the deletion ledger says - a few dozen bytes per
//! path. It never records the file's text: that would duplicate the user's own repository into
//! `~/.config/jerry`, at a size nobody agreed to and with contents nobody expects to find there.
//!
//! The store still needs that text on the other side (it is the "before" half of the next diff -
//! see `super::store`), and it gets it from the only honest place: **the file itself**. So each
//! record carries a SHA-256 of the exact content its spans were computed against, and
//! [`LineProvenanceState::restore_into`] re-reads the file and checks it.
//!
//! ## The stale case, and why it drops rather than guesses
//!
//! If the digest does not match, the file changed while Jerry was not running. By the model's own
//! rule that is a hand edit and those lines are `you`'s - but *which* lines is genuinely
//! unanswerable, because the content the spans described is gone and nothing on disk can
//! reconstruct it. The two dishonest options are to keep the spans (attributing agent tints to
//! lines the user may have written themselves, which is the exact failure Orca's rule exists to
//! prevent) or to flip the whole file to `you` (claiming the user wrote five hundred lines they
//! did not). The record is therefore **discarded**, and the path reads as unattributed until
//! something real is recorded for it again - the checklist's "degrades honestly: lines with no
//! recorded author render unattributed rather than guessed".
//!
//! ## Format
//!
//! Author values are string tags (`you`, `agent:<key>`), not a serialized enum - the same call
//! `crate::work_surface::tab_order_state::PersistedTab::kind` and
//! `crate::hooks::store::status_key` already make, for the same reason: the on-disk format must
//! not be coupled to a Rust type this codebase renames freely, and an unrecognised tag must be
//! skippable rather than fatal.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{PathProvenance, ProvenanceStore, RemovalMark};
use super::{AgentKey, Author};
use crate::review::state::{decode_worktree, encode_worktree};

/// The provenance file's name, resolved next to the real `settings.toml` - mirrors
/// `crate::review::baseline_state::REVIEW_BASELINE_FILE_NAME`.
pub const LINE_PROVENANCE_FILE_NAME: &str = "line-provenance.toml";

/// The provenance file for a given real settings-file path - identical reasoning to
/// `crate::review::baseline_state::review_baseline_path_for`: a test that supplies a temp-dir
/// settings path gets real, isolated persistence in that same directory, and a caller with no
/// settings path gets none at all.
pub fn line_provenance_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(LINE_PROVENANCE_FILE_NAME),
        None => PathBuf::from(LINE_PROVENANCE_FILE_NAME),
    }
}

/// The whole on-disk file: every worktree this app has recorded attribution for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LineProvenanceState {
    pub worktrees: BTreeMap<String, PersistedWorktree>,
}

/// One worktree's records, keyed by worktree-relative path (`/`-joined).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedWorktree {
    pub paths: BTreeMap<String, PersistedPath>,
}

/// One path's recorded attribution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedPath {
    /// Lowercase hex SHA-256 of the exact content [`Self::spans`] describes. The record is only
    /// usable against a file that still hashes to this - see the module docs.
    pub digest: String,
    /// How many lines that content had. Redundant with the digest as a correctness check, and
    /// kept anyway because it is the one field that makes a hand-read of this file mean anything.
    pub lines: usize,
    /// `"<0-based start>:<length>:<author>"` per run of same-authored lines. Unattributed runs are
    /// simply absent, so the common case of a barely-touched file costs almost nothing.
    pub spans: Vec<String>,
    /// `"<0-based anchor>:<line count>:<author>"` per recorded deletion.
    pub removals: Vec<String>,
}

impl LineProvenanceState {
    /// Loads `path`, falling back to empty state for *any* failure - the same "never important
    /// enough to fail startup over" rule every sibling persisted file follows.
    pub fn load_at(path: &Path) -> LineProvenanceState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return LineProvenanceState::default();
        };
        match toml::from_str::<LineProvenanceState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from no recorded line provenance",
                    path.display()
                );
                LineProvenanceState::default()
            }
        }
    }

    /// Writes `self` to `path` atomically - a process-unique sibling `*.tmp`, `sync_all`, rename,
    /// then a parent-directory sync. Copied from
    /// `crate::review::baseline_state::ReviewBaselineState::save_at`; see
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
            .unwrap_or_else(|| LINE_PROVENANCE_FILE_NAME.to_string());
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

    /// The app's real write path: replaces only the worktree keys this instance owns, then writes
    /// the result - so a second `jerry` instance (or a second window) working in a different
    /// worktree cannot erase attribution it knows nothing about.
    ///
    /// A key in `owned` but absent from `self` **is** removed, unlike
    /// `ReviewBaselineState::save_merged_at`: provenance is live state describing files as they
    /// are right now, not history. A worktree this instance owns and has nothing recorded for
    /// really does have nothing recorded.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = LineProvenanceState::load_at(path);
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
    pub fn capture(store: &ProvenanceStore) -> LineProvenanceState {
        let mut state = LineProvenanceState::default();
        for (worktree, records) in store.worktrees() {
            let mut paths: BTreeMap<String, PersistedPath> = BTreeMap::new();
            for (relative, record) in records.paths() {
                let Some(key) = relative_key(relative) else {
                    continue;
                };
                let spans = encode_spans(record.author_spans());
                let removals: Vec<String> = record
                    .removals()
                    .iter()
                    .map(|mark| {
                        format!("{}:{}:{}", mark.at, mark.lines, encode_author(&mark.author))
                    })
                    .collect();
                // A path with no attributed line and no recorded deletion is the same fact as no
                // record at all, and writing it would grow the file with rows that say nothing.
                if spans.is_empty() && removals.is_empty() {
                    continue;
                }
                paths.insert(
                    key,
                    PersistedPath {
                        digest: digest_of(record.content()),
                        lines: record.line_count(),
                        spans,
                        removals,
                    },
                );
            }
            if paths.is_empty() {
                continue;
            }
            state
                .worktrees
                .insert(encode_worktree(worktree), PersistedWorktree { paths });
        }
        state
    }

    /// Rebuilds what is still true into `store`, reading each file back to prove the record still
    /// describes it. Returns `(restored, discarded)` path counts - real numbers, for the log line
    /// and for the tests that pin the stale-file rule.
    pub fn restore_into(&self, store: &mut ProvenanceStore) -> (usize, usize) {
        let mut restored = 0usize;
        let mut discarded = 0usize;

        for (worktree_key, worktree) in &self.worktrees {
            let Some(worktree_path) = decode_worktree(worktree_key) else {
                discarded += worktree.paths.len();
                continue;
            };
            for (path_key, entry) in &worktree.paths {
                let Some(relative) = path_from_key(path_key) else {
                    discarded += 1;
                    continue;
                };
                let Ok(content) = std::fs::read_to_string(worktree_path.join(&relative)) else {
                    discarded += 1;
                    continue;
                };
                if digest_of(&content) != entry.digest {
                    discarded += 1;
                    continue;
                }
                let line_count = content.lines().count();
                if line_count != entry.lines {
                    // Unreachable for a file that really hashed the same, so this is a
                    // hand-edited or truncated record rather than a stale one - refused for the
                    // same reason.
                    discarded += 1;
                    continue;
                }
                let Some(authors) = decode_spans(&entry.spans, line_count) else {
                    discarded += 1;
                    continue;
                };
                let removals = decode_removals(&entry.removals);
                store.worktree_mut(&worktree_path).insert_record(
                    relative,
                    PathProvenance::from_parts(content, authors, removals),
                );
                restored += 1;
            }
        }

        (restored, discarded)
    }
}

fn digest_of(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn encode_author(author: &Author) -> String {
    match author {
        Author::Agent(key) => format!("agent:{key}"),
        Author::You => "you".to_string(),
        Author::Unattributed => "unattributed".to_string(),
    }
}

/// `None` for a tag this build does not recognise - a record written by a future release, or a
/// hand-edited file. Skipped rather than guessed at.
fn decode_author(tag: &str) -> Option<Author> {
    match tag {
        "you" => Some(Author::You),
        "unattributed" => Some(Author::Unattributed),
        _ => tag
            .strip_prefix("agent:")
            .filter(|key| !key.is_empty())
            .map(|key| Author::Agent(AgentKey::new(key))),
    }
}

fn encode_spans(authors: &[Author]) -> Vec<String> {
    let mut spans: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < authors.len() {
        let author = &authors[index];
        let mut end = index + 1;
        while end < authors.len() && authors[end] == *author {
            end += 1;
        }
        if *author != Author::Unattributed {
            spans.push(format!("{index}:{}:{}", end - index, encode_author(author)));
        }
        index = end;
    }
    spans
}

/// `None` if any span is malformed or reaches past `line_count` - a partly-applied set of spans
/// would attribute real lines to the wrong author, which is worse than attributing none.
fn decode_spans(spans: &[String], line_count: usize) -> Option<Vec<Author>> {
    let mut authors = vec![Author::Unattributed; line_count];
    for span in spans {
        let mut parts = span.splitn(3, ':');
        let start: usize = parts.next()?.parse().ok()?;
        let length: usize = parts.next()?.parse().ok()?;
        let author = decode_author(parts.next()?)?;
        let end = start.checked_add(length)?;
        if end > line_count {
            return None;
        }
        for slot in &mut authors[start..end] {
            *slot = author.clone();
        }
    }
    Some(authors)
}

/// Unlike [`decode_spans`], a single unreadable removal is dropped rather than failing the whole
/// record: the ledger is an additive tally, so losing one entry costs one deletion its author
/// (it becomes unattributed) and leaves everything else exactly as recorded.
fn decode_removals(removals: &[String]) -> Vec<RemovalMark> {
    removals
        .iter()
        .filter_map(|removal| {
            let mut parts = removal.splitn(3, ':');
            let at: usize = parts.next()?.parse().ok()?;
            let lines: u32 = parts.next()?.parse().ok()?;
            let author = decode_author(parts.next()?)?;
            (lines > 0).then_some(RemovalMark { at, author, lines })
        })
        .collect()
}

/// A worktree-relative path as a stable `/`-joined key - the same treatment
/// `crate::sidebar::fold_state::relative_key` gives its own per-file keys.
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

/// The read-side inverse, with the traversal guard a hand-editable file needs: a key naming `..`
/// (or an absolute path, or a Windows-style separator) must never resolve to something outside
/// the worktree it is filed under.
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

    use crate::provenance::store::RecordOutcome;
    use crate::provenance::Author;

    const BEFORE: &str = "one\ntwo\nthree\nfour\nfive\n";
    const AFTER_S3: &str = "one\nTWO\nthree\nfour\n";

    fn agent() -> AgentKey {
        AgentKey::new("utf8:/repo/wt-a|Claude|1700000000")
    }

    /// A worktree with one real, really-edited file - the same `PreToolUse` / write /
    /// `PostToolUse` sequence the store sees in production.
    fn recorded_worktree() -> (tempfile::TempDir, ProvenanceStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, BEFORE).expect("seed");

        let mut store = ProvenanceStore::default();
        store.begin_agent_edit(dir.path(), &file);
        std::fs::write(&file, AFTER_S3).expect("agent write");
        assert_eq!(
            store.record_agent_edit(dir.path(), &file, &agent()),
            RecordOutcome::Attributed
        );
        (dir, store)
    }

    fn authors(store: &ProvenanceStore, worktree: &Path) -> Vec<Author> {
        store
            .worktree(worktree)
            .and_then(|records| records.get(Path::new("a.txt")))
            .map(|record| {
                (1..=record.line_count())
                    .map(|line| record.author_at(line))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_provenance_file_lives_next_to_the_real_settings_file() {
        assert_eq!(
            line_provenance_path_for(Path::new("/home/someone/.config/jerry/settings.toml")),
            PathBuf::from("/home/someone/.config/jerry/line-provenance.toml")
        );
    }

    #[test]
    fn attribution_survives_a_restart() {
        // The checklist item, end to end: record, write the real file, throw the store away, load
        // it back in a brand new one - and get the same answer per line.
        let (dir, store) = recorded_worktree();
        let before_restart = authors(&store, dir.path());
        assert_eq!(
            before_restart,
            vec![
                Author::Unattributed,
                Author::Agent(agent()),
                Author::Unattributed,
                Author::Unattributed,
            ]
        );

        let state_path = dir.path().join("state").join("line-provenance.toml");
        LineProvenanceState::capture(&store)
            .save_at(&state_path)
            .expect("save");

        let mut relaunched = ProvenanceStore::default();
        let (restored, discarded) =
            LineProvenanceState::load_at(&state_path).restore_into(&mut relaunched);
        assert_eq!((restored, discarded), (1, 0));
        assert_eq!(
            authors(&relaunched, dir.path()),
            before_restart,
            "every line's author must survive the round trip, or the gutter lies after a restart"
        );

        // And the restored record is a *usable* baseline, not just a readback: the next edit must
        // diff against it rather than re-seeding.
        std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\nFOUR\n").expect("hand edit");
        assert_eq!(
            relaunched.record_hand_edit(dir.path(), &dir.path().join("a.txt")),
            RecordOutcome::Attributed
        );
        assert_eq!(
            authors(&relaunched, dir.path()),
            vec![
                Author::Unattributed,
                Author::Agent(agent()),
                Author::Unattributed,
                Author::You,
            ]
        );
    }

    #[test]
    fn a_file_edited_while_jerry_was_not_running_is_discarded_rather_than_misattributed() {
        // The stale case. The spans describe content that is gone, and nothing on disk can say
        // which of the current lines the user wrote - so the honest answer is none of them.
        let (dir, store) = recorded_worktree();
        let state_path = dir.path().join("line-provenance.toml");
        LineProvenanceState::capture(&store)
            .save_at(&state_path)
            .expect("save");

        std::fs::write(
            dir.path().join("a.txt"),
            "one\nsomething else entirely\nthree\nfour\n",
        )
        .expect("edit while jerry was off");

        let mut relaunched = ProvenanceStore::default();
        let (restored, discarded) =
            LineProvenanceState::load_at(&state_path).restore_into(&mut relaunched);
        assert_eq!((restored, discarded), (0, 1));
        assert!(
            relaunched.is_empty(),
            "keeping the spans would tint a line the user may have written themselves - the exact \
             failure Orca's hand-edit rule exists to prevent"
        );
    }

    #[test]
    fn a_file_deleted_while_jerry_was_not_running_is_discarded_rather_than_resurrected() {
        let (dir, store) = recorded_worktree();
        let state_path = dir.path().join("line-provenance.toml");
        LineProvenanceState::capture(&store)
            .save_at(&state_path)
            .expect("save");
        std::fs::remove_file(dir.path().join("a.txt")).expect("remove");

        let mut relaunched = ProvenanceStore::default();
        assert_eq!(
            LineProvenanceState::load_at(&state_path).restore_into(&mut relaunched),
            (0, 1)
        );
        assert!(relaunched.is_empty());
    }

    #[test]
    fn the_file_never_contains_the_content_it_describes() {
        // Attribution is spans, not text: this must never become a copy of the user's repository
        // under `~/.config/jerry`.
        let (dir, store) = recorded_worktree();
        let state_path = dir.path().join("line-provenance.toml");
        LineProvenanceState::capture(&store)
            .save_at(&state_path)
            .expect("save");

        let written = std::fs::read_to_string(&state_path).expect("read back");
        for line in AFTER_S3.lines() {
            assert!(
                !written.contains(line),
                "the persisted file must not carry the file's own text, but it contains {line:?}:\n{written}"
            );
        }
        assert!(
            written.contains("agent:"),
            "it must carry the spans:\n{written}"
        );
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_empty_state_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            LineProvenanceState::load_at(&dir.path().join("nope.toml")),
            LineProvenanceState::default()
        );
        let path = dir.path().join("line-provenance.toml");
        std::fs::write(&path, "not valid toml {{{").expect("write");
        assert_eq!(
            LineProvenanceState::load_at(&path),
            LineProvenanceState::default()
        );
    }

    #[test]
    fn saving_merges_with_another_instances_worktrees_instead_of_erasing_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("line-provenance.toml");

        let mut instance_a = LineProvenanceState::default();
        instance_a.worktrees.insert(
            "utf8:/repo/wt-a".to_string(),
            PersistedWorktree {
                paths: [("a.txt".to_string(), PersistedPath::default())]
                    .into_iter()
                    .collect(),
            },
        );
        instance_a
            .save_merged_at(
                &path,
                &["utf8:/repo/wt-a".to_string()].into_iter().collect(),
            )
            .expect("save a");

        let mut instance_b = LineProvenanceState::default();
        instance_b.worktrees.insert(
            "utf8:/repo/wt-b".to_string(),
            PersistedWorktree {
                paths: [("b.txt".to_string(), PersistedPath::default())]
                    .into_iter()
                    .collect(),
            },
        );
        instance_b
            .save_merged_at(
                &path,
                &["utf8:/repo/wt-b".to_string()].into_iter().collect(),
            )
            .expect("save b");

        let on_disk = LineProvenanceState::load_at(&path);
        assert!(on_disk.worktrees.contains_key("utf8:/repo/wt-a"));
        assert!(on_disk.worktrees.contains_key("utf8:/repo/wt-b"));
    }

    #[test]
    fn an_owned_worktree_that_no_longer_has_provenance_is_really_removed() {
        // Unlike review baselines, this file is live state rather than history - see
        // `save_merged_at`'s own docs.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("line-provenance.toml");
        let mut state = LineProvenanceState::default();
        state
            .worktrees
            .insert("utf8:/repo/wt-a".to_string(), PersistedWorktree::default());
        state.save_at(&path).expect("save");

        let owned: BTreeSet<String> = ["utf8:/repo/wt-a".to_string()].into_iter().collect();
        LineProvenanceState::default()
            .save_merged_at(&path, &owned)
            .expect("save");
        assert!(LineProvenanceState::load_at(&path).worktrees.is_empty());
    }

    #[test]
    fn a_hand_edited_record_that_names_a_path_outside_its_worktree_is_refused() {
        // The traversal guard every hand-editable per-path key in this codebase carries.
        for key in [
            "../../etc/passwd",
            "/etc/passwd",
            "src/../../x",
            "",
            "src\\main.rs",
            ".",
        ] {
            assert_eq!(path_from_key(key), None, "{key} must not resolve");
        }
        assert_eq!(
            path_from_key("src/api/users.rs"),
            Some(PathBuf::from("src/api/users.rs"))
        );
        assert_eq!(
            relative_key(Path::new("src/api/users.rs")),
            Some("src/api/users.rs".to_string())
        );
    }

    #[test]
    fn an_author_tag_this_build_does_not_recognise_is_skipped_rather_than_guessed() {
        assert_eq!(decode_author("you"), Some(Author::You));
        assert_eq!(decode_author("unattributed"), Some(Author::Unattributed));
        assert_eq!(
            decode_author("agent:utf8:/repo/wt-a|Claude|17"),
            Some(Author::Agent(AgentKey::new("utf8:/repo/wt-a|Claude|17")))
        );
        for unknown in ["", "agent:", "swarm:s3", "AGENT:s3"] {
            assert_eq!(decode_author(unknown), None, "{unknown}");
        }
        // A span naming an unknown author must take the whole record down rather than leave the
        // rest of the file attributed to the wrong people.
        assert_eq!(decode_spans(&["0:2:swarm:s3".to_string()], 4), None);
        assert_eq!(
            decode_spans(&["2:9:you".to_string()], 4),
            None,
            "past the end"
        );
        assert_eq!(decode_spans(&["nonsense".to_string()], 4), None);
    }

    #[test]
    fn spans_round_trip_through_their_own_encoding() {
        let s3 = Author::Agent(agent());
        let original = vec![
            Author::Unattributed,
            s3.clone(),
            s3.clone(),
            Author::You,
            Author::Unattributed,
        ];
        let encoded = encode_spans(&original);
        assert_eq!(
            encoded,
            vec![format!("1:2:agent:{}", agent()), "3:1:you".to_string()],
            "runs are collapsed and unattributed lines cost nothing"
        );
        assert_eq!(decode_spans(&encoded, original.len()), Some(original));
    }
}
