//! File-authorship and "what is it doing" activity heuristics for Revision R12's rail rework
//! (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md`, §4 "Where numbers live" and
//! §2.3's `writing auth.rs` / `bench 3 of 5` activity examples).
//!
//! ## Why this is a heuristic, not a structured signal
//!
//! The design doc's framing - "Jerry supervises the agent processes and sees every edit tool
//! call" - describes an *exact* per-tool-call attribution: Jerry would know, precisely, that
//! session `s1`'s `Write` tool call touched `auth.rs` at time `t`. Building that for real means
//! parsing each agent CLI's own structured output (Claude Code's and Codex's transcript/event
//! formats are different, undocumented-for-third-parties, and change across CLI versions) -
//! real per-CLI integration work, explicitly out of scope for this v1 (see this crate's own
//! `crate::rail`/`crate::terminal` split: `TerminalPane` only ever sees raw bytes, never
//! structured tool-call events).
//!
//! What's built here instead is an **approximation** from two signals every session already
//! produces for free, regardless of which CLI is running: *when did this session's pty last
//! produce output* (already tracked by `crate::terminal::pane::TerminalPane` for the rail's
//! Run/Ask status heuristic, `crate::rail::status`) and *what did it recently print*
//! (`TerminalPane::recent_output_text`, a small tap added alongside that same tracking - see its
//! docs). [`attribute_authorship`] correlates the first against a changed file's mtime;
//! [`extract_activity`] pattern-matches the second against a short list of common CLI-agent
//! output shapes. Both are "often right, gracefully absent when unclear", not exact - see each
//! function's own docs for precisely where they can be wrong. A real per-CLI structured
//! integration is the documented upgrade path, replacing this module's two functions with exact
//! equivalents while (ideally) keeping the same `HashMap<PathBuf, Vec<SessionId>>` /
//! `Option<String>` output shapes this module already committed to.
//!
//! ## Clock domain: everything is a [`Duration`] measured from "now"
//!
//! A file's mtime comes from the filesystem as a [`std::time::SystemTime`] (wall clock); a
//! session's last-output moment is tracked as a [`std::time::Instant`] (monotonic clock, see
//! `TerminalPane::activity_at`'s docs). The two are different clock domains with no `std`
//! conversion between them, so this module never compares them directly. Instead every input is
//! pre-reduced by its caller to "how long ago, as of now" - a plain [`Duration`] - using each
//! signal's *own* clock (`Instant::elapsed()` for sessions, `SystemTime::now().duration_since`
//! for files, see [`file_change_age`]). Two elapsed-since-now durations, even from different
//! underlying clocks, are directly and meaningfully comparable; two absolute timestamps from
//! different clocks are not. This is also why the pure correlation logic below takes plain
//! [`SessionActivity`]/[`FileChange`] structs instead of `Instant`/`SystemTime` fields - it keeps
//! the unit tests free of real sleeps or wall-clock fakery, since a `Duration` literal is exactly
//! as easy to construct as a real one to observe.
//!
//! This module is deliberately GPUI-free and pty/process-free, the same split
//! `crate::rail::status`'s own docs describe for the same reason: gathering the real signals
//! from a live `TerminalPane`/the filesystem is a caller's job (`crate::rail::state`/
//! `crate::root`, once the parallel data-model task in this rework lands - see this module's
//! own top-level task notes), not this module's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use regex::Regex;

use crate::work_surface::sessions::SessionId;

/// How close a session's last-output moment must be to a changed file's mtime (in either
/// direction) to attribute that file to that session.
///
/// 12 seconds. Reasoning:
/// - The poll loop that drains pty output (`crate::terminal::pane`'s `POLL_INTERVAL`/
///   `BACKGROUND_POLL_INTERVAL`, 8ms/33ms) adds only sub-frame latency, so it contributes
///   nothing meaningful to this budget - the real slack has to cover the *agent's* behavior,
///   not this app's polling.
/// - A tool-call summary line ("Writing auth.rs") is typically printed within a second or two
///   of the actual disk write either side of it (before, if the CLI announces then writes;
///   after, if it writes then confirms) - normal latency here is low single-digit seconds.
/// - `crate::rail::status::AGENT_ASK_IDLE_THRESHOLD` (15s) is this codebase's own existing
///   judgment call for "how long is normal agent-CLI pause latency before something looks
///   wrong" - 12s sits just under that, deliberately: a session already flagged `Ask` (quiet
///   past 15s) is unlikely to still be the honest author of a file that just changed.
/// - Too wide a window (e.g. 30s+) risks crediting a session that has simply been sitting idle
///   while an unrelated process (another agent, a background `git`/build step, the user's own
///   editor) touches the same worktree - the real ambiguous case this heuristic cannot resolve
///   perfectly even at a well-chosen threshold (see the module docs and this crate's commit
///   history for the honest limitation).
pub const CORRELATION_WINDOW: Duration = Duration::from_secs(12);

