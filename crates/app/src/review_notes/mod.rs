//! Diff-line review notes (GitHub issue #288): line-anchored comments on a diff, **batched** into
//! one prompt, delivered to a **named** agent's pty, and **kept pinned** afterwards so the
//! revision can be checked against them.
//!
//! See `docs/design/work-surface.md` for where this sits in the centre surface.
//!
//! ## The three rules, and where each one lives
//!
//! The audit is explicit that this is Orca's `Annotate AI Diff` *"including the parts they learned
//! the hard way"*, and it names three:
//!
//! 1. **Batch.** *"sending comments one at a time causes the agent to swing back and forth"*.
//!    [`prompt`] composes every note on the file into **one** string, and
//!    [`crate::terminal::pane::TerminalPane::send_prompt`] delivers it with exactly **one**
//!    submit byte. That is not a convention here, it is a checked invariant - see `send_prompt`'s
//!    own docs for the real, reachable way a multi-line payload would otherwise become N separate
//!    messages, and the refusal that stops it.
//! 2. **Send from the top of the diff.** [`render`]'s notes bar, above the hunks.
//! 3. **Keep them pinned after the revision.** [`flow`]'s send path only ever *sets*
//!    [`ReviewNote::sent`]; there is no code path anywhere in this folder that removes a note as
//!    a consequence of sending one.
//!
//! ## Layout
//!
//! Split the way every feature folder in this crate is (see `crate::provenance`'s own docs):
//!
//! - this file - the pure, GPUI-free model: what a note is, what it is anchored to, and the
//!   draft/sent state machine.
//! - [`prompt`] - the pure composition of a batch of notes into the one string that goes down the
//!   pty, including the two delivery forms and the sanitisation that keeps a note's text from
//!   ever being read as terminal control input.
//! - [`persist_state`] - `review-notes.toml`, so a note survives closing the file and the app.
//! - [`flow`] - the `impl AdeApp` glue: toggling a note on a line, typing into one, resolving the
//!   target agent, and the send itself.
//! - [`render`] - the notes bar and the pinned card. The only module here that knows a colour.

pub mod flow;
#[cfg(test)]
mod integration_tests;
pub mod persist_state;
pub mod prompt;
pub mod render;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which line of a diff a note is pinned beneath.
///
/// **Not a row index.** The design keys notes by their position in the rendered line array,
/// which is fine for a mockup whose diff never changes and fatal here: this
/// app re-reads the working tree constantly, and a row index would silently re-anchor every note
/// on a file the moment a hunk above it grew by a line. The issue's requirement is that notes are
/// *"keyed per worktree + path + line"*, and the only stable meaning of "line" in a unified diff
/// is a **file** line number.
///
/// Two variants rather than one number, because a unified diff has two line-number columns and a
/// removed line genuinely has no new-file number at all. *"Why did you delete this?"* is a real
/// review note, so removed lines have to be annotatable; collapsing both columns into one integer
/// would make old line 7 and new line 7 the same anchor, which they are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoteAnchor {
    /// A line that exists in the new file - an added line, or a context line.
    New(usize),
    /// A removed line, by its old-file number. It has no new-file number to key on.
    Old(usize),
}

impl NoteAnchor {
    /// The anchor for a diff line whose gutter numbers are `(old, new)` -
    /// `crate::sidebar::changes::hunk_line_numbers`' own output shape.
    ///
    /// New wins when both are present (a context line is a line of the file as it stands now).
    /// `None` for a line with neither, which is not a shape git produces but is what the numbers
    /// degrade to when they cannot be parsed - and a line whose own identity is unknown is a line
    /// no note can honestly be pinned to.
    pub fn from_gutter(numbers: (Option<usize>, Option<usize>)) -> Option<NoteAnchor> {
        match numbers {
            (_, Some(new)) => Some(NoteAnchor::New(new)),
            (Some(old), None) => Some(NoteAnchor::Old(old)),
            (None, None) => None,
        }
    }

