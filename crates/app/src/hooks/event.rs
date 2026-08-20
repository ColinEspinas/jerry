//! Turning one raw Claude Code hook payload into the small, already-decided fact Jerry's rail
//! needs (GitHub issue #239, phase 2).

use std::time::Duration;

/// How long a hook fact keeps outranking the pty-quiescence and terminal-title heuristics before
/// Jerry falls back to them - see [`crate::rail::status::HookSignal`] for how the fallback works.
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

/// Longest [`HookReport::prompt`] Jerry will keep - GitHub issue #227's run title.
pub const PROMPT_MAX_CHARS: usize = 120;

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

/// Whether an event is a real lifecycle **transition** or merely a **nudge** re-announcing a
/// state the agent is already in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A real lifecycle event: the agent moved from one state to another, and this report is the
    /// whole truth about where it is now. Everything except `Notification`.
    Transition,
    /// A `Notification` that announces a *block* (`permission_prompt`, `agent_needs_input`,
    /// `elicitation_dialog`). It may be the first Jerry hears of the block, so it can still move a
    /// finished agent back to [`HookFact::NeedsInput`] - it just must not overwrite the richer
    /// question a `PermissionRequest` already gave for the same block.
    BlockedNudge,
    /// The `idle_prompt` `Notification`: "you have not typed for a while". It carries no state at
    /// all beyond what a turn boundary already said, so it must never overwrite one.
    IdleNudge,
}

/// Whether an edit event fired *before* the agent wrote, or *after*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPhase {
    /// `PreToolUse`: the agent is about to write this file.
    Before,
    /// `PostToolUse`: the agent has written this file.
    After,
}

/// A file an agent's tool call is about to write, or has just written (GitHub issue #284).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditedFile {
    pub phase: EditPhase,
    /// Exactly the path string the payload carried, **not** normalised here. Every real capture
    /// on this machine held an absolute path, but a relative one is a real shape too (see
    /// `crate::hooks::server`'s own tests), and which worktree it belongs to is not a question
    /// this module can answer - `crate::provenance::flow` resolves it against the agent's own
    /// `cwd`, which is the only place both halves are known.
    pub path: String,
    /// The payload's own `cwd`, present on every real Claude Code event. This is what a relative
    /// `path` would be relative to.
    pub cwd: Option<String>,
}

/// The tools whose whole purpose is to write a file, and the `tool_input` key each puts the path
/// under.
const EDITING_TOOLS: [(&str, &str); 4] = [
    ("Edit", "file_path"),
    ("Write", "file_path"),
    ("MultiEdit", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

/// The file a `PreToolUse`/`PostToolUse` payload says is being written, if its tool writes files
/// at all.
fn edited_file(tool: &str, value: &serde_json::Value, phase: EditPhase) -> Option<EditedFile> {
    let key = EDITING_TOOLS
        .iter()
        .find_map(|(name, key)| (*name == tool).then_some(*key))?;
    let path = value
        .get("tool_input")?
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())?;
    Some(EditedFile {
        phase,
        path: path.to_owned(),
        cwd: value
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .filter(|cwd| !cwd.trim().is_empty())
            .map(str::to_owned),
    })
}

/// One parsed hook event, reduced to exactly what the rail row needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    /// Whether this event is a real transition or a re-announcement - see [`EventKind`].
    pub kind: EventKind,
    /// The state fact - see [`HookFact`].
    pub fact: HookFact,
    /// Trailing "what it is doing" text for a running row, roughly `"{tool}: {argument}"`, already
    /// truncated to [`ACTIVITY_MAX_CHARS`]. `None` for events that carry no tool context.
    pub activity: Option<String>,
    /// The real permission reason / notification message, already truncated to
    /// [`QUESTION_MAX_CHARS`]. `None` unless the event actually carries human-facing text.
    pub question: Option<String>,
    /// The real Claude Code `session_id` this payload carried, if any (GitHub issue #227).
    pub session_id: Option<String>,
    /// The file this tool call writes, for GitHub issue #284's per-agent line provenance. `None`
    /// for every event that is not a file-writing tool call - which is most of them.
    pub edit: Option<EditedFile>,
    /// The literal text the human typed, off a `UserPromptSubmit` payload's own `prompt` field,
    /// truncated to [`PROMPT_MAX_CHARS`] (GitHub issue #227). `None` for every other event.
    pub prompt: Option<String>,
}