/// One session's activity snapshot, already reduced to elapsed-since-now form - see the module
/// docs' "Clock domain" section for why this isn't a raw `Instant`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActivity {
    pub session_id: SessionId,
    /// How long ago this session's pty last produced output, as of the moment this snapshot was
    /// built. Callers derive this from `TerminalPane::time_since_last_output` - deliberately
    /// *not* `TerminalPane::idle_duration`, which goes `None` once the process exits; a session
    /// that just finished writing a file and exited must still be attributable (see
    /// `time_since_last_output`'s own docs).
    pub idle: Duration,
    /// Recently observed terminal output text (lossy-UTF8, oldest-first) - callers derive this
    /// from `TerminalPane::recent_output_text`. Used only by [`extract_activity`]; ignored by
    /// [`attribute_authorship`].
    pub recent_text: String,
}

/// One changed file, already reduced to "how long ago its mtime was" - see the module docs'
/// "Clock domain" section. Build with [`file_change_age`] for a real file, or directly for
/// tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    /// How long ago this file's mtime was, as of the moment this snapshot was built.
    pub age: Duration,
}

/// Reduces a real file's filesystem mtime to a [`FileChange`], as of `now`. Returns `None` if
/// the file's metadata can't be read (deleted mid-check, permissions, etc. - a real,
/// non-hardcoded outcome: no synthetic age is invented for a file this can't observe) or if the
/// platform doesn't support `mtime` at all, or if the mtime is somehow in the future relative to
/// `now` (clock skew/adjustment - `SystemTime::duration_since` returns `Err` rather than a
/// negative duration in that case; treated the same as "can't observe" rather than clamped to
/// zero, since a fabricated zero would claim false confidence).
pub fn file_change_age(path: &Path, now: SystemTime) -> Option<FileChange> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    Some(FileChange {
        path: path.to_path_buf(),
        age,
    })
}

/// Attributes each changed file to whichever session(s) had recent output "close enough"
/// (within [`CORRELATION_WINDOW`], either side) to that file's mtime - the heuristic behind the
/// design's `by: 's1'` / `by: ['s1', 's9']` change-row attribution
/// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §4).
///
/// Symmetric on purpose: a session's last-output moment can land *either* just before a file's
/// mtime (the common "print summary, then flush to disk" ordering) *or* just after (a CLI that
/// writes first and only then prints a confirmation) - this module has no reliable way to know
/// which ordering a given CLI uses, so it doesn't try to pick one direction.
///
/// A file with no session inside the window is simply absent from the returned map (never an
/// empty `Vec` entry) - "graceful absence" per the module docs, so callers can treat "no key" and
/// "no author known" as the same thing without a second check. A file within the window of more
/// than one session gets every one of them, sorted by [`SessionId`] for a deterministic result
/// independent of input order - this is the multi-author case the design's `⚠` shared-file
/// warning is built on, and it is a real, expected outcome of two agents active near the same
/// moment, not a bug in this function.
pub fn attribute_authorship(
    files: &[FileChange],
    sessions: &[SessionActivity],
) -> HashMap<PathBuf, Vec<SessionId>> {
    let mut result = HashMap::with_capacity(files.len());
    for file in files {
        let mut authors: Vec<SessionId> = sessions
            .iter()
            .filter(|session| duration_abs_diff(session.idle, file.age) <= CORRELATION_WINDOW)
            .map(|session| session.session_id)
            .collect();
        if authors.is_empty() {
            continue;
        }
        authors.sort_unstable();
        authors.dedup();
        result.insert(file.path.clone(), authors);
    }
    result
}

fn duration_abs_diff(a: Duration, b: Duration) -> Duration {
    a.abs_diff(b)
}