    /// How this anchor names itself inside the batched prompt - the phrase the agent reads.
    pub fn prompt_label(self) -> String {
        match self {
            NoteAnchor::New(line) => format!("line {line}"),
            NoteAnchor::Old(line) => format!("removed line {line}"),
        }
    }

    /// The persisted-state key, and the debug-selector suffix. Tagged, so the two columns can
    /// never collide - the same "injective by construction, not by luck" rule
    /// `crate::review::state::encode_worktree` states for its own encoding.
    pub fn encode(self) -> String {
        match self {
            NoteAnchor::New(line) => format!("new:{line}"),
            NoteAnchor::Old(line) => format!("old:{line}"),
        }
    }

    /// The inverse of [`Self::encode`]. `None` - not a panic and not a guess - for anything this
    /// build cannot read, so a hand-edited or future-version state file costs one dropped note
    /// rather than the whole file's notes.
    pub fn decode(key: &str) -> Option<NoteAnchor> {
        let (tag, number) = key.split_once(':')?;
        let line = number.parse().ok()?;
        match tag {
            "new" => Some(NoteAnchor::New(line)),
            "old" => Some(NoteAnchor::Old(line)),
            _ => None,
        }
    }
}

/// The `draft`/`sent` mark on a card, and the word it renders as.
///
/// Derived from [`ReviewNote`] rather than stored beside it - see [`ReviewNote::mark`] for why
/// that is the whole of the issue's "editing after send starts a new draft state" requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteMark {
    /// Written, not yet delivered - or delivered and since edited.
    Draft,
    /// Delivered to an agent exactly as it currently reads.
    Sent,
}

impl NoteMark {
    /// The card's own word, verbatim design copy.
    pub fn label(self) -> &'static str {
        match self {
            NoteMark::Draft => "draft",
            NoteMark::Sent => "sent",
        }
    }
}

/// One review note.
///
/// `sent` is **the exact text that was delivered**, not a boolean, and that is the whole of the
/// issue's *"Editing after send starts a new draft state for that note"* requirement: with a
/// boolean, editing a sent note would either leave it claiming `sent` (a lie - the agent never saw
/// this wording) or clear the flag outright (also a lie - something *was* sent, and the review
/// history is supposed to stay honest). Keeping the delivered wording means the card can go back
/// to `draft` while still being able to say what the agent actually received.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewNote {
    /// What the note says right now.
    pub text: String,
    /// The exact text last delivered to an agent, if this note has ever been sent.
    pub sent: Option<String>,
}

impl ReviewNote {
    /// A note with nothing typed into it yet - what a click on a bare diff line creates.
    pub fn empty() -> ReviewNote {
        ReviewNote::default()
    }

    /// Whether this note carries any real text at all. An empty note is a click, not a note: it
    /// is never delivered, never counted, and is discarded the moment it loses focus.
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// The card's mark. `sent` **only** while what was delivered still says what this note says.
    ///
    /// Compared through [`delivered_form`] rather than by raw string equality, because the batch
    /// does not deliver the buffer verbatim - it collapses control characters and whitespace runs
    /// (`crate::review_notes::prompt`). Without that, a note containing a tab or a double space
    /// would come back reading `draft` the instant it was sent, having changed not at all.
    pub fn mark(&self) -> NoteMark {
        match self.sent.as_deref() {
            Some(sent) if sent == delivered_form(&self.text) => NoteMark::Sent,
            _ => NoteMark::Draft,
        }
    }

    /// What the agent was actually told, for a note that has been sent and then edited - the
    /// honest half of the draft/sent history, surfaced as the card's tooltip.
    pub fn superseded_text(&self) -> Option<&str> {
        match self.sent.as_deref() {
            Some(sent) if sent != delivered_form(&self.text) => Some(sent),
            _ => None,
        }
    }
}

/// How a note's text reads once the batch has composed it - the one place the model and
/// [`prompt`] agree on what "the same note" means.
///
/// Re-exported from [`prompt`] rather than reimplemented, so [`ReviewNote::mark`]'s comparison and
/// the string actually delivered can never drift apart.
pub fn delivered_form(text: &str) -> String {
    prompt::sanitize(text.trim())
}

