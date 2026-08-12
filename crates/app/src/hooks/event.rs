//! Turning one raw Claude Code hook payload into the small, already-decided fact Jerry's rail
//! needs (GitHub issue #239, phase 2).
//!
//! GPUI-free, socket-free and process-free: takes an event name and the raw JSON bytes Claude
//! Code wrote to the forwarder's stdin, and returns a [`HookReport`] - so every extraction rule
//! below is directly `#[test]`-able without a window, a listener or a child process. That is the
//! same contract [`crate::rail::status`] and `crate::rail::title_signal` already hold.
//!
//! ## Why this is a real signal and the pty is not
//!
//! Phase 1 read what an agent CLI *happened to render into its terminal* - a title glyph, an
//! OSC 9 ping. Those are real, but they are presentation: they exist because a human was meant
//! to look at them, they are coarse (a spinner says "busy", never "editing auth.rs"), and a CLI
//! is free to restyle them in any release. A hook payload is the opposite kind of fact - it is a
//! documented, structured side-channel Claude Code emits *for programs*, delivered out-of-band
//! from the interactive TUI's stdio, carrying the actual tool name and the actual argument. It
//! is the difference between reading a progress bar off a screenshot and being handed the event.
//!
//! ## The payload shapes below were captured, not guessed
//!
//! Every field this module reads was verified against real payloads emitted by a real
//! `claude` 2.1.228 binary on this machine (a scratch project, hooks pointed at a capture
//! script, a prompt that drove a real `Bash` call and a real `Write` call), cross-checked
//! against <https://code.claude.com/docs/en/hooks>. That matters because the shapes are not
//! uniform: the "interesting" argument lives under a *different key per tool*
//! (`tool_input.command` for `Bash`, `tool_input.file_path` for `Edit`/`Write`/`Read`,
//! `tool_input.pattern` for `Grep`), so a single hardcoded field name would silently produce a
//! bare tool name for every tool but one. [`tool_input_preview`] is the real per-tool lookup,
//! ordered most-specific first, with a documented fallback rather than a guess.
//!
//! ## What is deliberately *not* extracted
//!
//! `tool_output` (`PostToolUse`) and `last_assistant_message` (`Stop`) are real fields carrying
//! real text, and both are ignored. They are model/command output of unbounded size and
//! arbitrary content, and the rail has one short line to render - a truncated first line of a
//! compiler's stderr is noise wearing the costume of a status. `Stop` is used purely as the turn
//! boundary it is; what changed during the turn is answered by the real review diff
//! (`crate::review::flow`), which is a fact about the worktree rather than about what the model
//! said it did.

use std::time::Duration;

/// How long a hook fact keeps outranking the pty-quiescence and terminal-title heuristics before
/// Jerry falls back to them - see [`crate::rail::status::HookSignal`] for how the fallback works.
///
/// 30 minutes, matching the TTL the research for GitHub issue #239 found in a competitor's
/// hook-based implementation. The value is a statement about *staleness*, not about session
/// length: a hook fact is a point-in-time observation, and the failure it must bound is the
/// process that stopped emitting hooks entirely (Claude Code killed with `SIGKILL`, a crashed
/// forwarder, a `claude` upgrade that renames an event) while its pty stays open. Left
/// unbounded, such an agent would pin whatever status it last reported forever, which is exactly
/// the "confidently wrong" failure the quiescence floor exists to catch.
///
/// Why not shorter: a real turn genuinely can run far longer than a few minutes between a
/// `PreToolUse` and its `PostToolUse` - a long test suite, a big build - and expiring mid-turn
/// would hand the row back to the quiescence guess precisely during the long silence that guess
/// is worst at (the false "needs input" this whole issue exists to fix). Why not longer: past
/// half an hour, a fact this stale is not evidence about the present, and an agent silently
/// wedged for 30 minutes *should* fall back to being reported by its silence.
pub const HOOK_SIGNAL_TTL: Duration = Duration::from_secs(30 * 60);

/// Longest [`HookReport::activity`] Jerry will keep - the rail renders this as trailing text on
/// one line and truncates visually anyway, so this is about not carrying an unbounded string
/// around, not about layout. A `Bash` command or a file path is the realistic content, and both
/// stay readable at this width.
pub const ACTIVITY_MAX_CHARS: usize = 80;

