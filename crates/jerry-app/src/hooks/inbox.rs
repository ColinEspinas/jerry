//! What the host's `event/hook` notifications have taught Jerry about each agent (GitHub issue
//! #239 phase 2; decision Q10, `docs/architecture/decisions.md` §19): the latest parsed fact per
//! agent, and every file write reported since the last drain. Pure in-memory bookkeeping - no
//! socket, no listener - fed by `crate::hooks::mod::HookRuntime`'s notification-consuming task.

use std::collections::{HashMap, VecDeque};

use crate::hooks::event::{self, EditedFile, HookFact, HookReport};
use crate::work_surface::agents::AgentId;

/// Most agents [`HookInbox`] will track at once - see [`HookInbox::record`].
const MAX_TRACKED_AGENTS: usize = 512;

/// How many un-drained agent edits [`EditLog`] will hold before it starts dropping the oldest.
const MAX_PENDING_EDITS: usize = 4096;

/// One agent's most recent hook fact, as stored for the rail to read, plus the two facts about
/// the *run as a whole* that a single latest-wins report structurally cannot carry (GitHub issue
/// #227).
#[derive(Debug, Clone)]
pub struct HookRecord {
    /// The parsed event - see [`crate::hooks::event::parse`].
    pub report: HookReport,
    /// When it arrived, for the freshness check in [`crate::rail::status::HookSignal`].
    pub received_at: std::time::Instant,
    /// How many turns this agent has really completed - one per `Stop`
    /// ([`HookFact::TurnEnded`]) it has reported.
    pub turns: u32,
    /// The first prompt this agent's human typed, latched once and never overwritten - the run's
    /// **title** (`crate::hooks::event::HookReport::prompt`).
    pub first_prompt: Option<String>,
}

/// What an agent's hooks have said about its **run**, as opposed to about its current state
/// (GitHub issue #227) - see [`HookRecord::turns`] and [`HookRecord::first_prompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFacts {
    /// Completed turns - one per `Stop`.
    pub turns: u32,
    /// The run's title: the first prompt its human typed, if hooks caught one.
    pub title: Option<String>,
}

/// Every agent's latest hook fact, shared between the notification-consuming task and the UI
/// thread.
#[derive(Debug, Default)]
pub struct HookInbox {
    latest: HashMap<AgentId, HookRecord>,
}

impl HookInbox {
    /// This agent's most recent hook fact, if it has ever reported one.
    pub fn get(&self, id: AgentId) -> Option<&HookRecord> {
        self.latest.get(&id)
    }

    /// Records a freshly parsed event as `id`'s current state, evicting the oldest entry if that
    /// would push the inbox past [`MAX_TRACKED_AGENTS`].
    pub fn record(&mut self, id: AgentId, report: HookReport) {
        // GitHub issue #227's run-level accumulation, taken from the *incoming* payload before
        // any nudge merging: a turn really ended if this payload said so, and the first prompt is
        // whichever `UserPromptSubmit` arrived first. Both are carried forward from whatever this
        // agent already had - unlike the report itself, neither is ever replaced by a later
        // event, and neither is subject to the TTL below (a run's turn count does not become
        // untrue because the agent went quiet for half an hour).
        let previous_run_facts = self
            .latest
            .get(&id)
            .map(|record| (record.turns, record.first_prompt.clone()));
        let (previous_turns, previous_prompt) = previous_run_facts.unwrap_or((0, None));
        let turns = previous_turns.saturating_add(u32::from(
            report.kind == event::EventKind::Transition && report.fact == HookFact::TurnEnded,
        ));
        let first_prompt = previous_prompt.or_else(|| report.prompt.clone());

        // Only a *fresh* previous record is worth folding into: past the TTL the rail has already
        // stopped believing it (see `crate::rail::status::HookSignal::fresh`), so a nudge is then
        // the only real evidence there is and must stand on its own.
        let report = match self.latest.get(&id) {
            Some(previous) if previous.received_at.elapsed() < event::HOOK_SIGNAL_TTL => {
                merge_nudge(&previous.report, report)
            }
            _ => report,
        };
        if self.latest.len() >= MAX_TRACKED_AGENTS && !self.latest.contains_key(&id) {
            if let Some(oldest) = self
                .latest
                .iter()
                .min_by_key(|(_, record)| record.received_at)
                .map(|(id, _)| *id)
            {
                self.latest.remove(&oldest);
            }
        }
        self.latest.insert(
            id,
            HookRecord {
                report,
                received_at: std::time::Instant::now(),
                turns,
                first_prompt,
            },
        );
    }

    /// Drops an agent's facts - called when the agent closes, so a recycled
    /// [`AgentId`] can never inherit a dead agent's status.
    pub fn forget(&mut self, id: AgentId) {
        self.latest.remove(&id);
    }
}