impl HookReport {
    /// A report carrying only a state fact - the common case for the turn-boundary events, which
    /// have no text worth rendering (see the module docs on `last_assistant_message`).
    ///
    /// `pub(crate)`: `crate::hooks::cursor_event` builds every one of its own reports off this
    /// too, rather than repeating the same all-`None` literal for a second agent kind.
    pub(crate) fn bare(fact: HookFact) -> HookReport {
        HookReport {
            kind: EventKind::Transition,
            fact,
            activity: None,
            question: None,
            session_id: None,
            edit: None,
            prompt: None,
        }
    }
}

/// Truncates on a real `char` boundary, appending an ellipsis only when something was actually
/// cut. Returns `None` for text that is empty or whitespace-only, so a present-but-blank JSON
/// field is treated as the absence of information rather than rendered as an empty row.
///
/// `pub(crate)`: `crate::hooks::cursor_event` reuses this for the same truncation, rather than a
/// second implementation of the same rule.
pub(crate) fn truncated(text: &str, max_chars: usize) -> Option<String> {
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
fn tool_input_preview(tool_input: &serde_json::Value) -> Option<&str> {
    const KEYS: [&str; 6] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "description",
    ];
    if let Some(flat) = KEYS
        .iter()
        .find_map(|key| tool_input.get(key).and_then(serde_json::Value::as_str))
    {
        return Some(flat);
    }
    // `header` first, then `question`: this feeds the *activity* line, which
    // [`ACTIVITY_MAX_CHARS`] documents as a label rather than a sentence, and `header` is
    // precisely that - Claude Code's own schema calls it a "very short label displayed as a
    // chip/tag", with "Auth method", "Library", "Approach" as its examples.
    let question = first_question(tool_input)?;
    ["header", "question"]
        .iter()
        .find_map(|key| question.get(key).and_then(serde_json::Value::as_str))
}

/// The first entry of an `AskUserQuestion` `tool_input.questions` array.
fn first_question(tool_input: &serde_json::Value) -> Option<&serde_json::Value> {
    tool_input
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .and_then(|questions| questions.first())
}

/// The full question sentence an `AskUserQuestion` is blocking on, for the rail's *question* slot.
fn asked_question(tool_input: &serde_json::Value) -> Option<&str> {
    let question = first_question(tool_input)?;
    ["question", "header"]
        .iter()
        .find_map(|key| question.get(key).and_then(serde_json::Value::as_str))
}

/// Whether a `Notification`'s `notification_type` really means "a human is being waited on".
fn notification_wants_human(notification_type: &str) -> bool {
    matches!(
        notification_type,
        "permission_prompt" | "idle_prompt" | "agent_needs_input" | "elicitation_dialog"
    )
}