/// Longest [`HookReport::question`] Jerry will keep. Wider than [`ACTIVITY_MAX_CHARS`] because a
/// permission reason is a real sentence a human has to act on ("Bash needs permission to run:
/// npm test"), where an activity line is a label.
pub const QUESTION_MAX_CHARS: usize = 200;

/// The largest hook payload Jerry will parse at all. Claude Code payloads are small - the real
/// ones captured were a few hundred bytes - but `tool_input.content` on a `Write` carries an
/// entire file, and `tool_output` an entire command's output, so the honest upper bound is "as
/// big as whatever the model just did". This is the parse-side guard; the listener enforces the
/// same limit on the wire (see `crate::hooks::server`) so an oversized body is never even
/// buffered.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// What one hook event tells Jerry about the agent's *state* - the whole reason the payload is
/// parsed at all. Deliberately four coarse variants rather than one per event name: the rail
/// renders five statuses, and several distinct events are the same fact about the agent (a
/// `PreToolUse` and a `PostToolUse` both mean "mid-turn, working").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFact {
    /// The agent is mid-turn and doing something - `UserPromptSubmit`, `PreToolUse`,
    /// `PostToolUse`, `SessionStart`.
    Working,
    /// The agent is blocked on the human - a `PermissionRequest`, or a `Notification` whose
    /// `notification_type` really means "waiting on you" (see [`notification_wants_human`]).
    NeedsInput,
    /// The turn ended cleanly (`Stop`). Whether that means "review ready" or "idle" is not this
    /// module's call - it depends on the real review diff, and is decided in
    /// [`crate::rail::status::derive_status`].
    TurnEnded,
    /// The turn ended badly, or a tool call failed - `StopFailure`, `PostToolUseFailure`.
    TurnFailed,
}

/// One parsed hook event, reduced to exactly what the rail row needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    /// The state fact - see [`HookFact`].
    pub fact: HookFact,
    /// Trailing "what it is doing" text for a running row, roughly `"{tool}: {argument}"`, already
    /// truncated to [`ACTIVITY_MAX_CHARS`]. `None` for events that carry no tool context.
    pub activity: Option<String>,
    /// The real permission reason / notification message, already truncated to
    /// [`QUESTION_MAX_CHARS`]. `None` unless the event actually carries human-facing text.
    pub question: Option<String>,
    /// The real Claude Code `session_id` this payload carried, if any (GitHub issue #227).
    ///
    /// Verified present on *every* real event type this module parses - a real `claude` 2.1.228
    /// binary was driven through `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`
    /// and `Stop` (a scratch project, `--settings` pointed at a capture script) and every single
    /// payload carried the same `session_id` for the whole conversation, including across a real
    /// `claude --resume <session_id>` re-invocation (`SessionStart`'s `source` simply reads
    /// `"resume"` instead of `"startup"`). That is the real, durable identifier `claude
    /// --resume`/`-r` takes - confirmed against the same real binary: resuming by this id and
    /// asking what the agent had just done answered correctly, proving it is the *same*
    /// conversation rather than a fresh one that merely inherited some context.
    ///
    /// `None` for a payload that omits it (untrusted input off the socket - not every hand-made
    /// or malformed request will carry one), which a reader must treat as "no id available",
    /// never as a reason to fail the rest of the report.
    pub session_id: Option<String>,
}

impl HookReport {
    /// A report carrying only a state fact - the common case for the turn-boundary events, which
    /// have no text worth rendering (see the module docs on `last_assistant_message`).
    fn bare(fact: HookFact) -> HookReport {
        HookReport {
            fact,
            activity: None,
            question: None,
            session_id: None,
        }
    }
}

/// Truncates on a real `char` boundary, appending an ellipsis only when something was actually
/// cut. Returns `None` for text that is empty or whitespace-only, so a present-but-blank JSON
/// field is treated as the absence of information rather than rendered as an empty row.
fn truncated(text: &str, max_chars: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Collapse real newlines/tabs: the rail renders a single line, and a multi-line permission
    // reason would otherwise render as its first line with the rest silently invisible.
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= max_chars {
        return Some(flattened);
    }
    let kept: String = flattened
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    Some(format!("{}\u{2026}", kept.trim_end()))
}

