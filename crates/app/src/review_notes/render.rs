//! The two surfaces GitHub issue #288 adds: the **notes bar** above the hunks, and the **card**
//! pinned beneath a line.
//!
//! Both are transcribed from `Jerry.dc.html`'s `Review · uncommitted` state - the bar is the
//! `hasNotes` block, the card is the `l.isNote` branch of the diff row loop. Every colour is a
//! [`crate::theme::notes`] token, and every string that is design copy is quoted in the doc
//! comment beside it.

use super::{flow, NoteAnchor, NoteMark};
use crate::provenance::render::author_style;
use crate::provenance::Author;
use crate::root::widgets::{render_keycap_row, text_tooltip, KeycapSize, SimpleInput};
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

/// What the send control says while this worktree has no agent session to send to - see
/// [`AdeApp::render_send_notes_unavailable`].
///
/// **A derivation, not a transcription.** The mock's diff always has a run beside it, so it has
/// no empty state for this control at all; it names the *missing thing* rather than an agent
/// precisely because there is no agent to name.
pub const SEND_UNAVAILABLE_LABEL: &str = "Send notes \u{2014} no agent in this worktree";

/// And why, at the length a tooltip can afford.
pub const SEND_UNAVAILABLE_TOOLTIP: &str =
    "These notes are kept. Start an agent in this worktree and they can be sent to it as one \
     prompt.";

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
            .children(self.render_note_input_node(cx))
            .children(self.render_review_notes_bar(&path, cx))
            .child(hunks)
            .into_any_element()
    }

    /// The open note's real keyboard input node - zero-sized, and deliberately **not** inside the
    /// diff's row list.
    ///
    /// This looks like misdirection and is the opposite. The pinned card is a row of a
    /// `gpui::uniform_list`, which builds only the rows in its visible range: put the
    /// `track_focus`/`key_context`/`on_key_down` on the card and scrolling the card off screen
    /// deletes the focused node from the dispatch tree mid-sentence. GPUI then evaluates every
    /// predicate against an **empty context stack**, where
    /// `KeyBindingContextPredicate::eval_inner` short-circuits to `false` - so the keystrokes
    /// stop being typed, `mod+enter` and `c` stop firing, and nothing on screen says so. That is
    /// the exact bug class `crate::keymap_overrides::real_context_stacks` includes the empty
    /// stack for, and it is reachable here without anyone calling
    /// [`AdeApp::close_note_draft`].
    ///
    /// Anchoring the node here instead makes the input's lifetime the *draft's* lifetime rather
    /// than the scroll position's. The card keeps the caret (which only *reads*
    /// `FocusHandle::is_focused`, so it needs no node of its own) and the text; exactly one
    /// element ever tracks [`AdeApp::note_focus_handle`], which is the other half of the same
    /// rule.
    fn render_note_input_node(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        self.note_draft.as_ref()?;
        Some(
            div()
                .id("diff-note-input")
                .debug_selector(|| "diff-note-input".to_string())
                .flex_none()
                .w(px(0.0))
                .h(px(0.0))
                .track_focus(&self.note_focus_handle)
                .key_context("text-input")
                .on_action(cx.listener(Self::handle_note_text_undo))
                .on_action(cx.listener(Self::handle_note_text_redo))
                .on_key_down(cx.listener(Self::handle_note_key_down))
                .into_any_element(),
        )
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
                //
                // Filtered against what is true *now*, not merely against what happened: a
                // `NoTarget` recorded while the worktree had no agent is a statement about the
                // worktree, and starting an agent makes it false. Left unfiltered it sat in the
                // bar in red directly beside the live `Send notes to Claude` button it was
                // contradicting - observed on a real window while reproducing this feature's
                // other reports.
                .children(
                    self.note_send_error
                        .filter(|err| {
                            !matches!(err, flow::NoteSendError::NoTarget) || target.is_none()
                        })
                        .map(|err| {
                            div()
                                .debug_selector(|| "diff-notes-bar-error".to_string())
                                .flex_none()
                                .whitespace_nowrap()
                                .font(font(theme::font::MONO))
                                .text_size(px(10.5))
                                .text_color(theme::notes::BAR_ERROR)
                                .child(err.message())
                        }),
                )
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
                .child(match target {
                    Some(target) => self.render_send_notes_button(send_path, target, cx),
                    None => self.render_send_notes_unavailable(),
                })
                .into_any_element(),
        )
    }

    /// The send control when there is **nobody to send to** - drawn, muted, and saying why.
    ///
    /// Live report: *"I can't really submit the comments, maybe there is something I don't
    /// understand"*. There was nothing to understand. This bar used to render the send button
    /// only once [`AdeApp::review_note_target`] resolved a live agent session in the worktree,
    /// and to render **nothing at all** otherwise - so a reviewer who opened a diff before
    /// starting an agent (which is the ordinary order: you read the change, then you ask for a
    /// revision) got a bar that counted their notes, explained that they would be sent as one
    /// prompt, and offered no control, no keycaps, and no explanation. `mod+enter` was bound the
    /// whole time and would have said `no agent open in this worktree to send to` - but the only
    /// place that keystroke is ever advertised is *inside the button that was not being drawn*.
    ///
    /// `REVISION-2026-08-14.md` §7 rule 1 (*"ship the affordance with the behaviour, or ship
    /// neither"*) is what the original shape was reaching for, and it is not violated here: the
    /// behaviour **is** shipped - the batch, the binding, the delivery - and the only thing
    /// missing is a target the reviewer can supply in one keystroke. What that rule forbids is a
    /// control that pretends to do something it cannot, which is why this one is deliberately not
    /// clickable, carries no hover, and names the thing that is missing rather than an agent.
    fn render_send_notes_unavailable(&self) -> gpui::AnyElement {
        let macos = self.window_controls_style().is_macos();
        div()
            .id("diff-notes-send-unavailable")
            .debug_selector(|| "diff-notes-send-unavailable".to_string())
            .flex_none()
            .h(px(22.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .gap(px(6.0))
            .bg(theme::surface::CARD_SUNK)
            .border_1()
            .border_color(theme::border::CARD_FIELD)
            // Ride-along I11's rule for an unlabelled or inert control: say what would make it
            // work, in the one place the pointer is already resting.
            .tooltip(text_tooltip(SEND_UNAVAILABLE_TOOLTIP))
            .child(
                div()
                    .debug_selector(|| "diff-notes-send-unavailable-label".to_string())
                    .whitespace_nowrap()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child(SEND_UNAVAILABLE_LABEL),
            )
            // The keycaps stay. They are the only advertisement `mod+enter` has, the binding is
            // real either way, and pressing it here produces this bar's own
            // `no agent open in this worktree to send to` rather than silence.
            .child(
                div()
                    .text_color(theme::text::GHOST)
                    .child(render_keycap_row(
                        &keymap::resolve_combo(SEND_NOTES_SPEC, macos),
                        KeycapSize::Standard,
                    )),
            )
            .into_any_element()
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
                    this.note_send_error = Some(err);
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
        // Only the card actually being edited has a caret at all, so only its draft's own offset
        // is meaningful; a pinned, read-only card renders with `focus_handle: None` and never
        // paints one.
        let text_caret = match &self.note_draft {
            Some(draft) if editing => draft.field.caret(),
            _ => text.len(),
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
        // Per row, not one shared name: `debug_bounds` is keyed by selector, and two pinned cards
        // sharing `diff-note-caret` would be indistinguishable to the test that proves only the
        // open one paints a caret at all.
        let caret_selector = format!("{selector}-caret");

        let body = div()
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
                    // The caret+text pair, through the one helper that owns that structure
                    // (`AdeApp::render_simple_input_row`) rather than hand-assembled here.
                    // Hand-assembling it is what put this field's caret at the far right of
                    // the card: `.flex_1().min_w_0()` used to sit on the *text* element, so
                    // its box stretched across the whole card and the caret after it went
                    // with it. See the helper's own docs - this is the third field in this
                    // app to have shipped that exact bug.
                    //
                    // Only a card being typed into gets a caret at all; the pinned cards
                    // around it are read-only text, so they render the same row with the
                    // caret suppressed by a focus handle that is not theirs to hold.
                    .child(self.render_simple_input_row(SimpleInput {
                        caret_selector: SharedString::from(caret_selector),
                        text_selector: SharedString::from(text_selector),
                        focus_handle: editing.then_some(&self.note_focus_handle),
                        text: &text,
                        caret_offset: text_caret,
                        placeholder: NOTE_PLACEHOLDER,
                        font: theme::font::SANS,
                        text_size: px(11.5),
                        text_color: theme::notes::CARD_FG,
                        placeholder_color: theme::notes::CARD_PLACEHOLDER,
                    }))
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

        div()
            // The slot. `rems(1.6)`, like every other item of this list, with 1px of breathing
            // room top and bottom so the card reads as a card rather than as a band.
            .h(rems(1.6))
            .py(px(1.0))
            // **The card's width, and why it has to be stated.**
            //
            // Live report: *"when clicking on the line the comment popover is changing width
            // strangely"* - and it really was. A `gpui::uniform_list` lays each item out through
            // `Drawable::layout_as_root`, which is *not* the window root: the window's own root
            // goes through `TaffyLayoutEngine::stretch_auto_size_to_fill` (the thing that makes a
            // top-level `auto` width behave like the web's initial containing block), and a list
            // item never does. So an item whose own width is `auto` is sized to its **content**,
            // clamped to the pane - which made `body`'s `flex_1` below resolve against the note's
            // own text rather than against the pane, and the card grew a few pixels per keystroke
            // and jumped to a different width on every line clicked.
            //
            // `w_full` is a percentage against the definite width `uniform_list` really hands
            // each item, so the card is the pane's width less the insets below, whatever it says.
            // `Jerry.dc.html`'s own card is a block in normal flow with `margin:3px 14px 5px
            // 74px`, i.e. exactly this: full width, inset.
            .w_full()
            // The mock's own inset: the card starts where the code text does, not at the gutter,
            // so it visibly belongs to the line above it.
            .pl(px(74.0))
            .pr(px(14.0))
            .flex()
            .child(body)
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
        let worktree = self.review_notes_worktree();
        let at = super::NoteRef::new(path, anchor);
        if self
            .note_draft
            .as_ref()
            .is_some_and(|draft| draft.is(&worktree, &at))
        {
            // Already the open draft - but focus still has to be taken back, not left alone.
            // `.track_focus` makes an element focus its own handle on mouse-down, and the notes
            // container is one, so the click that got here has just moved focus off the note's
            // input node and onto the container. Returning without this would leave the caret
            // visibly in a card that no longer receives a single keystroke.
            window.focus(&self.note_focus_handle, cx);
            cx.notify();
            return;
        }
        self.open_note_draft(at, window, cx);
    }
}
