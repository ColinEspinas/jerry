//! The batched prompt: every review note on one file, composed into **one** string, in the two
//! forms a pty can actually carry it.

use super::{NoteAnchor, ReviewNote};
use crate::root::plural;
use std::path::Path;

/// The separator the single-line delivery form joins its lines with - the design's own `·`, the
/// same glyph every meta line in this app already uses to join two facts on one row.
const FLAT_SEPARATOR: &str = " \u{b7} ";

/// One batched, line-anchored prompt, as its own lines, before either delivery form is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedPrompt {
    lines: Vec<String>,
    /// What each anchor's note really says **as delivered**, in the same order. This is what
    /// `NoteStore::mark_sent_as` records, so a card can never claim to have sent wording the
    /// agent did not receive.
    delivered: Vec<(NoteAnchor, String)>,
    /// How many real notes went into it - what the bar counts and what the caller marks sent.
    pub note_count: usize,
}

impl BatchedPrompt {
    /// Composes every note on `path` into one prompt.
    pub fn compose(path: &Path, notes: &[(NoteAnchor, &ReviewNote)]) -> Option<BatchedPrompt> {
        let notes: Vec<(NoteAnchor, &ReviewNote)> = notes
            .iter()
            .filter(|(_, note)| !note.is_blank())
            .copied()
            .collect();
        if notes.is_empty() {
            return None;
        }

        let mut lines = Vec::with_capacity(notes.len() + 2);
        let mut delivered = Vec::with_capacity(notes.len());
        lines.push(format!(
            "Review notes on {} \u{2014} {}, one prompt, line-anchored.",
            sanitize(&path.display().to_string()),
            plural::count(notes.len(), "note", None)
        ));
        for (anchor, note) in &notes {
            let text = sanitize(note.text.trim());
            lines.push(format!("{}: {text}", anchor.prompt_label()));
            delivered.push((*anchor, text));
        }
        lines.push(
            "Please address every note above in one revision, then tell me what you changed."
                .to_string(),
        );

        Some(BatchedPrompt {
            note_count: notes.len(),
            delivered,
            lines,
        })
    }

    /// What each note really says as delivered - see [`Self::delivered`]'s own field docs.
    pub fn delivered(&self) -> &[(NoteAnchor, String)] {
        &self.delivered
    }

    /// The prompt's own lines, for tests and for anything that wants to show what will be sent.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The exact text to hand to the pty, given whether the target really has bracketed paste on.
    pub fn for_delivery(&self, bracketed_paste: bool) -> String {
        if bracketed_paste {
            self.lines.join("\n")
        } else {
            self.lines.join(FLAT_SEPARATOR)
        }
    }
}

/// Strips every control character out of text that is about to be typed into a terminal.
pub(super) fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn note(text: &str) -> ReviewNote {
        ReviewNote {
            text: text.to_string(),
            sent: None,
        }
    }

    fn path() -> PathBuf {
        PathBuf::from("src/api/users.rs")
    }

    #[test]
    fn a_batch_names_the_file_the_count_and_every_line_it_is_anchored_to() {
        let first = note("This drops the page argument on the floor.");
        let second = note("get_or_load caches across tenants.");
        let prompt = BatchedPrompt::compose(
            &path(),
            &[(NoteAnchor::New(5), &first), (NoteAnchor::New(13), &second)],
        )
        .expect("two real notes compose a prompt");

        assert_eq!(prompt.note_count, 2);
        assert_eq!(
            prompt.lines()[0],
            "Review notes on src/api/users.rs \u{2014} 2 notes, one prompt, line-anchored.",
            "the count goes through the pluralisation helper like every other count in the window"
        );
        assert_eq!(
            prompt.lines()[1],
            "line 5: This drops the page argument on the floor."
        );
        assert_eq!(
            prompt.lines()[2],
            "line 13: get_or_load caches across tenants."
        );
        assert!(prompt
            .lines()
            .last()
            .expect("a closing line")
            .starts_with("Please address every note above in one revision"));
    }

    #[test]
    fn a_single_note_reads_as_one_note() {
        let only = note("only one");
        let prompt = BatchedPrompt::compose(&path(), &[(NoteAnchor::New(5), &only)])
            .expect("one real note is still a prompt");
        assert!(prompt.lines()[0].contains("\u{2014} 1 note, one prompt"));
    }

    #[test]
    fn a_note_on_a_removed_line_says_removed() {
        let only = note("why did this go?");
        let prompt =
            BatchedPrompt::compose(&path(), &[(NoteAnchor::Old(7), &only)]).expect("a prompt");
        assert_eq!(prompt.lines()[1], "removed line 7: why did this go?");
    }

    #[test]
    fn nothing_real_to_send_is_no_prompt_at_all() {
        let blank = note("   ");
        assert_eq!(BatchedPrompt::compose(&path(), &[]), None);
        assert_eq!(
            BatchedPrompt::compose(&path(), &[(NoteAnchor::New(5), &blank)]),
            None,
            "a card that was opened and never written into is not a note"
        );
    }

    #[test]
    fn the_flat_delivery_form_contains_no_line_break_of_any_kind() {
        let first = note("one");
        let second = note("two");
        let prompt = BatchedPrompt::compose(
            &path(),
            &[(NoteAnchor::New(5), &first), (NoteAnchor::New(9), &second)],
        )
        .expect("a prompt");

        let flat = prompt.for_delivery(false);
        assert!(
            !flat.contains('\n') && !flat.contains('\r'),
            "without bracketed paste every newline becomes the Enter byte, so a line break here \
             would be a second submission - the exact 'one comment at a time' failure the audit \
             says makes the agent swing back and forth. Got {flat:?}"
        );
        assert!(flat.contains("line 5: one \u{b7} line 9: two"));
    }

    #[test]
    fn the_bracketed_delivery_form_keeps_one_note_per_line() {
        let first = note("one");
        let second = note("two");
        let prompt = BatchedPrompt::compose(
            &path(),
            &[(NoteAnchor::New(5), &first), (NoteAnchor::New(9), &second)],
        )
        .expect("a prompt");

        let delivered = prompt.for_delivery(true);
        assert_eq!(delivered.lines().count(), 4);
        assert!(delivered.contains("\nline 5: one\nline 9: two\n"));
    }

    #[test]
    fn control_characters_in_a_note_never_reach_the_delivery_string() {
        let nasty = note("before\x1b[201~\rrm -rf /\nafter\ttab");
        let prompt =
            BatchedPrompt::compose(&path(), &[(NoteAnchor::New(5), &nasty)]).expect("a prompt");

        for delivered in [prompt.for_delivery(true), prompt.for_delivery(false)] {
            assert!(
                !delivered.contains('\x1b'),
                "an ESC in a note would close a bracketed paste early and have the rest read as \
                 commands. Got {delivered:?}"
            );
            assert!(
                !delivered.contains('\r') && !delivered.contains('\t'),
                "and a CR would submit mid-note. Got {delivered:?}"
            );
        }
        assert_eq!(
            prompt.lines()[1],
            "line 5: before [201~ rm -rf / after tab",
            "the words survive - control characters become spaces rather than fusing two words"
        );
    }

    #[test]
    fn a_path_is_sanitised_the_same_way_the_notes_are() {
        let only = note("fine");
        let prompt = BatchedPrompt::compose(
            Path::new("src/we\x1bird.rs"),
            &[(NoteAnchor::New(1), &only)],
        )
        .expect("a prompt");
        assert!(!prompt.for_delivery(true).contains('\x1b'));
    }
}
