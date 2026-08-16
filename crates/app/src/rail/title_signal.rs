//! Reading an agent CLI's own window title as a coarse status signal (GitHub issue #239).

/// A coarse read of what an agent CLI's window title is claiming about itself.
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
        assert_eq!(classify_title("日本語のタイトル"), TitleSignal::Unknown);
        assert_eq!(classify_title("\u{25c7} 準備完了"), TitleSignal::Idle);
        assert_eq!(classify_title("……ready……"), TitleSignal::Idle);
    }
}
