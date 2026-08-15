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
//! **Originally, no UI read this back** - browsing and resuming past agents was left as issue
//! #227's job, deliberately out of this phase's scope, so the module existed purely so that work
//! would have real data to build on rather than having to invent a capture mechanism first. Issue
//! #227's read side is [`crate::hooks::history`] (the plain `AgentStatusState` -> UI-facing
//! `PastAgent` list) and the rail's own history rows - this module remains the write/persistence
//! half only, unchanged in shape by that later work.
//!
//! ## Everything about the file format mirrors `crate::review::baseline_state`
//!
//! Same directory (a sibling of the real `settings.toml`), same `BTreeMap` for a stable diffable
//! file, same lossless `utf8:`/`bytes:` worktree encoding, same atomic write (temp sibling,
//! `sync_all`, rename, directory sync), and the same merge-on-save under
//! `crate::persisted_state_lock::with_locked_merge` so a second Jerry instance can't erase this
//! one's records. The keys are even the same [`crate::review::state::baseline_key`] values, so
//! [`crate::hooks::history`] can join a persisted status straight onto its review baseline without
//! a translation table.
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

/// How many agent records are kept - see [`AgentStatusState::prune_to_most_recent`].
///
/// Generous for a history UI (issue #227) while keeping the file small enough that rewriting it
/// on every status change stays cheap.
pub const MAX_RECORDED_AGENTS: usize = 500;

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
    /// The real Claude Code `session_id` this agent's hooks last reported (GitHub issue #227) -
    /// see `crate::hooks::event::HookReport::session_id` for what it is and how it was verified.
    /// This is what makes a literal `claude --resume <session_id>` possible; `None` for a Codex
    /// agent (no hooks exist for it at all) or a Claude agent whose hooks never fired before this
    /// field started being captured.
    pub session_id: Option<String>,
    /// The run's **title** - the first prompt its human typed, off a real `UserPromptSubmit`
    /// payload (GitHub issue #227, `crate::hooks::event::HookReport::prompt`).
    ///
    /// `None` for a run whose hooks never caught one, which History renders as the honest
    /// fallback rather than inventing a task description - see
    /// `crate::run_history::model::RunRecord::title_line`.
    pub title: Option<String>,
    /// How many turns this run really completed - one per `Stop`
    /// (`crate::hooks::server::HookRecord::turns`). `0` for a run that ended inside its first
    /// turn, and for every record written before this field existed; the reader treats zero as
    /// "not known" rather than printing `0 turns`.
    #[serde(default)]
    pub turns: u32,
    /// When this run really **ended**, as seconds since the Unix epoch - set once, by
    /// `crate::run_history::flow::AdeApp::finish_run_record`, at the moment the agent's pane was
    /// actually closed in this app.
    ///
    /// This is the field that distinguishes the two genuinely different endings a record can
    /// have, and the whole reason `abandoned` is a real outcome rather than a guess: `Some` means
    /// Jerry watched the run end and knows how it ended; `None` means the record simply stopped
    /// being updated - the app quit, the machine slept, the hooks stopped firing - and nobody
    /// ever saw it finish. See `crate::run_history::model::Outcome::of`.
    pub ended_at_unix: Option<i64>,
    /// The real review diffstat measured against this run's own baseline at the moment it ended
    /// (`wt_core::review::diff_against_tree` through
    /// `crate::run_history::flow::AdeApp::finish_run_record`) - what *this run* changed, not what
    /// the worktree currently contains.
    ///
    /// All three are `Some` together or `None` together: they come from one measurement, and a
    /// header printing a file count beside a blank diffstat would be reporting half of one fact.
    /// `None` whenever that measurement could not be made - no baseline was captured, or the
    /// `git diff` failed - which the transcript header renders by omitting the diffstat rather
    /// than by printing zeros.
    pub files_changed: Option<u32>,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
}

