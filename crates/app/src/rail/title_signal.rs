//! Reading an agent CLI's own window title as a coarse status signal (GitHub issue #239).
//!
//! GPUI-free and terminal-free: takes the plain title string
//! `crate::terminal::pane::TerminalPane::title` captured off the pty and returns a
//! [`TitleSignal`], so every classification rule below is directly `#[test]`-able without a
//! window or a child process - the same contract [`crate::rail::status`] holds.
//!
//! ## Why a terminal title is a real signal
//!
//! TUI programs have set their status into the terminal title since long before agent CLIs
//! existed - it is how `vim` shows the open file, how `ssh` shows the host, and the mechanism
//! tmux's `set-titles`/`automatic-rename` and every terminal multiplexer's window list is built
//! on. Agent CLIs reuse it the same way, so a terminal that reads the title learns what the
//! agent is doing *the moment the agent decides it*, instead of inferring it from how long the
//! pty has been quiet.
//!
//! ## What was verified on real CLIs, and what was not
//!
//! Observed directly on this machine, by driving each CLI under a real pty and logging every OSC
//! sequence it wrote:
//!
//! - **Claude Code 2.1.228** - verified across three independent sessions. It rests on
//!   `OSC 0 ; "\u{2733} <task>"` and, while actually working, alternates a two-frame half-circle
//!   spinner between `\u{25d0}` and `\u{25d1}` at roughly 1Hz. A representative capture:
//!   `\u{2733} Claude Code` at startup, `\u{25d0} Claude Code` 0.1s after the prompt was
//!   submitted, then `\u{25d0}`/`\u{25d1}` alternating for the next 16 seconds *including a
//!   9-second stretch where three `sleep 3` tool calls produced no terminal output at all*, then
//!   back to `\u{2733}` the moment it finished. That silent stretch is precisely the
//!   false-positive class `crate::rail::status`'s new busy refinement exists to fix, caught live.
//!   (The issue's research called `\u{2733}` the idle glyph and did not name the busy one; the
//!   idle half is confirmed, and `\u{25d0}`/`\u{25d1}` is the missing half.)
//! - **Gemini CLI** - emits `OSC 0 ; "\u{25c7}  Ready (<dir>)"` when idle, confirming the
//!   `\u{25c7}` idle glyph from the research. Its other three documented glyphs (`\u{2726}`
//!   working, `\u{23f2}` silently working, `\u{270b}` needs permission) were **not** reproduced:
//!   the captured sessions only ever reached the idle state. Those three are implemented from
//!   the documented set and are **unverified by observation here**.
//! - **OSC 9 / OSC 777 notifications** - neither CLI emitted one in any captured session, not
//!   even Claude Code at a live permission prompt (its notification channel is configurable and
//!   was evidently not set to a terminal-escape one). `crate::terminal::osc`'s handling of those
//!   is therefore verified against the protocol specs and its own tests, not against a live CLI.
//!
//! Everything below is therefore an explicitly heuristic layer, ordered most-specific first, and
//! it always answers [`TitleSignal::Unknown`] rather than guessing when nothing matches -
//! `crate::rail::status` treats `Unknown` as "no opinion" and falls back entirely to its own
//! quiescence heuristic.

/// A coarse read of what an agent CLI's window title is claiming about itself.
///
/// Deliberately four states and no free text: a title glyph can honestly support "busy / idle /
/// wants you", and nothing finer. Anything textual about *what* the agent is doing (tool name,
/// the actual question) needs a real structured payload, not a title scrape - see the parent
/// issue's Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSignal {
    /// The agent says it is actively working.
    Busy,
    /// The agent says it is sitting idle, done with its turn.
    Idle,
    /// The agent says it is blocked on the human - a permission prompt, a question.
    NeedsAttention,
    /// The title matched nothing recognizable (including the common case of a title that is
    /// just a shell's `user@host:path`). Callers must treat this as "no information", never as
    /// evidence of any state.
    Unknown,
}

/// Claude Code's at-rest glyph, `\u{2733}` - observed live, see the module docs.
const CLAUDE_IDLE: char = '\u{2733}'; // ✳

/// Claude Code's busy spinner. `\u{25d0}` and `\u{25d1}` were both observed alternating live;
/// `\u{25d2}` and `\u{25d3}` are the other two frames of the same contiguous half-circle set
/// (the `circleHalves` spinner in `cli-spinners`, the de-facto source for these), included so a
/// version that animates all four frames doesn't read as idle on half of them.
const CLAUDE_SPINNER_FIRST: char = '\u{25d0}'; // ◐
const CLAUDE_SPINNER_LAST: char = '\u{25d3}'; // ◓

