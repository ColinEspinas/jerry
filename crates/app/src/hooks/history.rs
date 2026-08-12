//! The read side of [`crate::hooks::store`] (GitHub issue #227): turns the persisted
//! [`crate::hooks::store::AgentStatusState`] into the list of past agents a UI can show.
//!
//! Phase 2 (`crate::hooks::store`'s own module docs) wrote this data and deliberately read none
//! of it back - "nothing reads `agent-status.toml` back - that's #227's job". This is that job's
//! data layer: GPUI-free, so it's directly unit-testable, and honest about the shape of what it
//! returns - every field on [`PastAgent`] traces back to a real, persisted
//! [`crate::hooks::store::PersistedAgentStatus`] field. Nothing here is invented to fill a gap in
//! what Phase 2 actually captured.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::hooks::store::{status_from_key, AgentStatusState, PersistedAgentStatus};
use crate::rail::status::Status;
use crate::work_surface::agents::AgentKind;

/// One closed (or at least not-currently-open) agent's real, persisted state - what a history UI
/// needs to render a row and, where possible, offer a real resume.
///
/// Deliberately does **not** carry a conversation title, a transcript excerpt, or anything else
/// Phase 2 never captured - see [`crate::hooks::store::PersistedAgentStatus`]'s own field list for
/// exactly what a hook payload gave Jerry to keep. [`Self::activity`]/[`Self::question`] are the
/// closest real substitute for "what was it doing" this data actually has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastAgent {
    /// The persisted record's own key (`crate::review::state::baseline_key`) - stable identity
    /// for a resume click handler to look the full record back up by.
    pub key: String,
    pub worktree: PathBuf,
    pub kind: AgentKind,
    pub spawned_at_unix: i64,
    /// This agent's last known real status before it stopped being recorded (either because it
    /// closed, or because its hooks simply stopped firing) - not necessarily "closed cleanly".
    pub status: Status,
    pub activity: Option<String>,
    pub question: Option<String>,
    /// When this record was last updated - the real "last active" timestamp a history row shows.
    pub updated_at_unix: i64,
    /// The real Claude Code `session_id` this agent's hooks last reported, if any - see
    /// [`crate::hooks::event::HookReport::session_id`] for what it is and how a real `claude
    /// --resume <session_id>` was verified to use it. `None` covers a Codex agent (no hooks exist
    /// for Codex at all) and a Claude agent that closed before any hook reported one.
    pub session_id: Option<String>,
}

impl PastAgent {
    /// Decodes one raw persisted record into a [`PastAgent`], or `None` if it isn't usable - an
    /// unencodable worktree, or a status key this build doesn't recognise (a record written by a
    /// future release). Skipping a record like this, rather than showing it wrong, mirrors
    /// `PersistedAgentStatus::worktree_path`/`status_from_key`'s own "unusable record" contract.
    fn from_record(key: &str, record: &PersistedAgentStatus) -> Option<PastAgent> {
        let worktree = record.worktree_path()?;
        let kind = AgentKind::from_label(&record.kind)?;
        let status = status_from_key(&record.status)?;
        Some(PastAgent {
            key: key.to_owned(),
            worktree,
            kind,
            spawned_at_unix: record.spawned_at_unix,
            status,
            activity: record.activity.clone(),
            question: record.question.clone(),
            updated_at_unix: record.updated_at_unix,
            session_id: record.session_id.clone(),
        })
    }
}

/// Every real, readable past agent in `state`, most recently active first. A record that fails to
/// decode is silently skipped - see [`PastAgent::from_record`].
pub fn past_agents(state: &AgentStatusState) -> Vec<PastAgent> {
    let mut agents: Vec<PastAgent> = state
        .agents
        .iter()
        .filter_map(|(key, record)| PastAgent::from_record(key, record))
        .collect();
    // Most recent first; the key breaks ties deterministically, mirroring
    // `AgentStatusState::prune_to_most_recent`'s own ordering.
    agents.sort_by(|a, b| {
        b.updated_at_unix
            .cmp(&a.updated_at_unix)
            .then_with(|| a.key.cmp(&b.key))
    });
    agents
}

/// [`past_agents`] filtered to exactly one worktree, and to records that are genuinely *past* -
/// excluding any key in `live_keys`.
///
/// That exclusion matters because a persisted record also exists for an agent that is still
/// running: `crate::hooks::flow::AdeApp::record_agent_statuses` records every agent with a fresh
/// hook fact, live ones included, not just closed ones (see that function's own module docs on
/// why "past" isn't something the store itself can decide). A still-open agent already has a
/// real, live rail row; showing it again under "History" would be the same agent rendered twice.
/// `live_keys` is `crate::review::state::baseline_key` for every currently open agent, computed by
/// the caller from live `Agent` state - this module has no notion of "currently open" itself.
pub fn past_agents_for_worktree(
    state: &AgentStatusState,
    worktree: &Path,
    live_keys: &HashSet<String>,
) -> Vec<PastAgent> {
    past_agents(state)
        .into_iter()
        .filter(|agent| agent.worktree == worktree && !live_keys.contains(&agent.key))
        .collect()
}