/// One note's address inside a worktree: which file, and which line of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRef {
    pub path: PathBuf,
    pub anchor: NoteAnchor,
}

impl NoteRef {
    pub fn new(path: impl Into<PathBuf>, anchor: NoteAnchor) -> NoteRef {
        NoteRef {
            path: path.into(),
            anchor,
        }
    }
}

/// Every review note this window is holding, keyed worktree -> path -> anchor.
///
/// Nested rather than flat, for the same two reasons `crate::provenance::store` is: the persisted
/// shape is the same nesting, and every real read here is *"the notes on this one open file"*,
/// which a flat map would have to scan for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoteStore {
    worktrees: BTreeMap<PathBuf, BTreeMap<PathBuf, BTreeMap<NoteAnchor, ReviewNote>>>,
}

impl NoteStore {
    /// Every note on one file, in anchor order.
    pub fn file(&self, worktree: &Path, path: &Path) -> Option<&BTreeMap<NoteAnchor, ReviewNote>> {
        self.worktrees.get(worktree)?.get(path)
    }

    /// One note.
    pub fn note(&self, worktree: &Path, path: &Path, anchor: NoteAnchor) -> Option<&ReviewNote> {
        self.file(worktree, path)?.get(&anchor)
    }

    /// Whether a note is pinned on this line at all - the diff row's own `●` predicate, and the
    /// one read that happens per rendered row.
    pub fn has_note(&self, worktree: &Path, path: &Path, anchor: NoteAnchor) -> bool {
        self.note(worktree, path, anchor).is_some()
    }

    /// Every anchor on one file that carries a note, which is exactly what the diff view's row
    /// plan needs and all it needs.
    pub fn anchors(&self, worktree: &Path, path: &Path) -> Vec<NoteAnchor> {
        self.file(worktree, path)
            .map(|notes| notes.keys().copied().collect())
            .unwrap_or_default()
    }

    /// The notes on one file that would really be delivered - every non-blank one, in anchor
    /// order. Blank cards are clicks that have not become notes yet.
    pub fn deliverable(&self, worktree: &Path, path: &Path) -> Vec<(NoteAnchor, &ReviewNote)> {
        self.file(worktree, path)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|(_, note)| !note.is_blank())
                    .map(|(anchor, note)| (*anchor, note))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Pins an empty note on a line that has none. Returns whether one was really created, so the
    /// caller can tell "opened a new note" from "focused the one already there".
    pub fn begin(&mut self, worktree: &Path, path: &Path, anchor: NoteAnchor) -> bool {
        let file = self
            .worktrees
            .entry(worktree.to_path_buf())
            .or_default()
            .entry(path.to_path_buf())
            .or_default();
        if file.contains_key(&anchor) {
            return false;
        }
        file.insert(anchor, ReviewNote::empty());
        true
    }

    /// Writes a note's text. Creates the note if it is not there yet, so the editing path never
    /// has to care whether the click that opened it already landed.
    pub fn set_text(&mut self, worktree: &Path, path: &Path, anchor: NoteAnchor, text: &str) {
        self.worktrees
            .entry(worktree.to_path_buf())
            .or_default()
            .entry(path.to_path_buf())
            .or_default()
            .entry(anchor)
            .or_default()
            .text = text.to_string();
    }

    /// Removes a note. Returns whether one was really there.
    ///
    /// The **only** removal path in this folder, and it is reachable from exactly two places -
    /// a card that lost focus with nothing typed into it, and the explicit toggle-off of such a
    /// card. Nothing about sending calls it: *"it never clears the notes"* is structural here,
    /// not a rule the send path has to remember.
    pub fn remove(&mut self, worktree: &Path, path: &Path, anchor: NoteAnchor) -> bool {
        let Some(files) = self.worktrees.get_mut(worktree) else {
            return false;
        };
        let Some(file) = files.get_mut(path) else {
            return false;
        };
        let removed = file.remove(&anchor).is_some();
        if file.is_empty() {
            files.remove(path);
        }
        if files.is_empty() {
            self.worktrees.remove(worktree);
        }
        removed
    }

