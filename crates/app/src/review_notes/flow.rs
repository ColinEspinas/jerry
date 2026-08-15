//! The `impl AdeApp` half of diff-line review notes (GitHub issue #288): pinning one on a line,
//! typing into it, resolving **which agent** a file's notes belong to, and the send itself.
//!
//! Nothing here draws anything (that is [`super::render`]) and nothing here decides what the
//! prompt says (that is [`super::prompt`]). What lives here is the state machine and the two
//! resolutions the issue is really about: *which line* and *which agent*.

use super::persist_state::ReviewNotesState;
use super::prompt::BatchedPrompt;
use super::{NoteAnchor, NoteRef, NoteStore};
use crate::provenance::{render::chip_authors, Author};
use crate::root::AdeApp;
use crate::text_history::TextField;
use crate::work_surface::agents::{AgentId, ProcessKind};
use gpui::{Context, Window};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// How long after the last keystroke a note is written to disk.
///
/// Sized against the same question `crate::text_history::COALESCE_IDLE` (600ms) answers - long
/// enough that an ordinary typing burst is one write, short enough that stepping away and closing
/// the window cannot lose what is on screen.
const REVIEW_NOTES_PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(600);

/// The one note currently open for typing, and its own text buffer.
///
/// **One at a time, and deliberately.** A per-note `TextField` + `FocusHandle` would have to be
/// created and destroyed as a virtualized diff row scrolls in and out of existence, and a focus
/// handle that disappears under the caret is the class of bug that is very hard to see and very
/// easy to ship. One draft, named by the note it belongs to, is enough: a reviewer writes one note
/// at a time, and the pinned cards around it are read-only text until they are clicked.
///
/// The buffer is a real [`TextField`], not a bare `String`, so a note gets the same per-widget
/// undo history every other hand-rolled input in this app has (`crate::text_history`'s own module
/// docs list the five; this is the sixth).
pub(crate) struct NoteDraft {
    /// Which **checkout** this note belongs to.
    ///
    /// Carried on the draft rather than re-read from [`AdeApp::diff_root`] at each store call,
    /// and that is a correctness point: `diff_root` is reassigned wholesale on every worktree
    /// switch (`crate::code_surface::tabs::AdeApp::load_diff`), so a draft opened in worktree A
    /// and still open after a switch to B would otherwise have every one of its writes land under
    /// B's key - overwriting B's own note on the same path and line, and persisting it there.
    pub worktree: PathBuf,
    /// Which note this buffer belongs to.
    pub at: NoteRef,
    /// Its live text.
    pub field: TextField,
}

impl NoteDraft {
    /// Whether this draft really is the one open for `worktree`'s `at` - the guard every read and
    /// write of the draft goes through, so a draft left over from another checkout matches
    /// nothing rather than matching by path alone.
    pub fn is(&self, worktree: &Path, at: &NoteRef) -> bool {
        self.worktree == worktree && &self.at == at
    }
}

/// Who a file's review notes are going to, resolved and ready to be sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteTarget {
    /// The live agent tab that will receive the prompt.
    pub agent: AgentId,
    /// Which CLI it is - what the send button's chip and label are drawn from.
    pub kind: ProcessKind,
    /// Whether this target came from the file's own attribution, or from the worktree's primary
    /// agent because the file named nobody. Only the send button's tooltip distinguishes them,
    /// but it is a real difference and the reviewer should be able to see it.
    pub from_file_author: bool,
}

impl NoteTarget {
    /// The name the send button says: `Send notes to <this>`.
    pub fn label(&self) -> &'static str {
        self.kind.label()
    }
}

/// Why a send did not happen. Every one of these is said out loud in the notes bar rather than
/// swallowed - audit item I12's whole complaint about this design is that nothing in it ever
/// fails, and a review note that silently never reached anyone is the worst possible version of
/// that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoteSendError {
    /// Nothing real to send.
    NothingToSend,
    /// No agent in this worktree to send to - the file names nobody and the worktree has no agent
    /// session open.
    NoTarget,
    /// The target agent's pty refused the write (see
    /// `crate::terminal::pane::TerminalPane::send_prompt`, which never sends hopefully).
    DeliveryFailed,
}

impl NoteSendError {
    /// What the notes bar says.
    pub fn message(&self) -> &'static str {
        match self {
            NoteSendError::NothingToSend => "nothing to send yet",
            NoteSendError::NoTarget => "no agent open in this worktree to send to",
            NoteSendError::DeliveryFailed => "the agent's terminal refused the prompt",
        }
    }
}