/// Parses one hook event into the fact Jerry acts on, or `None` if this event carries nothing
/// worth changing a row over.
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
        // Both mean "mid-turn, working". `UserPromptSubmit` additionally carries the real text
        // the human typed, which is GitHub issue #227's run title - see
        // [`HookReport::prompt`]. `SessionStart` carries no such field, so it stays bare.
        "SessionStart" => Some(HookReport::bare(HookFact::Working)),

        "UserPromptSubmit" => Some(HookReport {
            prompt: value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .and_then(|prompt| truncated(prompt, PROMPT_MAX_CHARS)),
            ..HookReport::bare(HookFact::Working)
        }),

        "PreToolUse" | "PostToolUse" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            let argument = value.get("tool_input").and_then(tool_input_preview);
            let activity = match argument {
                Some(argument) => truncated(&format!("{tool}: {argument}"), ACTIVITY_MAX_CHARS),
                None => truncated(tool, ACTIVITY_MAX_CHARS),
            };
            let phase = match event_name {
                "PreToolUse" => EditPhase::Before,
                _ => EditPhase::After,
            };
            Some(HookReport {
                kind: EventKind::Transition,
                fact: HookFact::Working,
                activity,
                question: None,
                session_id: None,
                edit: edited_file(tool, &value, phase),
                prompt: None,
            })
        }

        // A failed tool call is a real failure signal, but it is *not* the end of the turn -
        // Claude Code routinely recovers from one and keeps working. The activity text is kept so
        // the row can say which tool broke.
        "PostToolUseFailure" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            Some(HookReport {
                kind: EventKind::Transition,
                fact: HookFact::TurnFailed,
                activity: truncated(tool, ACTIVITY_MAX_CHARS),
                question: value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|error| truncated(error, QUESTION_MAX_CHARS)),
                session_id: None,
                edit: None,
                prompt: None,
            })
        }

        // A real permission prompt: the agent is blocked until a human answers. The tool and its
        // argument are the question - "Bash: sudo reboot" is what the human is being asked about.
        //
        // `AskUserQuestion` is the exception, and not a cosmetic one. It fires a real
        // `PermissionRequest` (verified against a real 2.1.228 session), but it is not asking to
        // be *allowed* to do something - the tool's entire purpose is to put a multiple-choice
        // question to the human, and that question is already a complete sentence written for
        // them to read. Wrapping it as "AskUserQuestion needs permission: Which date library
        // should we use?" would be both redundant and wrong about what is being asked. The rail
        // shows the question itself.
        "PermissionRequest" => {
            let tool = value.get("tool_name").and_then(serde_json::Value::as_str)?;
            let tool_input = value.get("tool_input");
            if let Some(asked) = tool_input.and_then(asked_question) {
                return Some(HookReport {
                    kind: EventKind::Transition,
                    fact: HookFact::NeedsInput,
                    activity: None,
                    question: truncated(asked, QUESTION_MAX_CHARS),
                    session_id: None,
                    edit: None,
                    prompt: None,
                });
            }
            let argument = tool_input.and_then(tool_input_preview);
            let question = match argument {
                Some(argument) => truncated(
                    &format!("{tool} needs permission: {argument}"),
                    QUESTION_MAX_CHARS,
                ),
                None => truncated(&format!("{tool} needs permission"), QUESTION_MAX_CHARS),
            };
            Some(HookReport {
                kind: EventKind::Transition,
                fact: HookFact::NeedsInput,
                activity: None,
                question,
                session_id: None,
                edit: None,
                prompt: None,
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
                // `idle_prompt` is the one type that says nothing a turn boundary hasn't already
                // said - see [`EventKind`] for the real behaviour this distinction was found in.
                kind: match notification_type {
                    "idle_prompt" => EventKind::IdleNudge,
                    _ => EventKind::BlockedNudge,
                },
                fact: HookFact::NeedsInput,
                activity: None,
                question: value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|message| truncated(message, QUESTION_MAX_CHARS)),
                session_id: None,
                edit: None,
                prompt: None,
            })
        }

        "Stop" => Some(HookReport::bare(HookFact::TurnEnded)),

        "StopFailure" => Some(HookReport {
            kind: EventKind::Transition,
            fact: HookFact::TurnFailed,
            activity: None,
            question: value
                .get("error_message")
                .and_then(serde_json::Value::as_str)
                .and_then(|message| truncated(message, QUESTION_MAX_CHARS)),
            session_id: None,
            edit: None,
            prompt: None,
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

    /// The real `PreToolUse` a real `claude` 2.1.228 wrote when it invoked `AskUserQuestion`,
    /// captured verbatim (only `transcript_path`/`session_id` shortened).
    const REAL_PRE_TOOL_USE_ASK: &[u8] = br#"{"session_id":"6ca8b423","cwd":"/tmp/capture","prompt_id":"826cb806","permission_mode":"default","effort":{"level":"high"},"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which date library should we use?","header":"Date lib","options":[{"label":"date-fns","description":"Modular, tree-shakeable pure functions operating on native Date objects. Larger API surface, no wrapper object."},{"label":"dayjs","description":"Tiny (~2KB) immutable wrapper with a Moment-compatible chainable API. Plugin-based for extras like timezones."}],"multiSelect":false}]},"tool_use_id":"toolu_01ACQnZZPUuATtRma6f1iv9e"}"#;

    /// The real `PermissionRequest` that followed it milliseconds later, from the same capture -
    /// same `tool_name` and same `tool_input`, no `tool_use_id`.
    const REAL_PERMISSION_REQUEST_ASK: &[u8] = br#"{"session_id":"6ca8b423","cwd":"/tmp/capture","prompt_id":"826cb806","permission_mode":"default","effort":{"level":"high"},"hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which date library should we use?","header":"Date lib","options":[{"label":"date-fns","description":"Modular, tree-shakeable pure functions operating on native Date objects. Larger API surface, no wrapper object."},{"label":"dayjs","description":"Tiny (~2KB) immutable wrapper with a Moment-compatible chainable API. Plugin-based for extras like timezones."}],"multiSelect":false}]}}"#;

    #[test]
    fn a_real_ask_user_question_says_what_it_is_asking_rather_than_just_its_own_name() {
        // The regression this exists for: `AskUserQuestion` nests its content under
        // `questions[0]`, so the flat top-level lookup found nothing and the rail rendered the
        // bare string "AskUserQuestion" - at exactly the moment the agent is blocked on the human
        // and the row most needs to say what it is blocked *on*.
        let pre = parse("PreToolUse", REAL_PRE_TOOL_USE_ASK).expect("real payload must parse");
        assert_eq!(
            pre.activity.as_deref(),
            Some("AskUserQuestion: Date lib"),
            "the activity line is a label, so it takes the `header` chip"
        );
        assert_eq!(pre.fact, HookFact::Working);

        // The blocked row's question is the real sentence the human has to answer - not
        // "AskUserQuestion needs permission: ...", which is redundant and wrong about what is
        // being asked. This tool is not requesting permission to act, it *is* the question.
        let permission = parse("PermissionRequest", REAL_PERMISSION_REQUEST_ASK)
            .expect("real payload must parse");
        assert_eq!(permission.fact, HookFact::NeedsInput);
        assert_eq!(
            permission.question.as_deref(),
            Some("Which date library should we use?")
        );
        assert!(
            !permission
                .question
                .as_deref()
                .is_some_and(|q| q.contains("needs permission")),
            "a question is not a permission request"
        );
    }

    #[test]
    fn an_ordinary_permission_request_still_names_the_tool_and_its_argument() {
        // The `AskUserQuestion` special case must not have swallowed the general shape: a tool
        // that really is asking to be allowed to act still reads as one.
        let payload = br#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"sudo reboot"}}"#;
        let report = parse("PermissionRequest", payload).expect("must parse");
        assert_eq!(
            report.question.as_deref(),
            Some("Bash needs permission: sudo reboot")
        );
    }

    #[test]
    fn a_malformed_questions_array_falls_back_instead_of_panicking() {
        // `questions` is model-authored, so every shape below is reachable: empty array, wrong
        // type, missing text. None may panic, and none may invent a preview.
        for input in [
            r#"{"questions":[]}"#,
            r#"{"questions":"not-an-array"}"#,
            r#"{"questions":[{}]}"#,
            r#"{"questions":[{"question":42}]}"#,
            r#"{"questions":[null]}"#,
        ] {
            let payload = format!(
                r#"{{"hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_input":{input}}}"#
            );
            let report = parse("PermissionRequest", payload.as_bytes()).expect("must still parse");
            assert_eq!(
                report.fact,
                HookFact::NeedsInput,
                "the agent is still blocked whatever the payload looks like"
            );
            assert_eq!(
                report.question.as_deref(),
                Some("AskUserQuestion needs permission"),
                "an unreadable question must fall back, never guess: {input}"
            );
        }
    }

    #[test]
    fn a_real_captured_write_names_the_file_it_is_about_to_write_and_the_one_it_just_wrote() {
        // GitHub issue #284's whole input signal, off the real captured payload above. The two
        // phases are not interchangeable: `Before` is the only chance to see what the file looked
        // like *before* the agent touched it, and `After` is the only moment the new content is
        // really on disk.
        let before = parse("PreToolUse", REAL_PRE_TOOL_USE_WRITE).expect("real payload must parse");
        assert_eq!(
            before.edit,
            Some(EditedFile {
                phase: EditPhase::Before,
                path: "/tmp/capture/done.txt".to_string(),
                cwd: Some("/tmp/capture".to_string()),
            })
        );

        // The same real body arrives again as the matching `PostToolUse` - verified against a real
        // `claude` 2.1.228, which sends the identical `tool_input` on both.
        let after = parse("PostToolUse", REAL_PRE_TOOL_USE_WRITE).expect("real payload must parse");
        assert_eq!(
            after.edit.as_ref().map(|edit| edit.phase),
            Some(EditPhase::After)
        );
    }

    #[test]
    fn every_file_writing_tool_is_recognised_and_nothing_else_is() {
        // The allow-list is the whole guard: `Read`/`Grep`/`Glob` all carry a `file_path` or
        // `path` too, and attributing on one of those would hand an agent every line of every file
        // it merely looked at.
        for (tool, key) in [
            ("Edit", "file_path"),
            ("Write", "file_path"),
            ("MultiEdit", "file_path"),
            ("NotebookEdit", "notebook_path"),
        ] {
            let payload = format!(
                r#"{{"cwd":"/wt","hook_event_name":"PostToolUse","tool_name":"{tool}","tool_input":{{"{key}":"src/main.rs"}}}}"#
            );
            let report = parse("PostToolUse", payload.as_bytes())
                .unwrap_or_else(|| panic!("{tool} must parse"));
            assert_eq!(
                report.edit.map(|edit| edit.path),
                Some("src/main.rs".to_string()),
                "{tool} really writes files"
            );
        }

        for (tool, key) in [
            ("Read", "file_path"),
            ("Grep", "path"),
            ("Glob", "path"),
            ("Bash", "command"),
            ("mcp__memory__store", "file_path"),
        ] {
            let payload = format!(
                r#"{{"cwd":"/wt","hook_event_name":"PostToolUse","tool_name":"{tool}","tool_input":{{"{key}":"src/main.rs"}}}}"#
            );
            let report = parse("PostToolUse", payload.as_bytes())
                .unwrap_or_else(|| panic!("{tool} must parse"));
            assert_eq!(
                report.edit, None,
                "{tool} changes nothing, so it must attribute nothing"
            );
        }
    }

    #[test]
    fn a_write_event_with_no_usable_path_carries_no_edit_rather_than_an_empty_one() {
        for tool_input in [
            r#"{}"#,
            r#"{"file_path":""}"#,
            r#"{"file_path":"   "}"#,
            r#"{"file_path":42}"#,
            r#"{"notebook_path":"x.ipynb"}"#,
        ] {
            let payload = format!(
                r#"{{"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{tool_input}}}"#
            );
            let report = parse("PostToolUse", payload.as_bytes()).expect("must parse");
            assert_eq!(report.edit, None, "{tool_input}");
        }
        // No `cwd` at all is a real shape for a hand-made request: the path still stands on its
        // own if it is absolute, and the reader is told there is nothing to resolve it against.
        let report = parse(
            "PostToolUse",
            br#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":"/wt/a.txt"}}"#,
        )
        .expect("must parse");
        assert_eq!(report.edit.expect("edit").cwd, None);
    }

    #[test]
    fn a_turn_boundary_or_a_notification_carries_no_edit() {
        assert_eq!(parse("Stop", REAL_STOP).expect("parse").edit, None);
        assert_eq!(
            parse(
                "Notification",
                br#"{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"m"}"#
            )
            .expect("parse")
            .edit,
            None
        );
        assert_eq!(
            parse(
                "PostToolUseFailure",
                br#"{"hook_event_name":"PostToolUseFailure","tool_name":"Edit","tool_input":{"file_path":"a.txt"},"error":"boom"}"#
            )
            .expect("parse")
            .edit,
            None,
            "a tool call that failed did not write the file it was asked to"
        );
    }

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
    fn a_notification_is_classified_as_a_nudge_and_every_lifecycle_event_as_a_transition() {
        // The classification `crate::hooks::server::merge_nudge` acts on. Getting `idle_prompt`
        // wrong here is what erased the review boundary one minute after every finished turn, and
        // getting `permission_prompt` wrong is what replaced every real permission question with
        // a constant - see `EventKind`'s own docs for both, observed live.
        let notification = |kind: &str| {
            let payload = format!(
                r#"{{"hook_event_name":"Notification","notification_type":"{kind}","message":"m"}}"#
            );
            parse("Notification", payload.as_bytes())
                .unwrap_or_else(|| panic!("{kind} must parse"))
                .kind
        };
        assert_eq!(notification("idle_prompt"), EventKind::IdleNudge);
        for blocking in [
            "permission_prompt",
            "agent_needs_input",
            "elicitation_dialog",
        ] {
            assert_eq!(
                notification(blocking),
                EventKind::BlockedNudge,
                "{blocking}"
            );
        }

        for (event, payload) in [
            ("SessionStart", r#"{"hook_event_name":"SessionStart"}"#),
            (
                "UserPromptSubmit",
                r#"{"hook_event_name":"UserPromptSubmit"}"#,
            ),
            (
                "PreToolUse",
                r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
            ),
            (
                "PostToolUse",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
            ),
            (
                "PostToolUseFailure",
                r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","error":"exit 1"}"#,
            ),
            (
                "PermissionRequest",
                r#"{"hook_event_name":"PermissionRequest","tool_name":"Write","tool_input":{"file_path":"a.txt"}}"#,
            ),
            ("Stop", r#"{"hook_event_name":"Stop"}"#),
            (
                "StopFailure",
                r#"{"hook_event_name":"StopFailure","error_message":"boom"}"#,
            ),
        ] {
            let report =
                parse(event, payload.as_bytes()).unwrap_or_else(|| panic!("{event} must parse"));
            assert_eq!(
                report.kind,
                EventKind::Transition,
                "{event} is a real lifecycle transition and must supersede whatever came before it"
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
        assert_eq!(
            parse("PreToolUse", br#"{"tool_input":{"command":"x"}}"#),
            None
        );
        assert_eq!(
            parse("PreCompact", br#"{"hook_event_name":"PreCompact"}"#),
            None
        );
        assert_eq!(parse("", b"{}"), None);
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
