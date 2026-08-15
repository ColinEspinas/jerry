//! The two surfaces GitHub issue #288 adds: the **notes bar** above the hunks, and the **card**
//! pinned beneath a line.
//!
//! Both are transcribed from `Jerry.dc.html`'s `Review · uncommitted` state - the bar is the
//! `hasNotes` block, the card is the `l.isNote` branch of the diff row loop. Every colour is a
//! [`crate::theme::notes`] token, and every string that is design copy is quoted in the doc
//! comment beside it.

use super::{NoteAnchor, NoteMark};
use crate::provenance::render::author_style;
use crate::provenance::Author;
use crate::root::widgets::{render_keycap_row, text_tooltip, KeycapSize};
use crate::root::{plural, AdeApp};
use crate::theme;
use crate::{keymap, work_surface};
use gpui::prelude::*;
use gpui::{div, font, px, rems, ClickEvent, Context, IntoElement, SharedString};

/// The bar's fixed second line, **verbatim** from `Jerry.dc.html` and from `STAGE-A-CHANGELOG.md`
/// §1's own row for this feature: *"A notes bar above the hunks: count, the line `one prompt,
/// line-anchored · pinned after the revision`, and `Send notes to <agent>` with `⌘⏎` keycaps"*.
pub const NOTES_BAR_META: &str = "one prompt, line-anchored \u{b7} pinned after the revision";

/// The send button's tooltip, verbatim from the mock's own `title=` attribute.
pub const SEND_TOOLTIP: &str = "Send every note as one prompt to this run's agent";

/// The `crate::keymap::resolve_combo` spec behind the send button's keycaps, and behind the real
/// `crate::root::SendReviewNotes` binding - one string, so the keycaps can never advertise a
/// shortcut that is not bound. Same discipline as
/// `crate::provenance::render::AUTHOR_FILTER_SPEC`.
pub const SEND_NOTES_SPEC: &str = "mod+enter";

/// What an empty card says while it is waiting to be written into.
///
/// **A derivation, not a transcription.** `Jerry.dc.html`'s cards are all pre-authored demo data,
/// so the mock has no empty state for one at all - in it, "toggle a note" can only ever mean
/// show/hide something that already exists. A real reviewer starts from nothing, and a blank card
/// with no prompt would read as a rendering fault.
const NOTE_PLACEHOLDER: &str = "what should change on this line?";

/// The notes bar's own count sentence, and the design copy it is checked against.
///
/// A pure function, apart from the element that draws it, precisely so the exact wording is
/// assertable: `STAGE-A-CHANGELOG.md` §3's verification list is *"bar reads `1 note on this file`,
/// send → `1 note sent — awaiting revision`"*, and those two strings are the acceptance criteria.
/// A render test can prove the label painted; only this can prove it says the right thing.
///
/// Both counts go through [`plural::count`] (GitHub issue #281's helper), so `1 note` and
/// `2 notes` are the helper's answer rather than a second, inlined rule.
pub fn notes_bar_label(state: super::FileNoteState) -> String {
    let count = plural::count(state.count, "note", None);
    if state.all_sent {
        format!("{count} sent \u{2014} awaiting revision")
    } else {
        format!("{count} on this file")
    }
}

impl AdeApp {
    /// Wraps the diff's hunks with the notes bar above them (GitHub issue #288's *"batched, from
    /// the top"*).
    ///
    /// The container carries the `diff-view` key context and a real focus handle, which is what
    /// makes `mod+enter` (send) and `c` (note on line) bound keystrokes rather than decoration -
    /// the same arrangement `crate::graph_view::rebase_render`'s `rebase-plan` surface uses, and
    /// for the same reason: those hints are drawn as keycaps, so they have to be real.
    pub(crate) fn wrap_diff_with_notes(
        &self,
        path: std::path::PathBuf,
        hunks: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .track_focus(&self.diff_notes_focus_handle)
            .key_context("diff-view")
            .on_action(cx.listener(Self::handle_send_review_notes))
            .on_action(cx.listener(Self::handle_toggle_line_note))
            .children(self.render_review_notes_bar(&path, cx))
            .child(hunks)
            .into_any_element()
    }