impl AdeApp {
    /// Which worktree the notes on screen belong to.
    ///
    /// [`Self::diff_root`] rather than the rail's selection: notes are keyed to the checkout the
    /// diff was read out of, and that is the only place the lines they are anchored to exist.
    pub(crate) fn review_notes_worktree(&self) -> PathBuf {
        self.diff_root.clone()
    }

    /// The file the notes surface is currently about, if any.
    ///
    /// [`Self::open_change`] - the **Uncommitted** diff. Notes are scoped to that one surface on
    /// purpose: `AUDIT-2026-08-13-competitive-v2.md` §3.2 puts a *second* `Send notes` on each
    /// row of the `Runs` section, with its own per-run scope and its own target, and shipping a
    /// notes bar on the Review tab too would quietly answer that question in a way §3.2 has
    /// already answered differently. One surface, one scope, until the Runs one is really built.
    pub(crate) fn review_notes_file(&self) -> Option<PathBuf> {
        self.open_change.clone()
    }

    /// Reads `review-notes.toml` back into the live store. Called once, from the constructor.
    pub(crate) fn restore_review_notes(&mut self) {
        let Some(path) = self.review_notes_path.clone() else {
            return;
        };
        let state = ReviewNotesState::load_at(&path);
        if state.worktrees.is_empty() {
            return;
        }
        self.review_notes_owned
            .extend(state.worktrees.keys().cloned());
        let (restored, discarded) = state.restore_into(&mut self.review_notes);
        if discarded > 0 {
            log::warn!(
                "{}: restored {restored} review notes, discarded {discarded} this build could \
                 not read",
                path.display()
            );
        }
    }

    /// Writes the live store back out, off-thread, merging under the shared lock.
    ///
    /// Called at every point a note's content has genuinely settled - a card closing, a batch
    /// being sent - where the write should not wait for anything.
    fn persist_review_notes(&mut self, cx: &mut Context<Self>) {
        self.write_review_notes(None, cx);
    }

    /// The same write, debounced - what a keystroke schedules.
    ///
    /// A note has to survive the app closing, and "persist when the card closes" is not that: a
    /// reviewer who types a note and then quits with the caret still in it would lose it, and
    /// losing review text is the one failure this feature cannot have. Writing per character
    /// instead would be a real `fsync`'d file write per character.
    ///
    /// So: one slot ([`AdeApp::_review_notes_persist_task`]), newest wins. Assigning a fresh task
    /// drops - and therefore cancels - whatever earlier timer was still waiting, so only the last
    /// keystroke's timer fires. Exactly the mechanism
    /// `crate::code_surface::editing::AdeApp::schedule_rehighlight` already uses, and the state is
    /// captured *inside* the task after the wait, so a coalesced write is a write of the newest
    /// content rather than of whatever was there when the first keystroke landed.
    fn schedule_review_notes_persist(&mut self, cx: &mut Context<Self>) {
        self.write_review_notes(Some(REVIEW_NOTES_PERSIST_DEBOUNCE), cx);
    }