/// Gemini CLI's four documented status glyphs. Only `\u{25c7}` (idle) was reproduced live on
/// this machine - see the module docs.
const GEMINI_WORKING: char = '\u{2726}'; // ✦
const GEMINI_SILENTLY_WORKING: char = '\u{23f2}'; // ⏲
const GEMINI_IDLE: char = '\u{25c7}'; // ◇
const GEMINI_NEEDS_PERMISSION: char = '\u{270b}'; // ✋

/// The Braille Patterns block, the near-universal spinner alphabet for CLI progress indicators
/// (the `braille` family in `cli-spinners`, which `ora`, `yaspin`, `indicatif` and effectively
/// every modern CLI spinner draw from). A title carrying one of these is animating a spinner in
/// the title bar, which only ever means "work in progress".
const BRAILLE_FIRST: char = '\u{2800}';
const BRAILLE_LAST: char = '\u{28ff}';

/// Keywords checked only after every glyph rule has missed, and only on whole words - see
/// [`has_word`].
const BUSY_WORDS: [&str; 3] = ["working", "thinking", "running"];
const IDLE_WORDS: [&str; 3] = ["ready", "idle", "done"];
const ATTENTION_WORDS: [&str; 4] = ["waiting", "permission", "approve", "confirm"];

/// Classifies a terminal title into a coarse [`TitleSignal`].
///
/// Rule order is most-specific first, and it matters: a Gemini title reads
/// `"\u{270b}  Waiting for permission (jerry)"`, where the glyph and the keywords agree, but a
/// title like `"\u{2726} finishing up, almost done"` has a working glyph and an idle *word* -
/// the glyph is the deliberate signal and the prose is incidental, so glyphs win.
///
/// 1. A recognized per-CLI status glyph anywhere in the title.
/// 2. A Braille spinner frame anywhere in the title (busy).
/// 3. Whole-word keyword match, attention first, then busy, then idle.
/// 4. [`TitleSignal::Unknown`].
pub fn classify_title(title: &str) -> TitleSignal {
    for c in title.chars() {
        match c {
            GEMINI_NEEDS_PERMISSION => return TitleSignal::NeedsAttention,
            GEMINI_WORKING | GEMINI_SILENTLY_WORKING => return TitleSignal::Busy,
            GEMINI_IDLE | CLAUDE_IDLE => return TitleSignal::Idle,
            CLAUDE_SPINNER_FIRST..=CLAUDE_SPINNER_LAST => return TitleSignal::Busy,
            BRAILLE_FIRST..=BRAILLE_LAST => return TitleSignal::Busy,
            _ => {}
        }
    }

    let lowered = title.to_lowercase();
    if ATTENTION_WORDS.iter().any(|word| has_word(&lowered, word)) {
        return TitleSignal::NeedsAttention;
    }
    if BUSY_WORDS.iter().any(|word| has_word(&lowered, word)) {
        return TitleSignal::Busy;
    }
    if IDLE_WORDS.iter().any(|word| has_word(&lowered, word)) {
        return TitleSignal::Idle;
    }
    TitleSignal::Unknown
}