/// One agent's file write, as it came off the wire (GitHub issue #284).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEdit {
    /// Which agent - the `JERRY_AGENT_ID` the spawn set.
    pub agent: AgentId,
    /// The file and the phase - see [`crate::hooks::event::EditedFile`].
    pub file: EditedFile,
    /// The file's content as it stood when a [`crate::hooks::event::EditPhase::Before`] event
    /// arrived - always `None` for an `After` event.
    pub before: Option<String>,
}

/// Every file write no reader has taken yet, oldest first.
#[derive(Debug, Default)]
pub struct EditLog {
    pending: VecDeque<AgentEdit>,
    /// How many entries were evicted un-drained since the last drain - a real number for a real
    /// log line, so an overflow is visible rather than silent.
    dropped: usize,
}

impl EditLog {
    pub fn record(&mut self, agent: AgentId, file: EditedFile, before: Option<String>) {
        while self.pending.len() >= MAX_PENDING_EDITS {
            self.pending.pop_front();
            self.dropped += 1;
        }
        self.pending.push_back(AgentEdit {
            agent,
            file,
            before,
        });
    }

    /// Takes everything pending, in arrival order, leaving the log empty. Returns
    /// `(edits, dropped_since_last_drain)`.
    pub fn drain(&mut self) -> (Vec<AgentEdit>, usize) {
        let dropped = std::mem::take(&mut self.dropped);
        (self.pending.drain(..).collect(), dropped)
    }