/// Verbs matched by [`extract_activity`]'s "verb + filename" pattern - gerund forms, since
/// that's the shape both a plain narrated CLI ("Writing auth.rs...") and a human paraphrase of a
/// tool call ("editing reports.rs") tend to take. Deliberately a small, curated list rather than
/// "any `-ing` word" - `Reasoning`, `Thinking`, `Checking` (used as a bare status word with no
/// filename) would otherwise combine with an unrelated nearby filename-looking token and produce
/// a confident-looking but false pairing.
const ACTIVITY_VERBS: &[&str] = &[
    "writing",
    "editing",
    "reading",
    "creating",
    "updating",
    "deleting",
    "removing",
    "running",
    "testing",
    "building",
    "opening",
    "viewing",
    "modifying",
    "formatting",
    "generating",
    "applying",
    "patching",
    "indexing",
    "searching",
    "fetching",
    "compiling",
    "linting",
];

fn verb_filename_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            let verbs = ACTIVITY_VERBS.join("|");
            // A "filename" here is deliberately loose (anything with a `.ext`-shaped suffix,
            // not `terminal::links`'s curated extension allow-list) - activity text is a much
            // lower-stakes surface than a clickable link (worst case: a slightly odd-looking
            // string next to an agent row, not a wrong navigation target), so this favors
            // recall over the stricter precision that allow-list buys.
            let pattern = format!(
                r"(?i)\b(?P<verb>{verbs})\b\s+(?:to\s+|in\s+|from\s+)?(?P<file>[\w][\w./-]*\.[A-Za-z0-9]{{1,8}})\b"
            );
            match Regex::new(&pattern) {
                Ok(regex) => Some(regex),
                Err(err) => {
                    log::error!("rail::authorship: verb/filename pattern failed to compile: {err}");
                    None
                }
            }
        })
        .as_ref()
}

fn progress_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            // An optional leading context word ("bench 3 of 5"), then `<num> of <num>` or
            // `<num>/<num>` ("148 of 312", "148/312"). Known false-positive class, accepted as
            // a heuristic trade-off (documented in the module docs): a bare `n/m`-shaped token
            // that isn't actually a progress counter (a version string, a fraction in prose)
            // will also match - there's no purely textual way to tell those apart from a real
            // progress fraction without much more context than a short recent-output window
            // gives.
            let pattern =
                r"(?i)\b(?:(?P<word>[A-Za-z][A-Za-z0-9_-]*)\s+)?(?P<num>\d{1,6})\s*(?P<sep>of|/)\s*(?P<den>\d{1,6})\b";
            match Regex::new(pattern) {
                Ok(regex) => Some(regex),
                Err(err) => {
                    log::error!("rail::authorship: progress pattern failed to compile: {err}");
                    None
                }
            }
        })
        .as_ref()
}