/// One live agent's currently-known state, as [`AgentStatusState::set`] takes it.
///
/// A struct rather than nine positional parameters (which is what this was, complete with an
/// `#[allow(clippy::too_many_arguments)]`): GitHub issue #227 added two more fields to it, and at
/// eleven positional arguments - four of them `Option<String>` - a caller swapping `activity` and
/// `question`, or `title` and `session_id`, would compile silently and write the wrong thing to
/// disk forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun<'a> {
    pub worktree: &'a Path,
    /// `crate::work_surface::agents::AgentKind::label`.
    pub kind: &'a str,
    pub spawned_at_unix: i64,
    pub status: Status,
    pub activity: Option<String>,
    pub question: Option<String>,
    pub session_id: Option<String>,
    /// See [`PersistedAgentStatus::title`].
    pub title: Option<String>,
    /// See [`PersistedAgentStatus::turns`].
    pub turns: u32,
}

impl<'a> LiveRun<'a> {
    /// A run described by only the four things every recordable agent always has: where it runs,
    /// what it is, when it started, and what it is doing now. Everything else is optional
    /// hook-derived detail, added through the builders below.
    pub fn new(worktree: &'a Path, kind: &'a str, spawned_at_unix: i64, status: Status) -> Self {
        LiveRun {
            worktree,
            kind,
            spawned_at_unix,
            status,
            activity: None,
            question: None,
            session_id: None,
            title: None,
            turns: 0,
        }
    }

    pub fn activity(mut self, activity: impl Into<String>) -> Self {
        self.activity = Some(activity.into());
        self
    }

