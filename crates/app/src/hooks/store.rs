//! Real, on-disk persistence for what Jerry learned about each agent from its hooks (GitHub
//! issue #239, phase 2 - groundwork for issue #227).
//!
//! ## What this is for, and what it deliberately is not
//!
//! Issue #227 ("Agent history and resume/recover") wants to show past and closed agents and let a
//! user resume them. That needs real, structured data about agents that are no longer running -
//! and until hooks existed, Jerry had none worth persisting: a status derived from pty silence is
//! a guess, and a guess written to disk is still a guess an hour later.
//!
//! A hook fact is different. "This agent finished its turn at 14:32, its last tool call was
//! `Edit: src/auth.rs`" is a real, dated, structured statement the agent itself made. That is
//! worth keeping, so this module keeps it as it is learned.
//!
//! **No UI reads this yet, and that is intentional** - browsing and resuming past agents is issue
//! #227's job, not this phase's. This module exists so that work has real data to build on rather
//! than having to invent a capture mechanism first. It is written, and it round-trips; nothing
//! renders it.
//!
//! ## Everything about the file format mirrors `crate::review::baseline_state`
//!
//! Same directory (a sibling of the real `settings.toml`), same `BTreeMap` for a stable diffable
//! file, same lossless `utf8:`/`bytes:` worktree encoding, same atomic write (temp sibling,
//! `sync_all`, rename, directory sync), and the same merge-on-save under
//! `crate::persisted_state_lock::with_locked_merge` so a second Jerry instance can't erase this
//! one's records. The keys are even the same [`crate::review::state::baseline_key`] values, so a
//! future issue #227 implementation can join a persisted status straight onto its review
//! baseline without a translation table.
//!
//! Like `baseline_state` and unlike its other siblings, a key absent from memory is **not**
//! deleted from disk on save: this is history, and an agent the user closed is exactly the agent
//! issue #227 most wants to show.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::rail::status::Status;

/// The status file's name, resolved next to the real `settings.toml` - mirrors
/// `crate::review::baseline_state::REVIEW_BASELINE_FILE_NAME`.
pub const AGENT_STATUS_FILE_NAME: &str = "agent-status.toml";

/// The status file for a given real settings-file path - identical reasoning to
/// `crate::review::baseline_state::review_baseline_path_for`: a test supplying a temp-dir
/// settings path gets real, isolated persistence there, and a `None` settings path gets none.
pub fn agent_status_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(AGENT_STATUS_FILE_NAME),
        None => PathBuf::from(AGENT_STATUS_FILE_NAME),
    }
}

/// One agent's last known real state, as learned from its own hook events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedAgentStatus {
    /// Lossless worktree encoding - `crate::review::state::encode_worktree`.
    pub worktree: String,
    /// `crate::work_surface::agents::AgentKind::label`.
    pub kind: String,
    /// The agent's spawn second, the durable half of its identity (a live `AgentId` is
    /// process-local and means nothing after a restart).
    pub spawned_at_unix: i64,
    /// The derived [`Status`], as a stable string key rather than the enum - so a future release
    /// that adds a status can still read a file written by this one.
    pub status: String,
    /// The last hook-derived activity line, if any.
    pub activity: Option<String>,
    /// The last hook-derived question/permission text, if any.
    pub question: Option<String>,
    /// When this record was last updated.
    pub updated_at_unix: i64,
}

impl PersistedAgentStatus {
    /// The real worktree path, or `None` for a record this app didn't write.
    pub fn worktree_path(&self) -> Option<PathBuf> {
        crate::review::state::decode_worktree(&self.worktree)
    }
}

/// The stable on-disk spelling of a [`Status`].
///
/// A dedicated mapping rather than `#[derive(Serialize)]` on [`Status`] itself: the file is a
/// compatibility surface a future issue #227 release must still be able to read, and deriving it
/// would silently couple the file format to an enum this codebase renames freely.
pub fn status_key(status: Status) -> &'static str {
    match status {
        Status::Ask => "ask",
        Status::Fail => "fail",
        Status::Review => "review",
        Status::Run => "run",
        Status::Idle => "idle",
    }
}

/// The inverse of [`status_key`] - `None` for anything this release doesn't know, which a reader
/// treats as "unusable record" rather than guessing.
pub fn status_from_key(key: &str) -> Option<Status> {
    match key {
        "ask" => Some(Status::Ask),
        "fail" => Some(Status::Fail),
        "review" => Some(Status::Review),
        "run" => Some(Status::Run),
        "idle" => Some(Status::Idle),
        _ => None,
    }
}