    /// Discards a note that was opened and never written into. Returns whether one really went.
    pub fn discard_if_blank(&mut self, worktree: &Path, path: &Path, anchor: NoteAnchor) -> bool {
        match self.note(worktree, path, anchor) {
            Some(note) if note.is_blank() => self.remove(worktree, path, anchor),
            _ => false,
        }
    }

    /// Records that every deliverable note on this file has just been sent, exactly as it reads
    /// now. Returns how many notes were marked.
    ///
    /// Blank cards are deliberately skipped rather than marked: nothing about them went down the
    /// pty, so nothing about them may claim to have.
    ///
    /// Real sends go through [`Self::mark_sent_as`] instead - see its docs for why the *delivered*
    /// wording, not the buffer, is what belongs in [`ReviewNote::sent`].
    pub fn mark_sent(&mut self, worktree: &Path, path: &Path) -> usize {
        let Some(file) = self
            .worktrees
            .get_mut(worktree)
            .and_then(|files| files.get_mut(path))
        else {
            return 0;
        };
        let mut marked = 0;
        for note in file.values_mut() {
            if note.is_blank() {
                continue;
            }
            note.sent = Some(note.text.clone());
            marked += 1;
        }
        marked
    }

    /// The same, but recording **what the agent actually received** for each anchor.
    ///
    /// [`ReviewNote::sent`] is documented, here and in the persisted file, as *the exact text that
    /// was delivered*, and the batched prompt does not deliver the buffer verbatim: it sanitises
    /// control characters and collapses whitespace runs
    /// (`crate::review_notes::prompt`). Marking with the buffer would make the card's own
    /// "sent earlier, and since edited" tooltip quote wording the agent never saw - which is
    /// precisely the dishonesty keeping the delivered wording exists to prevent.
    ///
    /// An anchor with no delivered entry is left alone: nothing about it went down the pty.
    pub fn mark_sent_as(
        &mut self,
        worktree: &Path,
        path: &Path,
        delivered: &[(NoteAnchor, String)],
    ) -> usize {
        let Some(file) = self
            .worktrees
            .get_mut(worktree)
            .and_then(|files| files.get_mut(path))
        else {
            return 0;
        };
        let mut marked = 0;
        for (anchor, text) in delivered {
            let Some(note) = file.get_mut(anchor) else {
                continue;
            };
            note.sent = Some(text.clone());
            marked += 1;
        }
        marked
    }

    /// The bar's own state for one file: how many real notes it carries, and whether every one of
    /// them has been delivered exactly as it currently reads.
    ///
    /// The second half is what makes the bar honest after an edit: `STAGE-A-CHANGELOG.md` §1 has
    /// the bar flip to *"N notes sent — awaiting revision"* on send, and the issue then requires
    /// that editing a sent note goes back to being a draft. A per-file boolean (which is what the
    /// mock stores) would leave the bar claiming the agent had seen wording it never saw.
    pub fn file_state(&self, worktree: &Path, path: &Path) -> FileNoteState {
        let notes = self.deliverable(worktree, path);
        FileNoteState {
            count: notes.len(),
            all_sent: !notes.is_empty()
                && notes.iter().all(|(_, note)| note.mark() == NoteMark::Sent),
        }
    }

    /// Every worktree this store holds notes for - the persistence layer's ownership set.
    pub fn worktrees(&self) -> impl Iterator<Item = &PathBuf> {
        self.worktrees.keys()
    }

    /// Every file with notes in one worktree, for persistence.
    pub fn files(
        &self,
        worktree: &Path,
    ) -> Option<&BTreeMap<PathBuf, BTreeMap<NoteAnchor, ReviewNote>>> {
        self.worktrees.get(worktree)
    }

    /// Restores one note straight from persisted state, without going through the editing path.
    pub fn restore(&mut self, worktree: &Path, path: &Path, anchor: NoteAnchor, note: ReviewNote) {
        self.worktrees
            .entry(worktree.to_path_buf())
            .or_default()
            .entry(path.to_path_buf())
            .or_default()
            .insert(anchor, note);
    }
}