/// The interesting argument out of a `tool_input` object, as `(value, was_found)`.
///
/// Ordered most-specific-first over the real per-tool keys, because there is genuinely no single
/// field: `Bash` puts its command in `command`, the file tools put a path in `file_path`, the
/// search tools put a needle in `pattern`/`query`, and `Task` puts a human label in
/// `description`. Anything unrecognised - including every MCP tool, whose input schema is defined
/// by a third-party server and cannot be enumerated here - falls through to `None`, which renders
/// as the bare tool name rather than as a wrong-but-plausible field.
fn tool_input_preview(tool_input: &serde_json::Value) -> Option<&str> {
    const KEYS: [&str; 6] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "description",
    ];
    KEYS.iter()
        .find_map(|key| tool_input.get(key).and_then(serde_json::Value::as_str))
}

/// Whether a `Notification`'s `notification_type` really means "a human is being waited on".
///
/// This distinction is the entire reason the field is read rather than treating every
/// `Notification` as attention-worthy: Claude Code emits real notification types that are pure
/// information (`auth_success`, `agent_completed`, the `elicitation_*` lifecycle echoes), and
/// promoting those to [`crate::rail::status::Status::Ask`] would light the rail up with rows
/// that need nothing - the exact false-positive class GitHub issue #239 exists to remove. Only
/// the types that describe a *block* count.
///
/// Unknown types deliberately return `false`: a notification type this build has never heard of
/// is not evidence a human is needed, and the quiescence floor still catches a genuinely stuck
/// agent on its own.
fn notification_wants_human(notification_type: &str) -> bool {
    matches!(
        notification_type,
        "permission_prompt" | "idle_prompt" | "agent_needs_input" | "elicitation_dialog"
    )
}