    /// Drops an agent's un-drained edits - called when the agent closes, so a recycled
    /// [`AgentId`] cannot be handed a dead agent's writes.
    pub fn forget(&mut self, id: AgentId) {
        self.pending.retain(|edit| edit.agent != id);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Folds a `Notification` into the fact Jerry already holds for the same agent, returning the
/// report that should actually be stored.
pub(crate) fn merge_nudge(previous: &HookReport, incoming: HookReport) -> HookReport {
    let keep_previous = match incoming.kind {
        // Not a nudge at all.
        event::EventKind::Transition => false,
        // A block Jerry may not have heard about yet: it must still be able to move a working or
        // finished agent to "needs you". The one thing it may not do is overwrite the specific
        // question a `PermissionRequest` gave for the very block it is announcing.
        event::EventKind::BlockedNudge => previous.fact == HookFact::NeedsInput,
        // "You have not typed for a while" - true of every agent that is blocked or done, and
        // news about neither.
        event::EventKind::IdleNudge => matches!(
            previous.fact,
            HookFact::NeedsInput | HookFact::TurnEnded | HookFact::TurnFailed
        ),
    };
    if !keep_previous {
        return incoming;
    }
    HookReport {
        kind: incoming.kind,
        fact: previous.fact,
        activity: previous.activity.clone(),
        // A question belongs to a row that is actually blocked. When the kept fact is a turn
        // boundary, the generic "waiting for your input" would be rendered next to a `Review`
        // row and describe nothing it doesn't already say.
        question: match previous.fact {
            HookFact::NeedsInput => previous.question.clone().or(incoming.question),
            _ => previous.question.clone(),
        },
        // The nudge is the more recent payload, so prefer its session id, but never lose one by
        // taking a payload that happened to omit it.
        session_id: incoming.session_id.or_else(|| previous.session_id.clone()),
        // A `Notification` never carries an edit, so this only ever restores what the kept fact
        // already said. Delivery of edits does not depend on this field either way - they are
        // appended to [`EditLog`] straight off the wire, before any merging - but a stored report
        // whose `edit` disagreed with the rest of it would be a trap for the next reader.
        edit: previous.edit.clone(),
        // Same reasoning as `edit`: only a `UserPromptSubmit` ever carries a prompt, and a nudge
        // is never one, so this restores what the kept fact already said rather than blanking it.
        // The run title itself does not depend on this field surviving here either - see
        // [`HookRecord::first_prompt`], which latches the first prompt once and never re-reads
        // the stored report.
        prompt: previous.prompt.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::event::{EditPhase, HookFact};

    /// One real `PreToolUse`/`PostToolUse`-shaped report - `event::parse` is exercised directly
    /// (it's the same parser the real notification consumer calls), rather than hand-building a
    /// `HookReport`, so these tests stay honest about what a real payload produces.
    fn parsed(event_name: &str, payload: &str) -> HookReport {
        event::parse(event_name, payload.as_bytes()).expect("must parse")
    }

    #[test]
    fn every_file_write_in_a_burst_survives_the_drain_rather_than_only_the_last_one() {
        // The whole reason `EditLog` is a second structure rather than another field on
        // `HookRecord`: a turn that writes six files is six facts, and the inbox next to it keeps
        // exactly one. Stored latest-wins, five of these would be gone before the UI thread looked
        // (GitHub issue #284).
        let mut edits = EditLog::default();
        for n in 0..6 {
            edits.record(
                7,
                EditedFile {
                    phase: EditPhase::After,
                    path: format!("f{n}.rs"),
                    cwd: None,
                },
                None,
            );
        }

        let (drained, dropped) = edits.drain();
        assert_eq!(dropped, 0);
        assert_eq!(
            drained
                .iter()
                .map(|edit| edit.file.path.clone())
                .collect::<Vec<_>>(),
            (0..6).map(|n| format!("f{n}.rs")).collect::<Vec<_>>(),
            "every write, in arrival order"
        );
        assert!(drained.iter().all(|edit| edit.agent == 7));

        let (drained_again, _) = edits.drain();
        assert!(
            drained_again.is_empty(),
            "a drain really takes them - a second reader must not replay the same edits"
        );
    }

    #[test]
    fn an_event_that_writes_no_file_never_reaches_the_edit_log() {
        for (event_name, body) in [
            (
                "PostToolUse",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/a.rs"}}"#,
            ),
            (
                "PostToolUse",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
            ),
            ("Stop", r#"{"hook_event_name":"Stop"}"#),
            (
                "Notification",
                r#"{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"m"}"#,
            ),
        ] {
            let report = parsed(event_name, body);
            assert!(report.edit.is_none(), "{event_name}: {report:?}");
        }
    }

    #[test]
    fn forgetting_an_agent_drops_its_undrained_edits_so_a_recycled_id_cannot_inherit_them() {
        let mut edits = EditLog::default();
        for agent in [7, 8] {
            edits.record(
                agent,
                EditedFile {
                    phase: EditPhase::After,
                    path: "a.rs".into(),
                    cwd: None,
                },
                None,
            );
        }

        edits.forget(7);
        let (drained, _) = edits.drain();
        assert_eq!(
            drained.iter().map(|edit| edit.agent).collect::<Vec<_>>(),
            vec![8],
            "the closed agent's writes go with it; the live agent's stay"
        );
    }

    #[test]
    fn the_edit_log_sheds_its_oldest_entries_rather_than_growing_without_bound() {
        let mut log = EditLog::default();
        for index in 0..MAX_PENDING_EDITS + 3 {
            log.record(
                7,
                EditedFile {
                    phase: EditPhase::After,
                    path: format!("f{index}.rs"),
                    cwd: None,
                },
                None,
            );
        }
        let (edits, dropped) = log.drain();
        assert_eq!(edits.len(), MAX_PENDING_EDITS);
        assert_eq!(dropped, 3, "an overflow is counted, not swallowed");
        assert_eq!(
            edits.first().map(|edit| edit.file.path.as_str()),
            Some("f3.rs"),
            "the oldest go first - their files are the ones most likely already overwritten"
        );
        assert_eq!(log.drain().1, 0, "the dropped count resets with the drain");
    }

    #[test]
    fn the_inbox_is_capped_so_invented_agent_ids_cannot_grow_it_without_bound() {
        let mut inbox = HookInbox::default();
        let report = parsed("Stop", r#"{"hook_event_name":"Stop"}"#);
        for id in 0..(MAX_TRACKED_AGENTS as u64 + 50) {
            inbox.record(id, report.clone());
        }
        assert_eq!(
            inbox.latest.len(),
            MAX_TRACKED_AGENTS,
            "the inbox must not grow past its cap"
        );
        assert!(inbox.get(MAX_TRACKED_AGENTS as u64 + 49).is_some());
        assert!(inbox.get(0).is_none());
    }

    #[test]
    fn the_latest_event_replaces_the_previous_one_for_the_same_agent() {
        // "Latest wins" is what keeps a routine tool failure from pinning a row to Failed - see
        // `HookInbox`'s own docs.
        let mut inbox = HookInbox::default();
        inbox.record(
            2,
            parsed(
                "PostToolUseFailure",
                r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","error":"exit 1"}"#,
            ),
        );
        assert_eq!(inbox.get(2).unwrap().report.fact, HookFact::TurnFailed);

        inbox.record(
            2,
            parsed(
                "PreToolUse",
                r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/tmp/a.rs"}}"#,
            ),
        );
        assert_eq!(
            inbox.get(2).unwrap().report.fact,
            HookFact::Working,
            "the next real event must supersede the failure"
        );
    }

    #[test]
    fn a_generic_permission_notification_cannot_overwrite_the_real_permission_question() {
        let mut inbox = HookInbox::default();
        inbox.record(
            4,
            parsed(
                "PreToolUse",
                r#"{"session_id":"s-1","hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"probe-notes.txt"}}"#,
            ),
        );
        inbox.record(
            4,
            parsed(
                "PermissionRequest",
                r#"{"session_id":"s-1","hook_event_name":"PermissionRequest","tool_name":"Write","tool_input":{"file_path":"probe-notes.txt"}}"#,
            ),
        );
        assert_eq!(
            inbox.get(4).unwrap().report.question.as_deref(),
            Some("Write needs permission: probe-notes.txt")
        );

        inbox.record(
            4,
            parsed(
                "Notification",
                r#"{"session_id":"s-1","hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission"}"#,
            ),
        );

        assert_eq!(inbox.get(4).unwrap().report.fact, HookFact::NeedsInput);
        assert_eq!(
            inbox.get(4).unwrap().report.question.as_deref(),
            Some("Write needs permission: probe-notes.txt"),
            "the generic notification must not replace the real question for the same block"
        );
    }

    #[test]
    fn an_idle_notification_cannot_erase_the_turn_boundary_a_real_stop_established() {
        let mut inbox = HookInbox::default();
        inbox.record(
            6,
            parsed("Stop", r#"{"session_id":"s-2","hook_event_name":"Stop"}"#),
        );
        assert_eq!(inbox.get(6).unwrap().report.fact, HookFact::TurnEnded);

        inbox.record(
            6,
            parsed(
                "Notification",
                r#"{"session_id":"s-2","hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input"}"#,
            ),
        );

        assert_eq!(
            inbox.get(6).unwrap().report.fact,
            HookFact::TurnEnded,
            "the turn really did end; an idle nudge is not evidence that it didn't"
        );
        assert_eq!(
            inbox.get(6).unwrap().report.question,
            None,
            "a finished row must not carry a question describing nothing it doesn't already say"
        );
    }

    #[test]
    fn a_notification_still_reports_a_block_jerry_has_not_already_heard_about() {
        // The other side of the rule: nudges are folded in, never ignored. An agent Jerry last saw
        // *working* that hits a permission prompt must still land on `NeedsInput` off the
        // notification alone - which is the only signal at all for a block that fires no
        // `PermissionRequest`.
        let mut inbox = HookInbox::default();
        inbox.record(
            7,
            parsed(
                "PreToolUse",
                r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#,
            ),
        );
        assert_eq!(inbox.get(7).unwrap().report.fact, HookFact::Working);

        inbox.record(
            7,
            parsed(
                "Notification",
                r#"{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission"}"#,
            ),
        );
        assert_eq!(inbox.get(7).unwrap().report.fact, HookFact::NeedsInput);
        assert_eq!(
            inbox.get(7).unwrap().report.question.as_deref(),
            Some("Claude needs your permission"),
            "with no better question on record, the notification's own message is the best there is"
        );

        inbox.record(
            7,
            parsed(
                "PostToolUse",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#,
            ),
        );
        assert_eq!(inbox.get(7).unwrap().report.fact, HookFact::Working);
        assert_eq!(inbox.get(7).unwrap().report.question, None);
    }

    #[test]
    fn a_runs_title_and_turn_count_come_off_its_own_real_hook_stream() {
        let mut inbox = HookInbox::default();
        assert!(
            inbox.get(11).is_none(),
            "an agent that has reported nothing has no record at all"
        );

        inbox.record(
            11,
            parsed(
                "UserPromptSubmit",
                r#"{"session_id":"s-11","hook_event_name":"UserPromptSubmit","prompt":"Reproduce the refresh race in a test"}"#,
            ),
        );
        assert_eq!(
            inbox.get(11).unwrap().first_prompt.as_deref(),
            Some("Reproduce the refresh race in a test")
        );
        assert_eq!(inbox.get(11).unwrap().turns, 0);

        for _ in 0..2 {
            inbox.record(
                11,
                parsed("Stop", r#"{"session_id":"s-11","hook_event_name":"Stop"}"#),
            );
        }
        assert_eq!(
            inbox.get(11).unwrap().turns,
            2,
            "each real Stop is one completed turn"
        );

        inbox.record(
            11,
            parsed(
                "UserPromptSubmit",
                r#"{"session_id":"s-11","hook_event_name":"UserPromptSubmit","prompt":"yes"}"#,
            ),
        );
        assert_eq!(
            inbox.get(11).unwrap().first_prompt.as_deref(),
            Some("Reproduce the refresh race in a test"),
            "the title is the first prompt, never the latest one"
        );

        inbox.record(
            11,
            parsed(
                "Notification",
                r#"{"session_id":"s-11","hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input"}"#,
            ),
        );
        assert_eq!(inbox.get(11).unwrap().turns, 2);
    }

    #[test]
    fn forgetting_an_agent_clears_its_facts_so_a_reused_id_cannot_inherit_them() {
        let mut inbox = HookInbox::default();
        inbox.record(9, parsed("Stop", r#"{"hook_event_name":"Stop"}"#));
        assert_eq!(inbox.get(9).unwrap().report.fact, HookFact::TurnEnded);
        inbox.forget(9);
        assert!(inbox.get(9).is_none());
    }
}
