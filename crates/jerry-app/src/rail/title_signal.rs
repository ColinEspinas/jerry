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

    /// The real captures this module was written against, byte for byte: Gemini CLI's idle title
    /// (OSC 0 payload, trailing padding included) and Claude Code 2.1.228's whole session
    /// transcript - at rest, a spinner alternating across a 9-second stretch of silent `sleep 3`
    /// tool calls, then back to rest. Nothing here is invented; see the module docs.
    #[test]
    fn the_real_captured_session_titles_classify_end_to_end() {
        let transcript = [
            (
                "\u{25c7}  Ready (scratchpad)                                                    ",
                TitleSignal::Idle,
            ),
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

    /// Every glyph family this module knows, and the rule that a glyph outranks contradicting
    /// prose - the glyph is the deliberate signal, the prose is incidental. `\u{25d0}`/`\u{25d1}`
    /// and the Braille frames were observed live; the rest of each contiguous set is included
    /// because it must not read as anything else (see the constants' own docs).
    #[test]
    fn a_status_glyph_decides_the_signal_whatever_the_prose_says() {
        let cases: &[(&str, TitleSignal)] = &[
            ("\u{25d0} doing something", TitleSignal::Busy),
            ("\u{25d1} doing something", TitleSignal::Busy),
            ("\u{25d2} doing something", TitleSignal::Busy),
            ("\u{25d3} doing something", TitleSignal::Busy),
            ("\u{2726} Working (jerry)", TitleSignal::Busy),
            ("\u{23f2} (jerry)", TitleSignal::Busy),
            ("\u{25c7} Ready", TitleSignal::Idle),
            (
                "\u{270b} Waiting for permission",
                TitleSignal::NeedsAttention,
            ),
            ("\u{280b} some-cli", TitleSignal::Busy),
            ("\u{2819} some-cli", TitleSignal::Busy),
            ("\u{2839} some-cli", TitleSignal::Busy),
            ("\u{2807} some-cli", TitleSignal::Busy),
            ("\u{2800} some-cli", TitleSignal::Busy),
            ("\u{28ff} some-cli", TitleSignal::Busy),
            ("\u{2726} almost done", TitleSignal::Busy),
            ("\u{270b} working on it", TitleSignal::NeedsAttention),
        ];
        for (title, expected) in cases {
            assert_eq!(classify_title(title), *expected, "{title:?}");
        }
    }

    /// Keywords match case-insensitively, and only on whole words - including at either end of
    /// the title, which the boundary check has to treat as a boundary rather than a missing
    /// character, and after an earlier non-matching occurrence, which `has_word`'s scan-onward
    /// loop has to get past ("already ready").
    #[test]
    fn a_keyword_matches_case_insensitively_on_whole_words_anywhere_in_the_title() {
        let cases: &[(&str, TitleSignal)] = &[
            ("agent: thinking", TitleSignal::Busy),
            ("[Ready]", TitleSignal::Idle),
            ("build done.", TitleSignal::Idle),
            ("waiting for approval", TitleSignal::NeedsAttention),
            ("WORKING", TitleSignal::Busy),
            ("Idle", TitleSignal::Idle),
            ("PERMISSION needed", TitleSignal::NeedsAttention),
            ("ready", TitleSignal::Idle),
            ("ready to go", TitleSignal::Idle),
            ("all ready", TitleSignal::Idle),
            ("already ready", TitleSignal::Idle),
            ("\u{2026}\u{2026}ready\u{2026}\u{2026}", TitleSignal::Idle),
        ];
        for (title, expected) in cases {
            assert_eq!(classify_title(title), *expected, "{title:?}");
        }
    }

    /// The false-positive class the word-boundary guard exists for: terminal titles are mostly
    /// paths, and plenty of ordinary ones embed these letters. An ordinary shell title - and a
    /// multibyte one, whose bytes `has_word`'s offset arithmetic must never split - claims
    /// nothing at all.
    #[test]
    fn an_ordinary_title_claims_no_status_even_when_it_embeds_the_letters() {
        for title in [
            "~/src/already/main.rs",
            "vim ~/notes/readyish.md",
            "npm run networking-tests",
            "~/code/rethinking/mod.rs",
            "~/work/idleness.txt",
            "colin@jerry: ~/spike/ade",
            "bash",
            "",
            "make -j8",
            "\u{2713} tests passed",
            "日本語のタイトル",
        ] {
            assert_eq!(
                classify_title(title),
                TitleSignal::Unknown,
                "{title:?} must not be read as a status claim"
            );
        }
    }
}