/// What the notes bar needs to know about the open file - see [`NoteStore::file_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileNoteState {
    /// How many real (non-blank) notes are pinned on the file.
    pub count: usize,
    /// Whether every one of them has been delivered exactly as it currently reads.
    pub all_sent: bool,
}

/// The pure model: anchoring, the draft/sent state machine, and the two rules that are structural
/// rather than remembered (sending never removes, blank is never a note).
#[cfg(test)]
mod tests {
    use super::*;

    fn wt() -> PathBuf {
        PathBuf::from("/repo/wt-a")
    }

    fn file() -> PathBuf {
        PathBuf::from("src/api/users.rs")
    }

    /// A context line has both numbers; the new one wins, because a context line is a line of the
    /// file as it stands now and that is what an agent will go and read.
    #[test]
    fn a_line_with_both_numbers_anchors_to_the_new_file() {
        assert_eq!(
            NoteAnchor::from_gutter((Some(7), Some(9))),
            Some(NoteAnchor::New(9))
        );
        assert_eq!(
            NoteAnchor::from_gutter((None, Some(9))),
            Some(NoteAnchor::New(9))
        );
    }

    /// The half a single-integer key would have lost: a removed line is annotatable, and its
    /// anchor is a different thing from the new-file line of the same number.
    #[test]
    fn a_removed_line_anchors_to_its_old_number_and_never_collides_with_a_new_one() {
        assert_eq!(
            NoteAnchor::from_gutter((Some(7), None)),
            Some(NoteAnchor::Old(7))
        );
        assert_ne!(NoteAnchor::Old(7), NoteAnchor::New(7));
        assert_ne!(NoteAnchor::Old(7).encode(), NoteAnchor::New(7).encode());
    }

    /// A line whose own identity is unknown carries no note, rather than one pinned to a guess.
    #[test]
    fn a_line_with_no_numbers_at_all_cannot_be_anchored() {
        assert_eq!(NoteAnchor::from_gutter((None, None)), None);
    }

    #[test]
    fn an_anchor_round_trips_through_its_persisted_key_and_rejects_anything_else() {
        for anchor in [NoteAnchor::New(1), NoteAnchor::Old(4_000)] {
            assert_eq!(NoteAnchor::decode(&anchor.encode()), Some(anchor));
        }
        assert_eq!(NoteAnchor::decode("sideways:3"), None);
        assert_eq!(NoteAnchor::decode("new:three"), None);
        assert_eq!(NoteAnchor::decode("new"), None);
    }

    /// The exact acceptance sequence's state half, at the level the model decides it.
    #[test]
    fn a_note_is_draft_until_it_is_sent_and_sent_only_while_it_still_reads_that_way() {
        let mut store = NoteStore::default();
        assert!(store.begin(&wt(), &file(), NoteAnchor::New(13)));
        store.set_text(&wt(), &file(), NoteAnchor::New(13), "needs tenant_id");

        assert_eq!(
            store
                .note(&wt(), &file(), NoteAnchor::New(13))
                .expect("the note")
                .mark(),
            NoteMark::Draft
        );
        assert_eq!(
            store.file_state(&wt(), &file()),
            FileNoteState {
                count: 1,
                all_sent: false
            }
        );

        assert_eq!(store.mark_sent(&wt(), &file()), 1);
        assert_eq!(
            store
                .note(&wt(), &file(), NoteAnchor::New(13))
                .expect("the note is still there - sending never clears it")
                .mark(),
            NoteMark::Sent
        );
        assert_eq!(
            store.file_state(&wt(), &file()),
            FileNoteState {
                count: 1,
                all_sent: true
            }
        );
    }