/// Parses one hook event into the fact Jerry acts on, or `None` if this event carries nothing
/// worth changing a row over.
///
/// `None` is a real, common answer, not just an error path: Jerry declares only the events it
/// uses, but a payload that fails to parse, an event this build doesn't act on, or a
/// `Notification` that isn't about a block must all leave the row exactly as it was rather than
/// force some default. Malformed JSON is likewise `None` - never a panic and never an error the
/// listener has to handle, because a hook payload is untrusted input arriving on a socket.
pub fn parse(event_name: &str, payload: &[u8]) -> Option<HookReport> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    // A payload that isn't valid JSON at all still carries one real fact: this event fired. For
    // the events whose meaning is the firing itself that would be enough - but trusting a body
    // Jerry couldn't parse is how a half-written or truncated request turns into a wrong status,
    // so every event here requires a real parse first.
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    // Every real hook payload is a JSON *object*. Requiring that (rather than only requiring
    // "valid JSON") matters for the events whose meaning is carried by the event name alone:
    // without it, a body of `"x"` or `[]` - valid JSON, and exactly what a confused or hostile
    // client would send - would be enough to forge a turn boundary.
    if !value.is_object() {
        return None;
    }

    // Read once, attached to whatever report the match below produces (GitHub issue #227): every
    // real event carries the same session-scoped `session_id`, so extracting it per-arm would be
    // pure repetition - see [`HookReport::session_id`]'s own docs for why this field exists at
    // all and how it was verified.
    let session_id = value
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    let report = match event_name {
        "SessionStart" | "UserPromptSubmit" => Some(HookReport::bare(HookFact::Working)),

        "PreToolUse" | "PostToolUse" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            let argument = value.get("tool_input").and_then(tool_input_preview);
            let activity = match argument {
                Some(argument) => truncated(&format!("{tool}: {argument}"), ACTIVITY_MAX_CHARS),
                None => truncated(tool, ACTIVITY_MAX_CHARS),
            };
            Some(HookReport {
                fact: HookFact::Working,
                activity,
                question: None,
                session_id: None,
            })
        }

        // A failed tool call is a real failure signal, but it is *not* the end of the turn -
        // Claude Code routinely recovers from one and keeps working. The activity text is kept so
        // the row can say which tool broke.
        "PostToolUseFailure" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            Some(HookReport {
                fact: HookFact::TurnFailed,
                activity: truncated(tool, ACTIVITY_MAX_CHARS),
                question: value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|error| truncated(error, QUESTION_MAX_CHARS)),
                session_id: None,
            })
        }

        // A real permission prompt: the agent is blocked until a human answers. The tool and its
        // argument are the question - "Bash: sudo reboot" is what the human is being asked about.
        "PermissionRequest" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            let argument = value.get("tool_input").and_then(tool_input_preview);
            let question = match argument {
                Some(argument) => truncated(
                    &format!("{tool} needs permission: {argument}"),
                    QUESTION_MAX_CHARS,
                ),
                None => truncated(&format!("{tool} needs permission"), QUESTION_MAX_CHARS),
            };
            Some(HookReport {
                fact: HookFact::NeedsInput,
                activity: None,
                question,
                session_id: None,
            })
        }

        "Notification" => {
            let notification_type = value
                .get("notification_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !notification_wants_human(notification_type) {
                return None;
            }
            Some(HookReport {
                fact: HookFact::NeedsInput,
                activity: None,
                question: value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|message| truncated(message, QUESTION_MAX_CHARS)),
                session_id: None,
            })
        }

        "Stop" => Some(HookReport::bare(HookFact::TurnEnded)),

        "StopFailure" => Some(HookReport {
            fact: HookFact::TurnFailed,
            activity: None,
            question: value
                .get("error_message")
                .and_then(serde_json::Value::as_str)
                .and_then(|message| truncated(message, QUESTION_MAX_CHARS)),
            session_id: None,
        }),

        _ => None,
    }?;

    Some(HookReport {
        session_id,
        ..report
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact `PreToolUse` body a real `claude` 2.1.228 wrote to a hook's stdin on this
    /// machine, captured during this phase's build (only `transcript_path`/`session_id` shortened).
    /// Pinned verbatim so a future refactor of the extraction rules is checked against a real
    /// payload rather than against a payload written to make the parser pass.
    const REAL_PRE_TOOL_USE_BASH: &[u8] = br#"{"session_id":"5a4bef04","transcript_path":"/home/colin/.claude/projects/x/5a4bef04.jsonl","cwd":"/tmp/capture","prompt_id":"4108775d","permission_mode":"default","effort":{"level":"high"},"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hello-from-jerry","description":"Echo hello-from-jerry"},"tool_use_id":"toolu_017yNzAHSe1j6rqbwMkN7gJc"}"#;

    /// The real `PreToolUse` for a `Write` from the same captured run - the payload that proves
    /// the per-tool key lookup is necessary, since it carries no `command` at all.
    const REAL_PRE_TOOL_USE_WRITE: &[u8] = br#"{"session_id":"5a4bef04","cwd":"/tmp/capture","hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"/tmp/capture/done.txt","content":"done\n"},"tool_use_id":"toolu_01TLYrMch3N78LFKWbm8J4WS"}"#;

    /// The real `Stop` body from the same run.
    const REAL_STOP: &[u8] = br#"{"session_id":"5a4bef04","cwd":"/tmp/capture","permission_mode":"default","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"Both done:\n- `echo hello-from-jerry`","background_tasks":[],"session_crons":[]}"#;

    #[test]
    fn a_real_captured_bash_pre_tool_use_becomes_working_with_the_real_command() {
        let report = parse("PreToolUse", REAL_PRE_TOOL_USE_BASH).expect("real payload must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert_eq!(
            report.activity.as_deref(),
            Some("Bash: echo hello-from-jerry")
        );
        assert_eq!(report.question, None);
        assert_eq!(report.session_id.as_deref(), Some("5a4bef04"));
    }

    #[test]
    fn a_real_captured_write_pre_tool_use_uses_file_path_not_command() {
        // The whole reason `tool_input_preview` is a per-tool lookup: this real payload has no
        // `command` key, and a parser hardcoded to `command` would report a bare "Write".
        let report = parse("PreToolUse", REAL_PRE_TOOL_USE_WRITE).expect("real payload must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert_eq!(
            report.activity.as_deref(),
            Some("Write: /tmp/capture/done.txt")
        );
        assert_eq!(report.session_id.as_deref(), Some("5a4bef04"));
    }

    #[test]
    fn a_real_captured_stop_is_the_turn_boundary_and_carries_no_text() {
        let report = parse("Stop", REAL_STOP).expect("real payload must parse");
        assert_eq!(report.fact, HookFact::TurnEnded);
        // `last_assistant_message` is real and present in this payload, and deliberately dropped -
        // see the module docs.
        assert_eq!(report.activity, None);
        assert_eq!(report.question, None);
        // A turn-boundary event still carries the real session id - GitHub issue #227's resume
        // flow needs it from `Stop` just as much as from a `PreToolUse`, since `Stop` is the last
        // event an agent that then sits idle (and is later closed) will ever send.
        assert_eq!(report.session_id.as_deref(), Some("5a4bef04"));
    }

    #[test]
    fn a_payload_with_no_session_id_leaves_it_none_rather_than_a_fabricated_value() {
        // Untrusted input off the socket: a hand-made or malformed request may simply omit the
        // field, and that must read back as "no id available", not panic or a wrong guess.
        let report = parse(
            "Stop",
            br#"{"hook_event_name":"Stop","stop_hook_active":false}"#,
        )
        .expect("must parse");
        assert_eq!(report.session_id, None);
    }

    #[test]
    fn a_resumed_sessions_hooks_report_the_same_session_id() {
        // The real proof this field is worth persisting: `claude --resume <id>` (verified against
        // a real 2.1.228 binary) keeps firing hooks under the *same* `session_id` it resumed -
        // only `SessionStart`'s `source` changes, from `"startup"` to `"resume"`. Pinned from a
        // real captured payload of exactly that resumed run.
        let real_resumed_session_start = br#"{"session_id":"5af4c210-34fa-4ab2-9c35-f6ceab76551c","transcript_path":"/home/colin/.claude/projects/x/5af4c210.jsonl","cwd":"/tmp/hook_capture/project","hook_event_name":"SessionStart","source":"resume"}"#;
        let report = parse("SessionStart", real_resumed_session_start).expect("must parse");
        assert_eq!(report.fact, HookFact::Working);
        assert_eq!(
            report.session_id.as_deref(),
            Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c")
        );
    }

    #[test]
    fn an_unknown_tool_falls_back_to_the_bare_tool_name_not_a_wrong_field() {
        let payload = br#"{"hook_event_name":"PreToolUse","tool_name":"mcp__memory__store","tool_input":{"entity":"x","observation":"y"}}"#;
        let report = parse("PreToolUse", payload).expect("must parse");
        assert_eq!(report.activity.as_deref(), Some("mcp__memory__store"));
    }

    #[test]
    fn a_permission_request_needs_input_and_names_what_it_is_asking_about() {
        let payload = br#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"sudo reboot","description":"Restart system"}}"#;
        let report = parse("PermissionRequest", payload).expect("must parse");
        assert_eq!(report.fact, HookFact::NeedsInput);
        assert_eq!(
            report.question.as_deref(),
            Some("Bash needs permission: sudo reboot")
        );
    }

    #[test]
    fn only_notification_types_that_really_block_a_human_reach_needs_input() {
        // The real reason `notification_type` is read at all (see `notification_wants_human`).
        for blocking in [
            "permission_prompt",
            "idle_prompt",
            "agent_needs_input",
            "elicitation_dialog",
        ] {
            let payload = format!(
                r#"{{"hook_event_name":"Notification","notification_type":"{blocking}","message":"Bash needs permission to run: npm test"}}"#
            );
            let report = parse("Notification", payload.as_bytes())
                .unwrap_or_else(|| panic!("{blocking} must produce a report"));
            assert_eq!(report.fact, HookFact::NeedsInput, "{blocking}");
            assert_eq!(
                report.question.as_deref(),
                Some("Bash needs permission to run: npm test")
            );
        }
        // Informational types must change nothing at all - promoting these to `Ask` would light
        // up the rail with rows that need nothing.
        for informational in [
            "auth_success",
            "agent_completed",
            "elicitation_complete",
            "elicitation_response",
            "some_type_from_a_future_release",
            "",
        ] {
            let payload = format!(
                r#"{{"hook_event_name":"Notification","notification_type":"{informational}","message":"all good"}}"#
            );
            assert_eq!(
                parse("Notification", payload.as_bytes()),
                None,
                "{informational} must not be treated as needing a human"
            );
        }
    }

    #[test]
    fn a_notification_with_no_type_at_all_is_ignored_rather_than_assumed_blocking() {
        let payload = br#"{"hook_event_name":"Notification","message":"something happened"}"#;
        assert_eq!(parse("Notification", payload), None);
    }

    #[test]
    fn stop_failure_and_post_tool_use_failure_are_both_failures() {
        let stop_failure = parse(
            "StopFailure",
            br#"{"hook_event_name":"StopFailure","error_type":"rate_limit","error_message":"Rate limit exceeded"}"#,
        )
        .expect("must parse");
        assert_eq!(stop_failure.fact, HookFact::TurnFailed);
        assert_eq!(
            stop_failure.question.as_deref(),
            Some("Rate limit exceeded")
        );

        let tool_failure = parse(
            "PostToolUseFailure",
            br#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_input":{"command":"npm test"},"error":"Command timed out after 120 seconds"}"#,
        )
        .expect("must parse");
        assert_eq!(tool_failure.fact, HookFact::TurnFailed);
        assert_eq!(tool_failure.activity.as_deref(), Some("Bash"));
        assert_eq!(
            tool_failure.question.as_deref(),
            Some("Command timed out after 120 seconds")
        );
    }

    #[test]
    fn session_start_and_user_prompt_submit_are_working() {
        assert_eq!(
            parse(
                "SessionStart",
                br#"{"hook_event_name":"SessionStart","source":"startup"}"#
            )
            .map(|report| report.fact),
            Some(HookFact::Working)
        );
        assert_eq!(
            parse(
                "UserPromptSubmit",
                br#"{"hook_event_name":"UserPromptSubmit","user_input":"do the thing"}"#
            )
            .map(|report| report.fact),
            Some(HookFact::Working)
        );
    }

    #[test]
    fn malformed_oversized_and_unknown_input_is_ignored_rather_than_trusted() {
        // Untrusted bytes off a socket: none of these may panic, and none may produce a fact.
        assert_eq!(parse("PreToolUse", b"not json at all"), None);
        assert_eq!(parse("PreToolUse", b""), None);
        assert_eq!(parse("PreToolUse", b"{\"truncated\":"), None);
        // Valid JSON of the wrong shape - a bare array, a string, a null. These matter most for
        // the events whose meaning is the event name alone: without the object check, any of
        // them would be enough to forge a turn boundary.
        assert_eq!(parse("PreToolUse", b"[]"), None);
        assert_eq!(parse("Stop", b"\"just a string\""), None);
        assert_eq!(parse("Stop", b"[]"), None);
        assert_eq!(parse("Stop", b"null"), None);
        assert_eq!(parse("StopFailure", b"12345"), None);
        // A `PreToolUse` with no `tool_name` has nothing to report on.
        assert_eq!(
            parse("PreToolUse", br#"{"tool_input":{"command":"x"}}"#),
            None
        );
        // An event Jerry does not act on.
        assert_eq!(
            parse("PreCompact", br#"{"hook_event_name":"PreCompact"}"#),
            None
        );
        assert_eq!(parse("", b"{}"), None);
        // Oversized bodies are refused before the JSON parser is ever handed them.
        let huge = vec![b'x'; MAX_PAYLOAD_BYTES + 1];
        assert_eq!(parse("PreToolUse", &huge), None);
    }

    #[test]
    fn long_and_multiline_text_is_truncated_on_a_char_boundary_and_flattened() {
        // A real `Write` of a long path, and a multi-line error - both must come out as one
        // bounded single-line string.
        let long_path = "x".repeat(500);
        let payload = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{{"file_path":"{long_path}"}}}}"#
        );
        let report = parse("PreToolUse", payload.as_bytes()).expect("must parse");
        let activity = report.activity.expect("must have activity");
        assert_eq!(activity.chars().count(), ACTIVITY_MAX_CHARS);
        assert!(activity.ends_with('\u{2026}'));

        let multiline = parse(
            "StopFailure",
            br#"{"hook_event_name":"StopFailure","error_message":"line one\nline two\tline three"}"#,
        )
        .expect("must parse");
        assert_eq!(
            multiline.question.as_deref(),
            Some("line one line two line three"),
            "a multi-line message must flatten, not render as its first line only"
        );
    }

    #[test]
    fn multibyte_text_truncates_without_panicking_or_splitting_a_char() {
        // `truncated` slices by `char`, not by byte - a byte slice would panic mid-codepoint on
        // exactly this input, and hook payloads are untrusted.
        let emoji_path = "\u{1f600}".repeat(300);
        let payload = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{{"file_path":"{emoji_path}"}}}}"#
        );
        let report = parse("PreToolUse", payload.as_bytes()).expect("must parse");
        let activity = report.activity.expect("must have activity");
        assert_eq!(activity.chars().count(), ACTIVITY_MAX_CHARS);
    }

    #[test]
    fn a_blank_field_is_absence_of_information_not_an_empty_row() {
        let report = parse(
            "StopFailure",
            br#"{"hook_event_name":"StopFailure","error_message":"   "}"#,
        )
        .expect("must parse");
        assert_eq!(report.question, None);
    }
}