/// Best-effort "what is it doing right now" activity string for the rail's agent-row line 2
/// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.3: `writing auth.rs`,
/// `editing reports.rs`, `bench 3 of 5`, `148 of 312`), derived from a session's recently
/// observed terminal output text.
///
/// Two shapes are matched (see [`ACTIVITY_VERBS`] and the progress pattern's own docs for exactly
/// what each accepts):
/// 1. A gerund verb followed by a filename-shaped token - `writing auth.rs`, `editing
///    reports.rs`.
/// 2. A progress fraction, optionally with a leading context word - `bench 3 of 5`,
///    `148 of 312`, `148/312`.
///
/// When both shapes appear in `recent_text`, the one whose match starts *later* in the text
/// wins - later means more recently printed, since `recent_text` is oldest-first (see
/// [`SessionActivity::recent_text`]'s docs) - so this reports the most recent plausible activity,
/// not just the first pattern that happens to match anywhere in the buffer.
///
/// Returns `None` when neither pattern matches - the documented "gracefully absent" half of this
/// module's accuracy contract (see the module docs): callers should render nothing rather than a
/// guess when this returns `None`, exactly as the design's `needs input` row already renders no
/// trailing text (§2.3's table).
pub fn extract_activity(recent_text: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;

    if let Some(regex) = verb_filename_regex() {
        if let Some(m) = regex.captures_iter(recent_text).last() {
            let start = m.get(0).map(|whole| whole.start()).unwrap_or(0);
            let verb = m.name("verb")?.as_str().to_lowercase();
            let file = m.name("file")?.as_str();
            best = Some((start, format!("{verb} {file}")));
        }
    }

    if let Some(regex) = progress_regex() {
        if let Some(m) = regex.captures_iter(recent_text).last() {
            let start = m.get(0).map(|whole| whole.start()).unwrap_or(0);
            let num = m.name("num")?.as_str();
            let den = m.name("den")?.as_str();
            // Preserve the separator actually printed (`of` vs `/`) rather than normalizing
            // both to "of" - `148/312` and `148 of 312` are both real shapes CLI progress bars
            // use and this reports back whichever one was seen, not a canonicalized guess.
            let core = if m.name("sep")?.as_str() == "/" {
                format!("{num}/{den}")
            } else {
                format!("{num} of {den}")
            };
            let text = match m.name("word") {
                Some(word) => format!("{} {core}", word.as_str().to_lowercase()),
                None => core,
            };
            let is_better = match &best {
                Some((best_start, _)) => start > *best_start,
                None => true,
            };
            if is_better {
                best = Some((start, text));
            }
        }
    }

    best.map(|(_, text)| text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: SessionId, idle_secs: u64) -> SessionActivity {
        SessionActivity {
            session_id: id,
            idle: Duration::from_secs(idle_secs),
            recent_text: String::new(),
        }
    }

    fn file(name: &str, age_secs: u64) -> FileChange {
        FileChange {
            path: PathBuf::from(name),
            age: Duration::from_secs(age_secs),
        }
    }

    // --- attribute_authorship ---------------------------------------------------------------

    #[test]
    fn a_session_whose_last_output_lands_right_on_the_files_mtime_is_attributed() {
        let files = vec![file("auth.rs", 3)];
        let sessions = vec![session(1, 3)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("auth.rs")), Some(&vec![1]));
    }

    #[test]
    fn a_session_whose_output_preceded_the_mtime_within_the_window_is_attributed() {
        // Output 4s before "now", file changed 10s before "now" - i.e. the file was written
        // roughly 6s after the session's last observed output. Within CORRELATION_WINDOW (12s).
        let files = vec![file("auth.rs", 10)];
        let sessions = vec![session(1, 4)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("auth.rs")), Some(&vec![1]));
    }

    #[test]
    fn a_session_whose_output_followed_the_mtime_within_the_window_is_also_attributed() {
        // Symmetric: output slightly *after* the file's mtime (write-then-confirm ordering)
        // must attribute too - see attribute_authorship's own docs for why this is symmetric.
        let files = vec![file("auth.rs", 10)];
        let sessions = vec![session(1, 15)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("auth.rs")), Some(&vec![1]));
    }

    #[test]
    fn a_session_idle_well_outside_the_window_is_not_attributed() {
        let files = vec![file("auth.rs", 3)];
        let sessions = vec![session(1, 3 + CORRELATION_WINDOW.as_secs() + 30)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("auth.rs")), None);
    }

    #[test]
    fn a_file_with_no_plausible_author_is_absent_from_the_map_not_an_empty_vec() {
        let files = vec![file("auth.rs", 1000)];
        let sessions = vec![session(1, 1)];
        let result = attribute_authorship(&files, &sessions);
        assert!(!result.contains_key(&PathBuf::from("auth.rs")));
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn two_sessions_both_plausibly_recent_are_both_attributed_and_sorted() {
        let files = vec![file("shared.rs", 5)];
        let sessions = vec![session(9, 4), session(1, 6)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(
            result.get(&PathBuf::from("shared.rs")),
            Some(&vec![1, 9]),
            "must be sorted by session id regardless of input order, for a deterministic result"
        );
    }

    #[test]
    fn exactly_at_the_window_boundary_is_still_attributed_inclusive() {
        let files = vec![file("auth.rs", 0)];
        let sessions = vec![session(1, CORRELATION_WINDOW.as_secs())];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("auth.rs")), Some(&vec![1]));
    }

    #[test]
    fn one_second_past_the_window_boundary_is_not_attributed() {
        let files = vec![file("auth.rs", 0)];
        let sessions = vec![session(1, CORRELATION_WINDOW.as_secs() + 1)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("auth.rs")), None);
    }

    #[test]
    fn multiple_files_are_attributed_independently() {
        let files = vec![file("near.rs", 2), file("far.rs", 1000)];
        let sessions = vec![session(1, 2)];
        let result = attribute_authorship(&files, &sessions);
        assert_eq!(result.get(&PathBuf::from("near.rs")), Some(&vec![1]));
        assert_eq!(result.get(&PathBuf::from("far.rs")), None);
    }

    #[test]
    fn no_sessions_means_nothing_is_attributed() {
        let files = vec![file("auth.rs", 1)];
        let result = attribute_authorship(&files, &[]);
        assert!(result.is_empty());
    }

    // --- file_change_age ----------------------------------------------------------------------

    #[test]
    fn file_change_age_returns_none_for_a_file_that_does_not_exist() {
        let now = SystemTime::now();
        let result = file_change_age(Path::new("/nonexistent/path/for/authorship/test"), now);
        assert_eq!(result, None);
    }

    #[test]
    fn file_change_age_reads_a_real_files_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "ade-authorship-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir for test");
        let path = dir.join("touched.txt");
        std::fs::write(&path, b"hello").expect("write temp file for test");

        let now = SystemTime::now();
        let result = file_change_age(&path, now).expect("real file must yield a FileChange");
        assert_eq!(result.path, path);
        // A file just written should read as very recently modified - generous bound to absorb
        // filesystem timestamp resolution and CI scheduling jitter, not exact-zero.
        assert!(
            result.age < Duration::from_secs(30),
            "expected a just-written file's age to be small, got {:?}",
            result.age
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- extract_activity: verb + filename -----------------------------------------------------

    #[test]
    fn extracts_writing_a_filename() {
        assert_eq!(
            extract_activity("Writing auth.rs..."),
            Some("writing auth.rs".to_string())
        );
    }

    #[test]
    fn extracts_editing_a_filename_case_insensitively() {
        assert_eq!(
            extract_activity("EDITING reports.rs\n"),
            Some("editing reports.rs".to_string())
        );
    }

    #[test]
    fn extracts_a_filename_with_a_preposition_between_verb_and_path() {
        assert_eq!(
            extract_activity("writing to src/auth/session.rs"),
            Some("writing src/auth/session.rs".to_string())
        );
    }

    #[test]
    fn prefers_the_most_recently_printed_verb_filename_match() {
        let text = "reading config.toml\nsome other output\nwriting auth.rs";
        assert_eq!(extract_activity(text), Some("writing auth.rs".to_string()));
    }

    // --- extract_activity: progress fraction ---------------------------------------------------

    #[test]
    fn extracts_a_bare_progress_fraction() {
        assert_eq!(
            extract_activity("148 of 312"),
            Some("148 of 312".to_string())
        );
    }

    #[test]
    fn extracts_a_progress_fraction_with_a_context_word() {
        assert_eq!(
            extract_activity("bench 3 of 5"),
            Some("bench 3 of 5".to_string())
        );
    }

    #[test]
    fn extracts_a_slash_progress_fraction() {
        assert_eq!(
            extract_activity("148/312 tests passed"),
            Some("148/312".to_string())
        );
    }

    #[test]
    fn prefers_whichever_pattern_matched_most_recently_across_both_shapes() {
        let text = "bench 3 of 5\nwriting auth.rs";
        assert_eq!(extract_activity(text), Some("writing auth.rs".to_string()));

        let text_reversed = "writing auth.rs\nbench 3 of 5";
        assert_eq!(
            extract_activity(text_reversed),
            Some("bench 3 of 5".to_string())
        );
    }

    // --- extract_activity: graceful absence ----------------------------------------------------

    #[test]
    fn returns_none_for_text_with_no_recognizable_pattern() {
        assert_eq!(extract_activity(""), None);
        assert_eq!(extract_activity("Thinking...\n"), None);
        assert_eq!(
            extract_activity("Anthropic released Claude Sonnet 4.5 this week"),
            None
        );
    }

    #[test]
    fn a_bare_reasoning_verb_with_no_filename_does_not_match() {
        // "Reasoning"/"Checking" alone (no trailing filename-shaped token) must not produce a
        // false pairing with some unrelated word later in the same buffer - see
        // ACTIVITY_VERBS's own docs for why the verb list is curated rather than "any -ing word".
        assert_eq!(extract_activity("Checking dependencies now"), None);
    }
}
