//! Turning one raw Cursor Agent CLI hook payload into the same [`HookReport`]/[`HookFact`] shape
//! `crate::hooks::event` produces for Claude Code (GitHub issue #479). Kept as a real second
//! parser rather than teaching `event::parse` a second schema: Cursor's field names, casing and
//! per-event shape don't match Claude's at all, so unifying them would only be indirection -
//! everything downstream (`crate::hooks::server`'s inbox/edit recording,
//! `crate::hooks::flow::AdeApp::record_agent_statuses`, History) reads the shared [`HookReport`]
//! type and doesn't know or care which parser produced it.
//!
//! **Scope cut, load-bearing:** [`HookReport::edit`] is always `None` for every report this
//! module produces, including `preToolUse`/`postToolUse` for a file-writing `tool_name` like
//! `Write`/`Edit`. `tool_input`'s *inner* JSON shape comes from a protobuf `toJson()` call inside
//! Cursor's own CLI, and protobuf JSON serialization can legally use either a field's exact proto
//! name or a camelCase transform of it depending on how the `.proto` schema annotates that field -
//! genuinely unknown without a live captured payload, unlike every field this module *does* read
//! below (flat, unambiguous, directly-constructed JS object keys, verified against the installed
//! CLI's own compiled bundle - see GitHub issue #479's research comment). Guessing the inner
//! `tool_input` shape wrong would silently attribute a file edit to the wrong path rather than
//! fail loudly, which is worse than not attributing one at all. Revisit only with a real captured
//! payload, the same standard `event.rs`'s own `REAL_*` fixtures are held to.

use crate::hooks::event::{
    truncated, HookFact, HookReport, ACTIVITY_MAX_CHARS, MAX_PAYLOAD_BYTES, PROMPT_MAX_CHARS,
    QUESTION_MAX_CHARS,
};

/// Parses one Cursor Agent CLI hook event into the fact Jerry acts on, or `None` if this payload
/// carries nothing worth changing a row over - an unrecognised `event_name`, a payload over
/// [`MAX_PAYLOAD_BYTES`], invalid/non-object JSON, or a required field missing for the event that
/// fired. Never panics: this runs on every real hook POST from a live agent process, so a bad
/// payload must degrade to "nothing changed" rather than take the listener down.
pub fn parse(event_name: &str, payload: &[u8]) -> Option<HookReport> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    if !value.is_object() {
        return None;
    }

    // Every real event payload carries `conversation_id` - Cursor's own `session_id` equivalent,
    // and what GitHub issue #227's History/"Resume here" needs (`HookReport::session_id`'s own
    // docs cover why it's read once here rather than per-arm, same as `event::parse`).
    let session_id = value
        .get("conversation_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    let report = match event_name {
        "beforeSubmitPrompt" => Some(HookReport {
            prompt: value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .and_then(|prompt| truncated(prompt, PROMPT_MAX_CHARS)),
            ..HookReport::bare(HookFact::Working)
        }),

        // `tool_input`'s inner shape is deliberately not read at all - see this module's own
        // scope-cut docs. The activity line degrades to the bare tool name, matching
        // `event::parse`'s own fallback when `tool_input_preview` finds nothing.
        "preToolUse" | "postToolUse" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            Some(HookReport {
                activity: truncated(tool, ACTIVITY_MAX_CHARS),
                ..HookReport::bare(HookFact::Working)
            })
        }

        "postToolUseFailure" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            Some(HookReport {
                activity: truncated(tool, ACTIVITY_MAX_CHARS),
                question: value
                    .get("error_message")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|message| truncated(message, QUESTION_MAX_CHARS)),
                ..HookReport::bare(HookFact::TurnFailed)
            })
        }

        // No `last_assistant_message`-equivalent field on `stop` itself - Cursor splits that onto
        // `afterAgentResponse.text` instead (handled below).
        "stop" => Some(HookReport::bare(HookFact::TurnEnded)),

        // Judgement call, deliberately flagged in the PR this shipped in: this event carries
        // `text` (the assistant's final message, what Claude gets for free on
        // `Stop.last_assistant_message`), but threading it onto the following `stop` needs
        // per-agent state kept across two calls, and `parse()` is pure with no memory of the last
        // payload. That pairing lives in the *caller* instead (`crate::hooks::server`'s
        // `edits`/`inbox`, keyed by `AgentId`), so `text` is dropped here rather than this module
        // growing a second, parallel state-tracking mechanism. `HookFact::Working`, not a fifth
        // variant, since the agent genuinely is still mid-turn.
        "afterAgentResponse" => Some(HookReport::bare(HookFact::Working)),

        _ => None,
    }?;

    Some(HookReport {
        session_id,
        ..report
    })
}

#[cfg(test)]
mod cursor_event_tests {
    use super::parse;
    use crate::hooks::event::{EventKind, HookFact, MAX_PAYLOAD_BYTES};

    // Synthetic payloads built from the field lists in GitHub issue #479's design comment - read
    // out of the installed CLI's own compiled bundle, but never captured from a live run. Named
    // `SYNTHETIC_*` (not `event.rs`'s `REAL_*`) so a future reader never mistakes these for
    // captured ground truth.