/// The whole on-disk file: every agent this app has recorded a real hook-derived status for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentStatusState {
    pub agents: BTreeMap<String, PersistedAgentStatus>,
}

impl AgentStatusState {
    /// Loads `path`, falling back to an empty state for any failure - the same "never important
    /// enough to fail startup over" rule every sibling state file follows.
    pub fn load_at(path: &Path) -> AgentStatusState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return AgentStatusState::default();
        };
        match toml::from_str::<AgentStatusState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from no recorded agent statuses",
                    path.display()
                );
                AgentStatusState::default()
            }
        }
    }

    /// Writes `self` to `path` atomically - a sibling `*.tmp`, `sync_all`, rename, directory
    /// sync. Copied from `crate::review::baseline_state::ReviewBaselineState::save_at`, including
    /// the `{file}.{pid}.{counter}.tmp` recipe (process id *and* a process-global counter,
    /// because several `AdeApp` instances share one process).
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
            .unwrap_or_else(|| AGENT_STATUS_FILE_NAME.to_string());
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

    /// The app's real write path: merges the entries this instance owns into whatever is on disk,
    /// then writes the result.
    ///
    /// Like `ReviewBaselineState::save_merged_at`, a key absent from `self` is left alone rather
    /// than deleted - see the module docs on this being history.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = AgentStatusState::load_at(path);
            for key in owned {
                if let Some(entry) = self.agents.get(key) {
                    merged.agents.insert(key.clone(), entry.clone());
                }
            }
            merged.save_at(path)
        })
    }

    /// Records one agent's real, current hook-derived state. Returns whether anything actually
    /// changed, so a caller can skip an otherwise pointless disk write on every render.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &mut self,
        key: String,
        worktree: &Path,
        kind: &str,
        spawned_at_unix: i64,
        status: Status,
        activity: Option<String>,
        question: Option<String>,
        now_unix: i64,
    ) -> bool {
        let record = PersistedAgentStatus {
            worktree: crate::review::state::encode_worktree(worktree),
            kind: kind.to_owned(),
            spawned_at_unix,
            status: status_key(status).to_owned(),
            activity,
            question,
            updated_at_unix: now_unix,
        };
        // Compare on everything except the timestamp: a status that hasn't changed must not
        // rewrite the file (and re-fsync) purely because time passed.
        let unchanged = self.agents.get(&key).is_some_and(|existing| {
            existing.worktree == record.worktree
                && existing.kind == record.kind
                && existing.spawned_at_unix == record.spawned_at_unix
                && existing.status == record.status
                && existing.activity == record.activity
                && existing.question == record.question
        });
        if unchanged {
            return false;
        }
        self.agents.insert(key, record);
        true
    }

    /// One recorded agent, if present and readable.
    pub fn get(&self, key: &str) -> Option<&PersistedAgentStatus> {
        self.agents.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_surface::agents::AgentKind;

    fn key_for(worktree: &str, spawned: i64) -> String {
        crate::review::state::baseline_key(Path::new(worktree), AgentKind::Claude, spawned)
    }

    #[test]
    fn the_status_file_lives_next_to_the_real_settings_file() {
        assert_eq!(
            agent_status_path_for(Path::new("/home/someone/.config/jerry/settings.toml")),
            PathBuf::from("/home/someone/.config/jerry/agent-status.toml")
        );
    }

    #[test]
    fn a_recorded_status_really_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(AGENT_STATUS_FILE_NAME);

        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 1_700_000_000);
        assert!(state.set(
            key.clone(),
            Path::new("/repo/wt-a"),
            "Claude",
            1_700_000_000,
            Status::Review,
            Some("Edit: src/auth.rs".to_owned()),
            None,
            1_700_000_500,
        ));

        let owned: BTreeSet<String> = std::iter::once(key.clone()).collect();
        state.save_merged_at(&path, &owned).expect("must save");
        assert!(path.is_file(), "a real file must be written");

        let reloaded = AgentStatusState::load_at(&path);
        let record = reloaded.get(&key).expect("the record must survive");
        assert_eq!(record.worktree_path(), Some(PathBuf::from("/repo/wt-a")));
        assert_eq!(record.kind, "Claude");
        assert_eq!(record.spawned_at_unix, 1_700_000_000);
        assert_eq!(status_from_key(&record.status), Some(Status::Review));
        assert_eq!(record.activity.as_deref(), Some("Edit: src/auth.rs"));
        assert_eq!(record.question, None);
        assert_eq!(record.updated_at_unix, 1_700_000_500);
    }

    #[test]
    fn an_unchanged_status_reports_no_change_so_it_never_rewrites_the_file() {
        // `set` is called from a render-frequency path; without this it would fsync constantly.
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 10);
        let args = |now| {
            (
                key.clone(),
                Path::new("/repo/wt-a"),
                "Claude",
                10i64,
                Status::Run,
                Some("Bash: cargo test".to_owned()),
                None,
                now,
            )
        };
        let (k, w, kind, spawned, status, activity, question, now) = args(100);
        assert!(
            state.set(k, w, kind, spawned, status, activity, question, now),
            "the first record is a real change"
        );
        let (k, w, kind, spawned, status, activity, question, now) = args(999);
        assert!(
            !state.set(k, w, kind, spawned, status, activity, question, now),
            "only the timestamp differs - this must not count as a change"
        );

        // A real status change does count.
        let key2 = key.clone();
        assert!(state.set(
            key2,
            Path::new("/repo/wt-a"),
            "Claude",
            10,
            Status::Review,
            Some("Bash: cargo test".to_owned()),
            None,
            1000,
        ));
    }

    #[test]
    fn saving_merges_rather_than_clobbering_another_instances_records() {
        // The real hazard: two Jerry instances sharing one `~/.config/jerry`.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(AGENT_STATUS_FILE_NAME);

        let other_key = key_for("/repo/other", 1);
        let mut other = AgentStatusState::default();
        other.set(
            other_key.clone(),
            Path::new("/repo/other"),
            "Claude",
            1,
            Status::Run,
            None,
            None,
            5,
        );
        other
            .save_merged_at(&path, &std::iter::once(other_key.clone()).collect())
            .expect("save");

        let mine_key = key_for("/repo/mine", 2);
        let mut mine = AgentStatusState::default();
        mine.set(
            mine_key.clone(),
            Path::new("/repo/mine"),
            "Claude",
            2,
            Status::Ask,
            None,
            Some("Bash needs permission: rm -rf /".to_owned()),
            6,
        );
        mine.save_merged_at(&path, &std::iter::once(mine_key.clone()).collect())
            .expect("save");

        let merged = AgentStatusState::load_at(&path);
        assert!(
            merged.get(&other_key).is_some(),
            "the other instance's record must survive this instance's save"
        );
        assert!(merged.get(&mine_key).is_some());
    }

    #[test]
    fn a_record_absent_from_memory_is_kept_on_disk_because_this_is_history() {
        // The deliberate divergence from `fold_state`/`tab_order_state`, matching
        // `baseline_state`: a closed agent is exactly what issue #227 wants to show.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(AGENT_STATUS_FILE_NAME);
        let key = key_for("/repo/wt-a", 3);

        let mut state = AgentStatusState::default();
        state.set(
            key.clone(),
            Path::new("/repo/wt-a"),
            "Claude",
            3,
            Status::Review,
            None,
            None,
            7,
        );
        let owned: BTreeSet<String> = std::iter::once(key.clone()).collect();
        state.save_merged_at(&path, &owned).expect("save");

        // Now the agent is gone from memory, but still "owned" by this instance.
        let empty = AgentStatusState::default();
        empty.save_merged_at(&path, &owned).expect("save");

        assert!(
            AgentStatusState::load_at(&path).get(&key).is_some(),
            "a closed agent's record must not be deleted"
        );
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("nope.toml");
        assert_eq!(
            AgentStatusState::load_at(&missing),
            AgentStatusState::default()
        );

        let corrupt = dir.path().join("corrupt.toml");
        std::fs::write(&corrupt, "this is not valid toml {{{").expect("write");
        assert_eq!(
            AgentStatusState::load_at(&corrupt),
            AgentStatusState::default()
        );
    }

    #[test]
    fn status_keys_round_trip_and_cover_every_status() {
        for status in Status::ORDER {
            assert_eq!(
                status_from_key(status_key(status)),
                Some(status),
                "{status:?} must round-trip through its stable file spelling"
            );
        }
        assert_eq!(status_from_key("a_status_from_a_future_release"), None);
    }
}