    pub fn question(mut self, question: impl Into<String>) -> Self {
        self.question = Some(question.into());
        self
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn turns(mut self, turns: u32) -> Self {
        self.turns = turns;
        self
    }
}

/// How a run really ended, as [`AgentStatusState::finish`] takes it (GitHub issue #227).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinishedRun {
    /// The run's status at the moment it ended - what
    /// `crate::run_history::model::Outcome::of` reads.
    pub status: Option<Status>,
    /// See [`PersistedAgentStatus::files_changed`]. All three are supplied together or not at all.
    pub files_changed: Option<u32>,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
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
            merged.prune_to_most_recent(MAX_RECORDED_AGENTS);
            merged.save_at(path)
        })
    }

    /// Records one agent's real, current hook-derived state. Returns whether anything actually
    /// changed, so a caller can skip an otherwise pointless disk write on every render.
    ///
    /// The end-of-run half of the record ([`PersistedAgentStatus::ended_at_unix`] and the
    /// diffstat) is deliberately carried over from whatever is already stored rather than reset:
    /// this function only ever describes a *live* agent, and it must never be able to un-finish a
    /// run that really finished.
    pub fn set(&mut self, key: String, run: LiveRun<'_>, now_unix: i64) -> bool {
        let existing = self.agents.get(&key);
        let record = PersistedAgentStatus {
            worktree: crate::review::state::encode_worktree(run.worktree),
            kind: run.kind.to_owned(),
            spawned_at_unix: run.spawned_at_unix,
            status: status_key(run.status).to_owned(),
            activity: run.activity,
            question: run.question,
            updated_at_unix: now_unix,
            session_id: run.session_id,
            title: run.title,
            turns: run.turns,
            ended_at_unix: existing.and_then(|existing| existing.ended_at_unix),
            files_changed: existing.and_then(|existing| existing.files_changed),
            insertions: existing.and_then(|existing| existing.insertions),
            deletions: existing.and_then(|existing| existing.deletions),
        };
        // Compare on everything except the timestamp: a status that hasn't changed must not
        // rewrite the file (and re-fsync) purely because time passed.
        let unchanged = existing.is_some_and(|existing| {
            existing.worktree == record.worktree
                && existing.kind == record.kind
                && existing.spawned_at_unix == record.spawned_at_unix
                && existing.status == record.status
                && existing.activity == record.activity
                && existing.question == record.question
                && existing.session_id == record.session_id
                && existing.title == record.title
                && existing.turns == record.turns
        });
        if unchanged {
            return false;
        }
        self.agents.insert(key, record);
        true
    }

    /// Marks an already-recorded run as really **ended**, at `ended_at_unix`, with whatever was
    /// measured about it at that moment (GitHub issue #227). Returns whether anything changed.
    ///
    /// A no-op returning `false` for a key this state has never recorded, and that is the
    /// contract, not a shortcut: [`crate::hooks::flow::AdeApp::record_agent_statuses`] only ever
    /// records agents that produced a real hook fact (see its own docs on why a status inferred
    /// from pty silence is not worth persisting), so an agent with no record here is one Jerry
    /// never knew anything real about. Inventing an entry for it at close time would put a run in
    /// History whose every field was a guess.
    pub fn finish(&mut self, key: &str, ended_at_unix: i64, run: FinishedRun) -> bool {
        let Some(record) = self.agents.get_mut(key) else {
            return false;
        };
        let mut changed = false;
        if record.ended_at_unix != Some(ended_at_unix) {
            record.ended_at_unix = Some(ended_at_unix);
            record.updated_at_unix = ended_at_unix;
            changed = true;
        }
        if let Some(status) = run.status {
            let status = status_key(status).to_owned();
            if record.status != status {
                record.status = status;
                changed = true;
            }
        }
        // The three diffstat fields move together or not at all - see
        // [`PersistedAgentStatus::files_changed`]. A `FinishedRun` carrying no measurement leaves
        // whatever was already there alone rather than blanking it.
        if run.files_changed.is_some() {
            if record.files_changed != run.files_changed
                || record.insertions != run.insertions
                || record.deletions != run.deletions
            {
                changed = true;
            }
            record.files_changed = run.files_changed;
            record.insertions = run.insertions;
            record.deletions = run.deletions;
        }
        changed
    }

    /// One recorded agent, if present and readable.
    pub fn get(&self, key: &str) -> Option<&PersistedAgentStatus> {
        self.agents.get(key)
    }

    /// Keeps only the `limit` most recently updated records, dropping the oldest.
    ///
    /// This file is history and never deletes a key on its own (see the module docs), which on its
    /// own means unbounded growth: every agent ever run adds a permanent entry, and the whole file
    /// is re-serialised and `fsync`ed - under the process-wide persistence lock - on every change.
    /// A user who runs a few dozen agents a day would be rewriting an ever-larger file forever.
    ///
    /// Trimming by `updated_at_unix` keeps exactly what GitHub issue #227 would want to show
    /// first - the most recent agents - and discards the tail no history UI would realistically
    /// page back to. Applied at save time rather than on insert so a merge that pulls in another
    /// instance's records is bounded too.
    pub fn prune_to_most_recent(&mut self, limit: usize) {
        if self.agents.len() <= limit {
            return;
        }
        let mut by_recency: Vec<(String, i64)> = self
            .agents
            .iter()
            .map(|(key, record)| (key.clone(), record.updated_at_unix))
            .collect();
        // Most recent first; the key breaks ties so the result is deterministic rather than
        // dependent on iteration order for records written in the same second.
        by_recency.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (key, _) in by_recency.into_iter().skip(limit) {
            self.agents.remove(&key);
        }
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
            LiveRun::new(
                Path::new("/repo/wt-a"),
                "Claude",
                1_700_000_000,
                Status::Review
            )
            .activity("Edit: src/auth.rs".to_owned())
            .session_id("5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned()),
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
        assert_eq!(
            record.session_id.as_deref(),
            Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c")
        );
    }

    #[test]
    fn an_unchanged_status_reports_no_change_so_it_never_rewrites_the_file() {
        // `set` is called from a render-frequency path; without this it would fsync constantly.
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 10);
        let run = || {
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 10, Status::Run)
                .activity("Bash: cargo test")
        };
        assert!(
            state.set(key.clone(), run(), 100),
            "the first record is a real change"
        );
        assert!(
            !state.set(key.clone(), run(), 999),
            "only the timestamp differs - this must not count as a change"
        );

        // A real status change does count.
        let key2 = key.clone();
        assert!(state.set(
            key2,
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 10, Status::Review)
                .activity("Bash: cargo test".to_owned()),
            1000,
        ));
    }

    #[test]
    fn a_newly_reported_session_id_counts_as_a_real_change() {
        // Not just timestamps: an agent's hooks may only report a `session_id` after this record
        // already exists (its first-ever hook could be `PostToolUseFailure`, which carries none -
        // see `crate::hooks::event::parse`), and that must not be treated as a no-op write.
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 20);
        assert!(state.set(
            key.clone(),
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 20, Status::Run),
            100,
        ));
        assert!(
            state.set(
                key.clone(),
                LiveRun::new(Path::new("/repo/wt-a"), "Claude", 20, Status::Run)
                    .session_id("5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned()),
                101,
            ),
            "a real session id appearing where there was none must count as a change"
        );
        assert_eq!(
            state.get(&key).and_then(|record| record.session_id.clone()),
            Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned())
        );
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
            LiveRun::new(Path::new("/repo/other"), "Claude", 1, Status::Run),
            5,
        );
        other
            .save_merged_at(&path, &std::iter::once(other_key.clone()).collect())
            .expect("save");

        let mine_key = key_for("/repo/mine", 2);
        let mut mine = AgentStatusState::default();
        mine.set(
            mine_key.clone(),
            LiveRun::new(Path::new("/repo/mine"), "Claude", 2, Status::Ask)
                .question("Bash needs permission: rm -rf /".to_owned()),
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
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 3, Status::Review),
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

    /// GitHub issue #227: the end-of-run half of a record survives a real file round trip, and
    /// carries exactly what was measured - no fabricated zeros for what was not.
    #[test]
    fn a_finished_run_really_round_trips_with_its_ending_and_its_diffstat() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(AGENT_STATUS_FILE_NAME);
        let key = key_for("/repo/wt-a", 1_700_000_000);

        let mut state = AgentStatusState::default();
        state.set(
            key.clone(),
            LiveRun::new(
                Path::new("/repo/wt-a"),
                "Claude",
                1_700_000_000,
                Status::Run,
            )
            .title("Reproduce the refresh race in a test")
            .turns(9),
            1_700_000_400,
        );
        assert!(
            state.finish(
                &key,
                1_700_000_960,
                FinishedRun {
                    status: Some(Status::Review),
                    files_changed: Some(2),
                    insertions: Some(41),
                    deletions: Some(0),
                },
            ),
            "recording a real ending is a real change"
        );

        state
            .save_merged_at(&path, &std::iter::once(key.clone()).collect())
            .expect("must save");
        let record = AgentStatusState::load_at(&path)
            .get(&key)
            .cloned()
            .expect("the record must survive");

        assert_eq!(
            record.title.as_deref(),
            Some("Reproduce the refresh race in a test")
        );
        assert_eq!(record.turns, 9);
        assert_eq!(record.ended_at_unix, Some(1_700_000_960));
        assert_eq!(
            record.updated_at_unix, 1_700_000_960,
            "the record's last-updated moment is when it ended"
        );
        assert_eq!(
            status_from_key(&record.status),
            Some(Status::Review),
            "the ending status replaces the last live one"
        );
        assert_eq!(record.files_changed, Some(2));
        assert_eq!(record.insertions, Some(41));
        assert_eq!(record.deletions, Some(0));
    }

    /// The contract that keeps History honest: an agent Jerry never learned anything real about
    /// gets no record at close time either - `finish` refuses to invent one.
    #[test]
    fn finishing_a_run_that_was_never_recorded_invents_nothing() {
        let mut state = AgentStatusState::default();
        assert!(!state.finish(
            &key_for("/repo/wt-a", 1),
            1_700_000_000,
            FinishedRun {
                status: Some(Status::Idle),
                ..FinishedRun::default()
            },
        ));
        assert!(
            state.agents.is_empty(),
            "no entry may be conjured for an agent that reported nothing"
        );
    }

    /// A later live poll must never be able to un-finish a run that really finished, nor lose the
    /// measurement taken when it did.
    #[test]
    fn a_later_live_record_cannot_erase_a_runs_real_ending() {
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 1);
        state.set(
            key.clone(),
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 1, Status::Run),
            100,
        );
        state.finish(
            &key,
            200,
            FinishedRun {
                status: Some(Status::Review),
                files_changed: Some(3),
                insertions: Some(10),
                deletions: Some(4),
            },
        );

        state.set(
            key.clone(),
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 1, Status::Run).activity("Bash: ls"),
            300,
        );

        let record = state.get(&key).expect("the record must still exist");
        assert_eq!(record.ended_at_unix, Some(200));
        assert_eq!(record.files_changed, Some(3));
        assert_eq!(record.insertions, Some(10));
        assert_eq!(record.deletions, Some(4));
    }

    /// A file written before GitHub issue #227 added these fields must still load, with the new
    /// fields reading as "not known" rather than as zeros that would render as real facts.
    #[test]
    fn a_file_written_before_the_run_fields_existed_still_loads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(AGENT_STATUS_FILE_NAME);
        let key = key_for("/repo/wt-a", 1_700_000_000);
        let legacy = format!(
            "[agents.\"{}\"]\n\
             worktree = \"utf8:/repo/wt-a\"\n\
             kind = \"Claude\"\n\
             spawned_at_unix = 1700000000\n\
             status = \"idle\"\n\
             updated_at_unix = 1700000500\n",
            key.replace('\\', "\\\\").replace('"', "\\\"")
        );
        std::fs::write(&path, legacy).expect("write the legacy file");

        let record = AgentStatusState::load_at(&path)
            .get(&key)
            .cloned()
            .expect("a pre-#227 record must still load");
        assert_eq!(record.title, None);
        assert_eq!(record.turns, 0);
        assert_eq!(record.ended_at_unix, None);
        assert_eq!(record.files_changed, None);
    }

    #[test]
    fn the_record_is_capped_keeping_the_most_recent_agents() {
        // This file never deletes a key on its own (it is history), which without a cap means it
        // grows forever and is fully re-serialised and fsynced on every change.
        let mut state = AgentStatusState::default();
        for index in 0..(MAX_RECORDED_AGENTS as i64 + 25) {
            let key = key_for(&format!("/repo/wt-{index}"), index);
            state.set(
                key,
                LiveRun::new(
                    Path::new(&format!("/repo/wt-{index}")),
                    "Claude",
                    index,
                    Status::Idle,
                ),
                index, // updated_at_unix ascending, so higher index == more recent
            );
        }
        assert!(state.agents.len() > MAX_RECORDED_AGENTS);
        state.prune_to_most_recent(MAX_RECORDED_AGENTS);
        assert_eq!(state.agents.len(), MAX_RECORDED_AGENTS);

        // The most recent survive; the oldest are the ones dropped.
        let newest = key_for(
            &format!("/repo/wt-{}", MAX_RECORDED_AGENTS as i64 + 24),
            MAX_RECORDED_AGENTS as i64 + 24,
        );
        assert!(
            state.get(&newest).is_some(),
            "the newest record must be kept"
        );
        assert!(
            state.get(&key_for("/repo/wt-0", 0)).is_none(),
            "the oldest record must be the one pruned"
        );
    }

    #[test]
    fn pruning_a_file_already_under_the_cap_changes_nothing() {
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 1);
        state.set(
            key.clone(),
            LiveRun::new(Path::new("/repo/wt-a"), "Claude", 1, Status::Run),
            10,
        );
        let before = state.clone();
        state.prune_to_most_recent(MAX_RECORDED_AGENTS);
        assert_eq!(state, before);
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