    /// Two slots, not one: a debounced write and an immediate one are different promises. Sharing
    /// a slot would let the next keystroke's timer *cancel* an already-committed write from
    /// `close_note_draft`/`send_review_notes` before it ran.
    fn write_review_notes(&mut self, after: Option<std::time::Duration>, cx: &mut Context<Self>) {
        let Some(path) = self.review_notes_path.clone() else {
            return;
        };
        self.review_notes_owned
            .insert(crate::review::state::encode_worktree(
                &self.review_notes_worktree(),
            ));
        let task = cx.spawn(async move |this, cx| {
            if let Some(delay) = after {
                cx.background_executor().timer(delay).await;
            }
            // Captured after the wait, so a debounced write carries the newest content.
            let Ok((state, owned)) = this.read_with(cx, |this, _| {
                (
                    ReviewNotesState::capture(&this.review_notes),
                    this.review_notes_owned.clone(),
                )
            }) else {
                return;
            };
            let save_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { state.save_merged_at(&save_path, &owned) })
                .await;
            if let Err(err) = result {
                log::warn!("failed to save {}: {err}", path.display());
            }
        });
        match after {
            Some(_) => self._review_notes_debounce_task = Some(task),
            None => self._review_notes_persist_task = Some(task),
        }
    }

    /// Clicking a diff line: pin a note beneath it, or - if the click landed on the line whose
    /// note is already open for typing - close that note again.
    ///
    /// `STAGE-A-CHANGELOG.md` §1 says the gesture *toggles*, and `Jerry.dc.html` implements that
    /// against a fixed set of pre-authored notes, where "toggle" can only mean show/hide. Here the
    /// note does not exist until you make one, so toggling has to mean create/remove - with one
    /// deliberate asymmetry: a card you have **written into** is never destroyed by a click. It
    /// closes, and stays pinned. Only a card that is still blank (i.e. the click was a mistake)
    /// goes away again, which is exactly the case the toggle exists for. Emptying a note's text
    /// is the way to remove one you meant to write.
    pub(crate) fn toggle_line_note(
        &mut self,
        anchor: NoteAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.review_notes_file() else {
            return;
        };
        let at = NoteRef::new(path, anchor);
        self.note_cursor = Some(at.clone());
        let worktree = self.review_notes_worktree();
        if self
            .note_draft
            .as_ref()
            .is_some_and(|draft| draft.is(&worktree, &at))
        {
            self.close_note_draft(window, cx);
            cx.notify();
            return;
        }
        self.review_notes.begin(&worktree, &at.path, anchor);
        self.open_note_draft(at, window, cx);
    }

    /// Opens `at`'s card for typing, seeding the buffer from whatever the note already says, and
    /// moves real keyboard focus into it.
    pub(in crate::review_notes) fn open_note_draft(
        &mut self,
        at: NoteRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Whatever was open before settles first, so a half-typed blank card left behind on
        // another line does not linger.
        self.close_note_draft(window, cx);
        let worktree = self.review_notes_worktree();
        let text = self
            .review_notes
            .note(&worktree, &at.path, at.anchor)
            .map(|note| note.text.clone())
            .unwrap_or_default();
        self.note_cursor = Some(at.clone());
        self.note_draft = Some(NoteDraft {
            worktree,
            at,
            field: TextField::seeded(&text),
        });
        self.note_send_error = None;
        window.focus(&self.note_focus_handle, cx);
        cx.notify();
    }

    /// Closes whatever note is open for typing, discarding it if nothing was ever written into it,
    /// and hands keyboard focus back to the diff pane.
    ///
    /// The single removal point in the whole feature (besides emptying a note's text, which lands
    /// here too). Nothing about sending calls it.
    ///
    /// The focus hand-off is load-bearing, not tidiness. The card only carries
    /// [`Self::note_focus_handle`] while it is *being edited* (see
    /// `crate::review_notes::render::AdeApp::render_review_note_card` for why exactly one element
    /// may), so closing the draft removes that node from the dispatch tree - and a focus handle
    /// whose node is no longer rendered is GPUI's **empty context stack**, where
    /// `KeyBindingContextPredicate::eval_inner` short-circuits to `false` and every scoped binding
    /// silently dies. That is a real, repeatedly-hit bug class in this codebase (see
    /// `crate::keymap_overrides::real_context_stacks`' own docs, which include the empty stack for
    /// exactly this reason), and it would take `mod+enter` and `c` with it.
    pub(crate) fn close_note_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.note_draft.take() else {
            return;
        };
        // The draft's own worktree, never the currently-open one - see [`NoteDraft::worktree`].
        self.review_notes
            .discard_if_blank(&draft.worktree, &draft.at.path, draft.at.anchor);
        window.focus(&self.diff_notes_focus_handle, cx);
        self.persist_review_notes(cx);
    }

    /// One keystroke into the open note card - the same hand-rolled single-line input
    /// `crate::sidebar::render::AdeApp::handle_commit_message_key_down` implements, against the
    /// same [`TextField`], so a note gets the same real undo history as every other field here.
    pub(crate) fn handle_note_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.note_draft.as_mut() else {
            return;
        };
        let keystroke = &event.keystroke;
        // The same guard `crate::sidebar::render::AdeApp::handle_commit_message_key_down` uses,
        // and it is what lets `mod+enter` reach `SendReviewNotes` from inside this very field
        // rather than being typed into it.
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        let changed = match keystroke.key.as_str() {
            "backspace" => draft.field.pop(Instant::now()),
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => draft.field.push_str(text, Instant::now()),
                _ => false,
            },
        };
        if !changed {
            return;
        }
        let worktree = draft.worktree.clone();
        let at = draft.at.clone();
        let text = draft.field.as_str().to_string();
        self.review_notes
            .set_text(&worktree, &at.path, at.anchor, &text);
        self.schedule_review_notes_persist(cx);
        cx.notify();
        cx.stop_propagation();
    }

    /// Text undo/redo inside the open note card, wired exactly like the commit composer's.
    pub(crate) fn handle_note_text_undo(
        &mut self,
        _action: &crate::root::TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_note_history(|field| field.undo(), cx);
    }

    pub(crate) fn handle_note_text_redo(
        &mut self,
        _action: &crate::root::TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_note_history(|field| field.redo(), cx);
    }

    fn apply_note_history(
        &mut self,
        step: impl FnOnce(&mut TextField) -> bool,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.note_draft.as_mut() else {
            return;
        };
        if !step(&mut draft.field) {
            return;
        }
        let worktree = draft.worktree.clone();
        let at = draft.at.clone();
        let text = draft.field.as_str().to_string();
        self.review_notes
            .set_text(&worktree, &at.path, at.anchor, &text);
        self.schedule_review_notes_persist(cx);
        cx.notify();
    }

    /// **Which agent this file's notes belong to.**
    ///
    /// `STAGE-A-CHANGELOG.md` §1: *"`noteTarget` resolves to the file's first author, falling back
    /// to the worktree's primary agent - so in a shared worktree the notes go to whoever wrote the
    /// lines"*. Both halves are real here:
    ///
    /// 1. **The file's first author** is GitHub issue #287's own answer, read from the same place
    ///    the `⚠` ring reads it (`crate::provenance::render::chip_authors`, whose order is the
    ///    chip order on the row), filtered to authors that are agents and that are still open in
    ///    this worktree. `you` is skipped rather than treated as a target: the human is who the
    ///    notes are *from*.
    /// 2. **The worktree's primary agent** is `crate::work_surface::agents::Agents::primary_for_cwd`
    ///    - the same rule that decides which tab the centre pane shows for this worktree - further
    ///      filtered to a real agent session. A shell shares the worktree but cannot revise
    ///      anything, and typing a review prompt into somebody's `bash` would be both useless
    ///      and, since it would arrive at a shell prompt, actively unpleasant.
    ///
    /// `None` when there is nobody at all - and then the bar says so and the button is not drawn,
    /// rather than a button that names a target it does not have.
    pub(crate) fn review_note_target(&self, path: &Path) -> Option<NoteTarget> {
        let worktree = self.review_notes_worktree();
        if let Some(entry) = self.uncommitted_change_set.entry(path) {
            for author in chip_authors(entry) {
                let Author::Agent(key) = &author else {
                    continue;
                };
                let found = self
                    .agents
                    .iter_for_cwd(worktree.clone())
                    .filter(|agent| agent.kind.is_agent_session())
                    .find(|agent| match agent.kind {
                        ProcessKind::Agent(kind) => {
                            crate::review::state::baseline_key(
                                &agent.cwd,
                                kind,
                                agent.spawned_at_unix,
                            ) == key.as_str()
                        }
                        ProcessKind::Shell => false,
                    });
                if let Some(agent) = found {
                    return Some(NoteTarget {
                        agent: agent.id,
                        kind: agent.kind,
                        from_file_author: true,
                    });
                }
            }
        }

        let primary = self.agents.primary_for_cwd(&worktree);
        let agent = match primary {
            Some(agent) if agent.kind.is_agent_session() => Some(agent),
            // The primary tab is a shell, so fall through to this worktree's first real agent
            // session rather than refusing outright - "the worktree's primary agent" means the
            // agent it is primarily running, and a terminal open alongside it does not change
            // that.
            _ => self
                .agents
                .iter_for_cwd(worktree.clone())
                .find(|agent| agent.kind.is_agent_session()),
        }?;
        Some(NoteTarget {
            agent: agent.id,
            kind: agent.kind,
            from_file_author: false,
        })
    }

    /// The one batched prompt this file's notes would compose into right now, in diff order.
    ///
    /// `order` is the anchors as the diff view really lays them out, top to bottom - see
    /// [`Self::review_note_order`]. Falls back to plain anchor order for anything the diff no
    /// longer shows, so a note pinned to a line that has since moved out of a hunk is still
    /// delivered rather than silently dropped.
    pub(crate) fn batched_review_prompt(&self, path: &Path) -> Option<BatchedPrompt> {
        let worktree = self.review_notes_worktree();
        let mut notes = self.review_notes.deliverable(&worktree, path);
        let order = self.review_note_order(path);
        notes.sort_by_key(|(anchor, _)| {
            (
                order
                    .iter()
                    .position(|known| known == anchor)
                    .unwrap_or(usize::MAX),
                *anchor,
            )
        });
        BatchedPrompt::compose(path, &notes)
    }

    /// The anchors of `path`'s notes in the order the open diff really pins them, top to bottom.
    /// Empty when that file is not the one on screen, which is exactly when "diff order" has no
    /// meaning.
    fn review_note_order(&self, path: &Path) -> Vec<NoteAnchor> {
        let Some(file) = self.open_diff_file_cache.as_ref() else {
            return Vec::new();
        };
        if file.path != path {
            return Vec::new();
        }
        crate::code_surface::diff_view::note_anchors_in_diff_order(file)
    }

    /// **The send.** One batched prompt, into one agent's real pty, once.
    ///
    /// Everything this method does after a successful write is the issue's *"pinned after send"*
    /// half: it flips each note's mark by recording the wording that was delivered, and it does
    /// not remove anything. There is no clear-on-send path to review, because there is no code
    /// here that could clear one.
    pub(crate) fn send_review_notes(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<usize, NoteSendError> {
        // A half-typed card is part of the batch too - settle it first so what is on screen and
        // what goes down the pty cannot differ.
        if let Some(draft) = self.note_draft.as_ref() {
            let worktree = draft.worktree.clone();
            let at = draft.at.clone();
            let text = draft.field.as_str().to_string();
            self.review_notes
                .set_text(&worktree, &at.path, at.anchor, &text);
        }

        let prompt = self
            .batched_review_prompt(&path)
            .ok_or(NoteSendError::NothingToSend)?;
        let target = self
            .review_note_target(&path)
            .ok_or(NoteSendError::NoTarget)?;
        let pane = self
            .agents
            .iter()
            .find(|agent| agent.id == target.agent)
            .ok_or(NoteSendError::NoTarget)?
            .pane
            .clone();

        let delivered = pane.update(cx, |pane, cx| {
            // The form is chosen from the *target's* live terminal mode, which is the only place
            // the answer exists - see `BatchedPrompt::for_delivery` and `TerminalPane::send_prompt`
            // for why getting this wrong would turn one prompt into N.
            let text = prompt.for_delivery(pane.bracketed_paste_enabled());
            pane.send_prompt(&text, cx)
        });
        if !delivered {
            self.note_send_error = Some(NoteSendError::DeliveryFailed.message().to_string());
            cx.notify();
            return Err(NoteSendError::DeliveryFailed);
        }

        let worktree = self.review_notes_worktree();
        // Marked with what was really **delivered**, not with the raw buffer: the prompt's own
        // sanitisation collapses control characters and whitespace runs
        // (`crate::review_notes::prompt`), so storing the buffer would have the card's own
        // "sent earlier" tooltip quote wording the agent never received.
        let marked = self
            .review_notes
            .mark_sent_as(&worktree, &path, prompt.delivered());
        self.note_send_error = None;
        self.persist_review_notes(cx);
        cx.notify();
        Ok(marked)
    }

    /// `mod+enter` over the diff - the keycaps the notes bar draws, as a real binding.
    pub(crate) fn handle_send_review_notes(
        &mut self,
        _action: &crate::root::SendReviewNotes,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.review_notes_file() else {
            return;
        };
        if let Err(err) = self.send_review_notes(path, cx) {
            // `NothingToSend` deliberately says nothing: the bar only exists once the file has a
            // real note, so there is nowhere to show it now - and setting it would leave the
            // message sitting in the bar the moment a note *was* written.
            if err != NoteSendError::NothingToSend {
                self.note_send_error = Some(err.message().to_string());
                cx.notify();
            }
        }
    }

    /// `C` over the diff - *"note on line"*, the footer hint's own wording.
    ///
    /// Acts on [`Self::note_cursor`]: the line whose note you last opened, which a click sets. A
    /// keyboard-only "which line" would need a diff-line caret, and this read-only virtualized
    /// list deliberately has none (see `crate::code_surface::diff_view`); inventing a hidden one
    /// so a hint could be literal would be worse than this. With no cursor yet, `C` does nothing
    /// rather than guessing a line.
    pub(crate) fn handle_toggle_line_note(
        &mut self,
        _action: &crate::root::ToggleLineNote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cursor) = self.note_cursor.clone() else {
            return;
        };
        if self.review_notes_file().as_deref() != Some(cursor.path.as_path()) {
            return;
        }
        self.toggle_line_note(cursor.anchor, window, cx);
    }

    /// The notes on the open file, for the render layer - text, mark, and whether this one is the
    /// card currently being typed into.
    pub(crate) fn review_notes_store(&self) -> &NoteStore {
        &self.review_notes
    }
}