/// One past agent's real record, by its persisted key - the resume click handler's lookup
/// (`crate::hooks::flow::AdeApp::resume_past_agent`), so it doesn't have to re-filter/re-sort the
/// whole history just to find the one record a click named.
pub fn find(state: &AgentStatusState, key: &str) -> Option<PastAgent> {
    let record = state.get(key)?;
    PastAgent::from_record(key, record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_for(worktree: &str, spawned: i64) -> String {
        crate::review::state::baseline_key(Path::new(worktree), AgentKind::Claude, spawned)
    }

    /// The real round trip GitHub issue #227's whole read side depends on: a record written
    /// through `AgentStatusState`'s real save path, reloaded from a real file, and turned into a
    /// UI-facing `PastAgent` with every field intact.
    #[test]
    fn a_persisted_record_really_round_trips_into_a_past_agent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(crate::hooks::store::AGENT_STATUS_FILE_NAME);

        let key = key_for("/repo/wt-a", 1_700_000_000);
        let mut state = AgentStatusState::default();
        state.set(
            key.clone(),
            Path::new("/repo/wt-a"),
            "Claude",
            1_700_000_000,
            Status::Review,
            Some("Edit: src/auth.rs".to_owned()),
            None,
            Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned()),
            1_700_000_500,
        );
        state
            .save_merged_at(&path, &std::iter::once(key.clone()).collect())
            .expect("save");

        let reloaded = AgentStatusState::load_at(&path);
        let agents = past_agents(&reloaded);
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert_eq!(agent.key, key);
        assert_eq!(agent.worktree, PathBuf::from("/repo/wt-a"));
        assert_eq!(agent.kind, AgentKind::Claude);
        assert_eq!(agent.spawned_at_unix, 1_700_000_000);
        assert_eq!(agent.status, Status::Review);
        assert_eq!(agent.activity.as_deref(), Some("Edit: src/auth.rs"));
        assert_eq!(agent.question, None);
        assert_eq!(agent.updated_at_unix, 1_700_000_500);
        assert_eq!(
            agent.session_id.as_deref(),
            Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c")
        );
    }

    #[test]
    fn past_agents_are_sorted_most_recently_active_first() {
        let mut state = AgentStatusState::default();
        state.set(
            key_for("/repo/wt-old", 1),
            Path::new("/repo/wt-old"),
            "Claude",
            1,
            Status::Idle,
            None,
            None,
            None,
            100,
        );
        state.set(
            key_for("/repo/wt-new", 2),
            Path::new("/repo/wt-new"),
            "Codex",
            2,
            Status::Idle,
            None,
            None,
            None,
            999,
        );
        let agents = past_agents(&state);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].worktree, PathBuf::from("/repo/wt-new"));
        assert_eq!(agents[1].worktree, PathBuf::from("/repo/wt-old"));
    }

    #[test]
    fn a_record_with_an_unrecognised_status_key_is_skipped_not_shown_wrong() {
        // The real reason `status_from_key` returning `None` matters here: a record written by a
        // future release must not be shown with some default/guessed status.
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 1);
        state.agents.insert(
            key,
            PersistedAgentStatus {
                worktree: crate::review::state::encode_worktree(Path::new("/repo/wt-a")),
                kind: "Claude".to_owned(),
                spawned_at_unix: 1,
                status: "a_status_from_a_future_release".to_owned(),
                activity: None,
                question: None,
                updated_at_unix: 1,
                session_id: None,
            },
        );
        assert!(past_agents(&state).is_empty());
    }

    #[test]
    fn a_record_with_an_unrecognised_kind_is_skipped_not_shown_wrong() {
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 1);
        state.agents.insert(
            key,
            PersistedAgentStatus {
                worktree: crate::review::state::encode_worktree(Path::new("/repo/wt-a")),
                kind: "SomeFutureAgent".to_owned(),
                spawned_at_unix: 1,
                status: "idle".to_owned(),
                activity: None,
                question: None,
                updated_at_unix: 1,
                session_id: None,
            },
        );
        assert!(past_agents(&state).is_empty());
    }

    #[test]
    fn past_agents_for_worktree_filters_by_path_and_excludes_live_keys() {
        let mut state = AgentStatusState::default();
        let closed_key = key_for("/repo/wt-a", 1);
        let live_key = key_for("/repo/wt-a", 2);
        let other_worktree_key = key_for("/repo/wt-b", 3);
        for (key, worktree, spawned) in [
            (closed_key.clone(), "/repo/wt-a", 1),
            (live_key.clone(), "/repo/wt-a", 2),
            (other_worktree_key.clone(), "/repo/wt-b", 3),
        ] {
            state.set(
                key,
                Path::new(worktree),
                "Claude",
                spawned,
                Status::Idle,
                None,
                None,
                None,
                spawned,
            );
        }

        let live_keys: HashSet<String> = std::iter::once(live_key.clone()).collect();
        let history = past_agents_for_worktree(&state, Path::new("/repo/wt-a"), &live_keys);
        assert_eq!(history.len(), 1, "only the closed wt-a agent must show");
        assert_eq!(history[0].key, closed_key);
    }

    #[test]
    fn a_worktree_with_no_persisted_history_shows_nothing() {
        let state = AgentStatusState::default();
        assert!(
            past_agents_for_worktree(&state, Path::new("/repo/wt-a"), &HashSet::new()).is_empty()
        );
    }

    #[test]
    fn find_looks_up_one_record_by_its_real_persisted_key() {
        let mut state = AgentStatusState::default();
        let key = key_for("/repo/wt-a", 1);
        state.set(
            key.clone(),
            Path::new("/repo/wt-a"),
            "Claude",
            1,
            Status::Idle,
            None,
            None,
            Some("session-abc".to_owned()),
            10,
        );
        let found = find(&state, &key).expect("the record must be found");
        assert_eq!(found.session_id.as_deref(), Some("session-abc"));
        assert_eq!(find(&state, "no-such-key"), None);
    }
}