    const SYNTHETIC_BEFORE_SUBMIT_PROMPT: &[u8] = br#"{
        "conversation_id": "conv-123",
        "generation_id": "gen-1",
        "model": "claude-4.5-sonnet",
        "prompt": "add a health check endpoint",
        "attachments": [],
        "composer_mode": "agent"
    }"#;

    const SYNTHETIC_PRE_TOOL_USE: &[u8] = br#"{
        "conversation_id": "conv-123",
        "generation_id": "gen-1",
        "model": "claude-4.5-sonnet",
        "tool_name": "Write",
        "tool_input": { "file_path": "/tmp/repo/src/main.rs", "content": "fn main() {}" },
        "tool_use_id": "tool-1",
        "cwd": "/tmp/repo"
    }"#;

    const SYNTHETIC_POST_TOOL_USE: &[u8] = br#"{
        "conversation_id": "conv-123",
        "generation_id": "gen-1",
        "model": "claude-4.5-sonnet",
        "tool_name": "Edit",
        "tool_input": { "file_path": "/tmp/repo/src/main.rs" },
        "tool_use_id": "tool-1",
        "cwd": "/tmp/repo",
        "tool_output": "ok",
        "duration": 120
    }"#;

    const SYNTHETIC_POST_TOOL_USE_FAILURE: &[u8] = br#"{
        "conversation_id": "conv-123",
        "generation_id": "gen-1",
        "model": "claude-4.5-sonnet",
        "tool_name": "Bash",
        "tool_input": { "command": "cargo test" },
        "tool_use_id": "tool-2",
        "cwd": "/tmp/repo",
        "error_message": "exit status 101",
        "failure_type": "non_zero_exit",
        "is_interrupt": false
    }"#;

    const SYNTHETIC_STOP: &[u8] = br#"{
        "conversation_id": "conv-123",
        "generation_id": "gen-1",
        "model": "claude-4.5-sonnet",
        "status": "completed",
        "loop_count": 3,
        "input_tokens": 4200,
        "output_tokens": 850
    }"#;

    const SYNTHETIC_AFTER_AGENT_RESPONSE: &[u8] = br#"{
        "conversation_id": "conv-123",
        "generation_id": "gen-1",
        "model": "claude-4.5-sonnet",
        "text": "Both done: added the endpoint and a test for it.",
        "input_tokens": 4200,
        "output_tokens": 850
    }"#;

    #[test]
    fn before_submit_prompt_reports_working_and_carries_the_prompt_and_session_id() {
        let report =
            parse("beforeSubmitPrompt", SYNTHETIC_BEFORE_SUBMIT_PROMPT).expect("must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert_eq!(report.kind, EventKind::Transition);
        assert_eq!(
            report.prompt.as_deref(),
            Some("add a health check endpoint")
        );
        assert_eq!(report.session_id.as_deref(), Some("conv-123"));
        assert!(report.edit.is_none());
    }

    #[test]
    fn pre_tool_use_reports_working_with_an_activity_line_and_no_edit() {
        let report = parse("preToolUse", SYNTHETIC_PRE_TOOL_USE).expect("must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert_eq!(report.activity.as_deref(), Some("Write"));
        assert!(
            report.edit.is_none(),
            "the scope cut must hold even for a file-writing tool_name"
        );
    }

    #[test]
    fn post_tool_use_reports_working_with_an_activity_line_and_no_edit() {
        let report = parse("postToolUse", SYNTHETIC_POST_TOOL_USE).expect("must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert_eq!(report.activity.as_deref(), Some("Edit"));
        assert!(
            report.edit.is_none(),
            "the scope cut must hold even for a file-writing tool_name"
        );
    }

    #[test]
    fn post_tool_use_failure_reports_turn_failed_with_the_error_message() {
        let report =
            parse("postToolUseFailure", SYNTHETIC_POST_TOOL_USE_FAILURE).expect("must parse");
        assert_eq!(report.fact, HookFact::TurnFailed);
        assert_eq!(report.activity.as_deref(), Some("Bash"));
        assert_eq!(report.question.as_deref(), Some("exit status 101"));
        assert!(report.edit.is_none());
    }

    #[test]
    fn stop_reports_turn_ended_bare() {
        let report = parse("stop", SYNTHETIC_STOP).expect("must parse");
        assert_eq!(report.fact, HookFact::TurnEnded);
        assert_eq!(report.session_id.as_deref(), Some("conv-123"));
        assert!(report.edit.is_none());
    }

    #[test]
    fn after_agent_response_reports_working_and_never_a_fifth_fact() {
        let report =
            parse("afterAgentResponse", SYNTHETIC_AFTER_AGENT_RESPONSE).expect("must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert!(report.edit.is_none());
    }

    #[test]
    fn an_unknown_event_name_returns_none() {
        assert!(parse("somethingCursorDoesNotSend", SYNTHETIC_STOP).is_none());
    }

    #[test]
    fn malformed_json_returns_none_rather_than_panicking() {
        assert!(parse("stop", b"{ not json").is_none());
    }

    #[test]
    fn a_non_object_json_root_returns_none() {
        assert!(parse("stop", b"[1,2,3]").is_none());
    }

    #[test]
    fn pre_tool_use_missing_tool_name_returns_none_rather_than_panicking() {
        let payload = br#"{"conversation_id": "conv-123", "tool_input": {}}"#;
        assert!(parse("preToolUse", payload).is_none());
    }

    #[test]
    fn post_tool_use_failure_missing_tool_name_returns_none() {
        let payload = br#"{"conversation_id": "conv-123", "error_message": "boom"}"#;
        assert!(parse("postToolUseFailure", payload).is_none());
    }

    #[test]
    fn a_payload_over_the_max_size_returns_none() {
        let oversized = vec![b'a'; MAX_PAYLOAD_BYTES + 1];
        assert!(parse("stop", &oversized).is_none());
    }
}