/// Whether `haystack` (already lowercased) contains `word` as a whole word.
///
/// A plain `contains` is wrong here and would misfire constantly in practice: terminal titles
/// are overwhelmingly *paths*, and `~/src/already/threading.rs` contains "ready" and "reading"
/// contains neither of the words anyone meant. A word boundary is any non-alphanumeric byte -
/// so `"ready."`, `"[ready]"` and `"working…"` all match, while `"already"`, `"readyish"` and
/// `"networking"` do not.
fn has_word(haystack: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(word) {
        let start = from + offset;
        let end = start + word.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        // Advance past this occurrence's first char, not past the whole match: overlapping
        // occurrences are possible and skipping the whole match could step over a real one.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exact_gemini_idle_title_captured_from_a_real_session_reads_as_idle() {
        // Byte-for-byte what Gemini CLI wrote to a real pty on this machine (OSC 0 payload,
        // trailing padding included) - see the module docs.
        assert_eq!(
            classify_title(
                "\u{25c7}  Ready (scratchpad)                                                    "
            ),
            TitleSignal::Idle
        );
    }

    #[test]
    fn the_real_claude_code_session_transcript_classifies_end_to_end() {
        // The exact titles Claude Code 2.1.228 wrote to a real pty, in order, over one session:
        // at rest, then a spinner alternating across a 9-second stretch of silent `sleep 3` tool
        // calls, then back to rest. Nothing here is invented - see the module docs for the
        // capture. This is the whole signal, pinned as one sequence.
        let transcript = [
            ("\u{2733} Claude Code", TitleSignal::Idle),
            ("\u{25d0} Claude Code", TitleSignal::Busy),
            (
                "\u{25d0} Run sequential shell commands with delays",
                TitleSignal::Busy,
            ),
            (
                "\u{25d1} Run sequential shell commands with delays",
                TitleSignal::Busy,
            ),
            (
                "\u{2733} Run sequential shell commands with delays",
                TitleSignal::Idle,
            ),
        ];
        for (title, expected) in transcript {
            assert_eq!(classify_title(title), expected, "{title:?}");
        }
    }

    #[test]
    fn every_frame_of_claude_codes_half_circle_spinner_reads_as_busy() {
        // `\u{25d0}`/`\u{25d1}` were observed live; the other two are the rest of the same
        // contiguous set (see the constants' docs) and must not read as anything else.
        for frame in ['\u{25d0}', '\u{25d1}', '\u{25d2}', '\u{25d3}'] {
            assert_eq!(
                classify_title(&format!("{frame} doing something")),
                TitleSignal::Busy,
                "{frame:?}"
            );
        }
    }

    #[test]
    fn geminis_documented_glyph_set_maps_to_its_four_states() {
        assert_eq!(
            classify_title("\u{2726} Working (jerry)"),
            TitleSignal::Busy
        );
        assert_eq!(classify_title("\u{23f2} (jerry)"), TitleSignal::Busy);
        assert_eq!(classify_title("\u{25c7} Ready"), TitleSignal::Idle);
        assert_eq!(
            classify_title("\u{270b} Waiting for permission"),
            TitleSignal::NeedsAttention
        );
    }

    #[test]
    fn a_braille_spinner_frame_anywhere_means_busy() {
        for frame in [
            '\u{280b}', '\u{2819}', '\u{2839}', '\u{2807}', '\u{2800}', '\u{28ff}',
        ] {
            assert_eq!(
                classify_title(&format!("{frame} some-cli")),
                TitleSignal::Busy,
                "{frame:?} is a Braille spinner frame"
            );
        }
    }

    #[test]
    fn a_glyph_beats_a_contradicting_keyword() {
        assert_eq!(
            classify_title("\u{2726} almost done"),
            TitleSignal::Busy,
            "the glyph is the deliberate signal; the prose is incidental"
        );
        assert_eq!(
            classify_title("\u{270b} working on it"),
            TitleSignal::NeedsAttention
        );
    }

    #[test]
    fn keywords_match_only_on_whole_words() {
        assert_eq!(classify_title("agent: thinking"), TitleSignal::Busy);
        assert_eq!(classify_title("[Ready]"), TitleSignal::Idle);
        assert_eq!(classify_title("build done."), TitleSignal::Idle);
        assert_eq!(
            classify_title("waiting for approval"),
            TitleSignal::NeedsAttention
        );
    }

    #[test]
    fn a_path_that_merely_contains_a_keyword_as_a_substring_does_not_match() {
        // The exact false-positive class the word-boundary guard exists for: terminal titles
        // are mostly paths, and plenty of ordinary ones embed these letters.
        for title in [
            "~/src/already/main.rs",
            "vim ~/notes/readyish.md",
            "npm run networking-tests",
            "~/code/rethinking/mod.rs",
            "~/work/idleness.txt",
        ] {
            assert_eq!(
                classify_title(title),
                TitleSignal::Unknown,
                "{title:?} must not be read as a status claim"
            );
        }
    }

    #[test]
    fn an_ordinary_shell_title_is_unknown() {
        for title in [
            "colin@jerry: ~/spike/ade",
            "bash",
            "",
            "make -j8",
            "\u{2713} tests passed",
        ] {
            assert_eq!(classify_title(title), TitleSignal::Unknown, "{title:?}");
        }
    }

    #[test]
    fn classification_is_case_insensitive_for_keywords() {
        assert_eq!(classify_title("WORKING"), TitleSignal::Busy);
        assert_eq!(classify_title("Idle"), TitleSignal::Idle);
        assert_eq!(
            classify_title("PERMISSION needed"),
            TitleSignal::NeedsAttention
        );
    }

    #[test]
    fn a_word_at_either_end_of_the_title_still_matches() {
        // The boundary check has to treat "start of string" and "end of string" as boundaries,
        // not as missing characters that fail the test.
        assert_eq!(classify_title("ready"), TitleSignal::Idle);
        assert_eq!(classify_title("ready to go"), TitleSignal::Idle);
        assert_eq!(classify_title("all ready"), TitleSignal::Idle);
    }

    #[test]
    fn a_real_word_after_a_non_matching_occurrence_is_still_found() {
        // Exercises `has_word`'s scan-onward loop: the first "ready" is inside "already" and
        // must be rejected without ending the search.
        assert_eq!(classify_title("already ready"), TitleSignal::Idle);
    }

    #[test]
    fn multibyte_titles_do_not_panic_and_classify_by_their_glyph() {
        // `has_word`'s byte-offset arithmetic must never split a UTF-8 character.
        assert_eq!(classify_title("日本語のタイトル"), TitleSignal::Unknown);
        assert_eq!(classify_title("\u{25c7} 準備完了"), TitleSignal::Idle);
        assert_eq!(classify_title("……ready……"), TitleSignal::Idle);
    }
}