    /// The notes bar itself. `None` - no bar at all, not an empty one - when the file carries no
    /// real note, exactly as the mock's `sc-if value="{{ hasNotes }}"` does.
    fn render_review_notes_bar(
        &self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let state = self
            .review_notes_store()
            .file_state(&self.review_notes_worktree(), path);
        if state.count == 0 {
            return None;
        }
        // `STAGE-A-CHANGELOG.md` §1: send flips the bar to *"N notes sent — awaiting revision"*.
        // It flips back the moment a sent note is edited - see `NoteStore::file_state`.
        let label = notes_bar_label(state);
        let target = self.review_note_target(path);
        let send_path = path.to_path_buf();

        Some(
            div()
                .id("diff-notes-bar")
                .debug_selector(|| "diff-notes-bar".to_string())
                .flex_none()
                .flex()
                .items_center()
                .gap(px(9.0))
                .px(px(12.0))
                .py(px(6.0))
                .bg(theme::notes::BAR_BG)
                .border_b_1()
                .border_color(theme::notes::BAR_BORDER)
                // The mock's 5px selection-blue square, which is the whole of what marks this
                // band as belonging to the notes rather than to the diff.
                .child(
                    div()
                        .flex_none()
                        .w(px(5.0))
                        .h(px(5.0))
                        .rounded(px(1.0))
                        .bg(theme::notes::EDGE),
                )
                .child(
                    div()
                        .debug_selector(|| "diff-notes-bar-label".to_string())
                        .flex_none()
                        .whitespace_nowrap()
                        .font(font(theme::font::SANS))
                        .text_size(px(11.0))
                        .text_color(theme::notes::BAR_LABEL)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::notes::BAR_META)
                        .child(NOTES_BAR_META),
                )
                // A delivery that really failed says so here, rather than leaving the bar looking
                // exactly as it did before the click.
                .children(self.note_send_error.clone().map(|message| {
                    div()
                        .debug_selector(|| "diff-notes-bar-error".to_string())
                        .flex_none()
                        .whitespace_nowrap()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::notes::BAR_ERROR)
                        .child(message)
                }))
                // The mock's `✓ sent` confirmation, next to the button that produced it.
                .when(state.all_sent, |el| {
                    el.child(
                        div()
                            .debug_selector(|| "diff-notes-bar-sent".to_string())
                            .flex_none()
                            .whitespace_nowrap()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.5))
                            .text_color(theme::notes::MARK_SENT)
                            .child("\u{2713} sent"),
                    )
                })
                .children(target.map(|target| self.render_send_notes_button(send_path, target, cx)))
                .into_any_element(),
        )
    }

    /// `Send notes to <agent>` - the agent's own chip, its name, and the `mod+enter` keycaps.
    ///
    /// Drawn only when a target really resolved. A button naming an agent that is not there is
    /// worse than no button: `REVISION-2026-08-14.md` §7's rule 1 is *"ship the affordance with
    /// the behaviour, or ship neither"*.
    fn render_send_notes_button(
        &self,
        path: std::path::PathBuf,
        target: super::flow::NoteTarget,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let macos = self.window_controls_style().is_macos();
        let (fg, bg) = work_surface::state::agent_tint(target.kind);
        let initial = work_surface::state::agent_initial(target.kind);
        let tooltip = if target.from_file_author {
            format!(
                "{SEND_TOOLTIP} \u{2014} {} wrote the lines in this file",
                target.label()
            )
        } else {
            format!(
                "{SEND_TOOLTIP} \u{2014} {} is this worktree's agent; the file names no author",
                target.label()
            )
        };
        div()
            .id("diff-notes-send")
            .debug_selector(|| "diff-notes-send".to_string())
            .flex_none()
            .h(px(22.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .gap(px(6.0))
            .bg(theme::notes::SEND_BG)
            .border_1()
            .border_color(theme::notes::SEND_BORDER)
            .hover(|style| style.bg(theme::notes::SEND_HOVER_BG))
            .tooltip(text_tooltip(tooltip))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                if let Err(err) = this.send_review_notes(path.clone(), cx) {
                    this.note_send_error = Some(err.message().to_string());
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(bg)
                    .text_color(fg)
                    .font(font(theme::font::MONO))
                    .text_size(px(8.0))
                    .child(initial),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::notes::SEND_FG)
                    .child(format!("Send notes to {}", target.label())),
            )
            .child(
                div()
                    .text_color(theme::notes::SEND_CAP_FG)
                    .child(render_keycap_row(
                        &keymap::resolve_combo(SEND_NOTES_SPEC, macos),
                        KeycapSize::Standard,
                    )),
            )
            .into_any_element()
    }

    /// One pinned note, as a row of the diff's own `uniform_list`.
    ///
    /// ## The one place this is not the mock
    ///
    /// `Jerry.dc.html` draws the card as a free-height block whose text wraps. It cannot be one
    /// here: since GitHub issue #224 the diff is a `gpui::uniform_list`, which measures item 0 and
    /// lays **every** slot out at exactly that height (`rems(1.6)`, a diff line's own line
    /// height). A taller card would be silently clipped, and a list that measured itself from a
    /// card would make every diff line as tall as one.
    ///
    /// The alternative considered and rejected was letting a note occupy N slots and painting
    /// bands of one card across them: `N` would have to be guessed from the text before layout
    /// knows the pane's width, so a wrong guess either clips a review comment or leaves a hole -
    /// and it would fail outright the moment the card's head scrolled off the top, since only
    /// visible slots are ever built.
    ///
    /// So the card is one row: the same four elements in the same order (selection-blue left edge,
    /// author chip, text, `draft`/`sent` mark), with the text kept on one line and the whole of it
    /// available as a tooltip. That is a real, stated loss against the mock's wrapping card, taken
    /// because the surface it lives in has a hard constraint the mock does not - and it is also
    /// exactly the shape of every other text input in this app, all five of which are single-line
    /// (`crate::text_history`'s own module docs).
    pub(crate) fn render_review_note_card(
        &self,
        path: std::path::PathBuf,
        anchor: NoteAnchor,
        selector_prefix: &'static str,
        note_row: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let worktree = self.review_notes_worktree();
        let note = self.review_notes_store().note(&worktree, &path, anchor);
        let editing = self
            .note_draft
            .as_ref()
            .is_some_and(|draft| draft.at.path == path && draft.at.anchor == anchor);
        let text = match (&self.note_draft, note) {
            (Some(draft), _) if editing => draft.field.as_str().to_string(),
            (_, Some(note)) => note.text.clone(),
            _ => String::new(),
        };
        let mark = note.map(|note| note.mark()).unwrap_or(NoteMark::Draft);
        // The honest half of the draft/sent history: a card that has been sent and then edited
        // can still say what the agent really was told.
        let tooltip = note
            .and_then(|note| note.superseded_text())
            .map(|sent| format!("Sent earlier, and since edited: \u{201c}{sent}\u{201d}"))
            .unwrap_or_else(|| match mark {
                NoteMark::Sent => "Sent to the agent, exactly as it reads now".to_string(),
                NoteMark::Draft => "Not sent yet \u{2014} click to edit".to_string(),
            });
        let selector = format!("{selector_prefix}-note-{note_row}");
        let text_selector = format!("{selector}-text");
        let mark_selector = format!("{selector}-mark");
        let is_blank = text.trim().is_empty();

        let body =
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector)
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .rounded(theme::radius::CARD_SM)
                .bg(theme::notes::CARD_BG)
                .border_1()
                .border_color(theme::notes::CARD_BORDER)
                // So the blue edge below is clipped by the card's own rounded corners.
                .overflow_hidden()
                .cursor_text()
                .tooltip(text_tooltip(tooltip))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    // Stops the click reaching the diff row underneath, which would toggle the note
                    // shut while the caret is being placed in it.
                    cx.stop_propagation();
                    this.focus_review_note(anchor, window, cx);
                }))
                // *"the selection-blue left edge"*. A real 2px child rather than a second border
                // colour: GPUI's `Style` carries **one** `border_color` for all four sides, so
                // `.border_l(px(2.0)).border_color(EDGE)` after `.border_color(CARD_BORDER)` would
                // have repainted the whole outline blue - the mock's `border:1px solid #2b3d4f;
                // border-left:2px solid #5a9ad4` has no single-element equivalent here. Same
                // technique the diff row's own author gutter already uses.
                .child(
                    div()
                        .flex_none()
                        .w(px(2.0))
                        .self_stretch()
                        .bg(theme::notes::EDGE),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(8.0))
                        // The note author. `you`, through the same chip vocabulary GitHub issue #287
                        // gave every other author mark in this app, rather than the mock's bare `Y`:
                        // one author is one recognisable mark wherever it appears.
                        .children(
                            author_style(&Author::You)
                                .map(|style| self.render_author_chip(&Author::You, &style)),
                        )
                        .when(editing && is_blank, |el| {
                            el.child(self.render_simple_input_caret(
                                "diff-note-caret",
                                &self.note_focus_handle,
                            ))
                        })
                        .child(
                            div()
                                .debug_selector(move || text_selector)
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font(font(theme::font::SANS))
                                .text_size(px(11.5))
                                .text_color(if is_blank {
                                    theme::notes::CARD_PLACEHOLDER
                                } else {
                                    theme::notes::CARD_FG
                                })
                                .child(if is_blank {
                                    NOTE_PLACEHOLDER.to_string()
                                } else {
                                    text
                                }),
                        )
                        .when(editing && !is_blank, |el| {
                            el.child(self.render_simple_input_caret(
                                "diff-note-caret",
                                &self.note_focus_handle,
                            ))
                        })
                        .child(
                            div()
                                .debug_selector(move || mark_selector)
                                .flex_none()
                                .whitespace_nowrap()
                                .font(font(theme::font::MONO))
                                .text_size(px(9.5))
                                .text_color(match mark {
                                    NoteMark::Draft => theme::notes::MARK_DRAFT,
                                    NoteMark::Sent => theme::notes::MARK_SENT,
                                })
                                .child(mark.label()),
                        ),
                );

        // The text-input machinery goes on **only** the card being typed into.
        //
        // Not on every card, and that is a correctness point rather than a saving: two elements
        // tracking one `FocusHandle` in the same frame put two nodes with the same `FocusId` into
        // GPUI's dispatch tree, and which of them a keystroke resolves against is then an accident
        // of traversal order. A pinned card that is not being edited is read-only text, and says
        // so by not being a `"text-input"` node at all - which is also what makes
        // `ToggleLineNote`'s `&& !text-input` conjunct mean what it says.
        let card = if editing {
            body.track_focus(&self.note_focus_handle)
                .key_context("text-input")
                .on_action(cx.listener(Self::handle_note_text_undo))
                .on_action(cx.listener(Self::handle_note_text_redo))
                .on_key_down(cx.listener(Self::handle_note_key_down))
                .into_any_element()
        } else {
            body.into_any_element()
        };

        div()
            // The slot. `rems(1.6)`, like every other item of this list, with 1px of breathing
            // room top and bottom so the card reads as a card rather than as a band.
            .h(rems(1.6))
            .py(px(1.0))
            // The mock's own inset: the card starts where the code text does, not at the gutter,
            // so it visibly belongs to the line above it.
            .pl(px(74.0))
            .pr(px(14.0))
            .flex()
            .child(card)
            .into_any_element()
    }

    /// Moves the caret into an already-pinned note without toggling it - what clicking the card
    /// itself does, as opposed to clicking the line above it.
    fn focus_review_note(
        &mut self,
        anchor: NoteAnchor,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.review_notes_file() else {
            return;
        };
        let at = super::NoteRef::new(path, anchor);
        if self.note_draft.as_ref().is_some_and(|draft| draft.at == at) {
            return;
        }
        self.open_note_draft(at, window, cx);
    }
}