    /// *"Editing after send starts a new draft state for that note"* - and the record of what was
    /// really delivered survives the edit, which is the half a boolean flag would have thrown away.
    #[test]
    fn editing_a_sent_note_goes_back_to_draft_without_losing_what_was_actually_sent() {
        let mut store = NoteStore::default();
        store.set_text(
            &wt(),
            &file(),
            NoteAnchor::New(5),
            "drops the page argument",
        );
        store.mark_sent(&wt(), &file());
        store.set_text(
            &wt(),
            &file(),
            NoteAnchor::New(5),
            "drops the page argument, and the limit",
        );

        let note = store
            .note(&wt(), &file(), NoteAnchor::New(5))
            .expect("the note");
        assert_eq!(
            note.mark(),
            NoteMark::Draft,
            "the agent has not seen this wording, so the card must not claim it has"
        );
        assert_eq!(
            note.superseded_text(),
            Some("drops the page argument"),
            "and what it really was told must still be recoverable - the review history stays \
             honest"
        );
        assert!(
            !store.file_state(&wt(), &file()).all_sent,
            "so the bar goes back to counting notes rather than claiming a revision is awaited"
        );
    }

    /// The requirement stated as a property of the type rather than of the send path: the store's
    /// send-side operation cannot remove anything, whatever it is handed.
    #[test]
    fn marking_a_file_sent_never_removes_a_note() {
        let mut store = NoteStore::default();
        store.set_text(&wt(), &file(), NoteAnchor::New(5), "one");
        store.set_text(&wt(), &file(), NoteAnchor::Old(7), "two");
        store.begin(&wt(), &file(), NoteAnchor::New(9)); // blank - opened, never written into

        let before = store.anchors(&wt(), &file());
        store.mark_sent(&wt(), &file());
        assert_eq!(
            store.anchors(&wt(), &file()),
            before,
            "every anchor, including the blank one, is still pinned after a send"
        );
    }

    /// A blank card is a click, not a note: it is never counted and never delivered.
    #[test]
    fn a_blank_card_is_not_a_note() {
        let mut store = NoteStore::default();
        store.begin(&wt(), &file(), NoteAnchor::New(5));
        store.set_text(&wt(), &file(), NoteAnchor::New(9), "   ");

        assert_eq!(store.file_state(&wt(), &file()).count, 0);
        assert!(store.deliverable(&wt(), &file()).is_empty());
        assert_eq!(
            store.mark_sent(&wt(), &file()),
            0,
            "and nothing about it may claim to have been sent"
        );
        assert_eq!(
            store
                .note(&wt(), &file(), NoteAnchor::New(5))
                .expect("still pinned while it is being typed into")
                .mark(),
            NoteMark::Draft
        );
    }

    #[test]
    fn discarding_a_blank_card_leaves_a_written_one_alone() {
        let mut store = NoteStore::default();
        store.begin(&wt(), &file(), NoteAnchor::New(5));
        store.set_text(&wt(), &file(), NoteAnchor::New(9), "real");

        assert!(store.discard_if_blank(&wt(), &file(), NoteAnchor::New(5)));
        assert!(!store.discard_if_blank(&wt(), &file(), NoteAnchor::New(9)));
        assert_eq!(store.anchors(&wt(), &file()), vec![NoteAnchor::New(9)]);
    }

    /// Notes on one file must never be visible on another, or in another checkout of the same
    /// path - the issue's "keyed per worktree + path + line".
    #[test]
    fn notes_are_keyed_by_worktree_and_path_together() {
        let mut store = NoteStore::default();
        store.set_text(&wt(), &file(), NoteAnchor::New(5), "here");

        assert!(store.file(Path::new("/repo/wt-b"), &file()).is_none());
        assert!(store.file(&wt(), Path::new("src/other.rs")).is_none());
        assert_eq!(store.deliverable(&wt(), &file()).len(), 1);
    }

    /// Removing the last note on a path must not leave an empty shell behind that later reads as
    /// "this file has notes".
    #[test]
    fn removing_the_last_note_leaves_no_empty_file_or_worktree_entry() {
        let mut store = NoteStore::default();
        store.set_text(&wt(), &file(), NoteAnchor::New(5), "one");
        assert!(store.remove(&wt(), &file(), NoteAnchor::New(5)));
        assert!(store.file(&wt(), &file()).is_none());
        assert_eq!(store.worktrees().count(), 0);
        assert!(!store.remove(&wt(), &file(), NoteAnchor::New(5)));
    }
}
