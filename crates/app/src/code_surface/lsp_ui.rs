//! The language-server UI drawn *over* Surface C: the hover card, the diagnostics card,
//! the per-severity decoration colours, and go-to-definition. The client that produces
//! the responses these draw lives in `crate::lsp`; this module is only their UI.

use super::*;
// Only this module's own tests read `LspClientState` directly now - the render path goes through
// `AdeApp::lsp_connection_for_path`'s facade instead of raw client states (see
// `crate::lsp::client::LspConnection`), so a non-test import here would be genuinely unused.
#[cfg(test)]
use crate::code_surface::fixtures::{temp_repo, wait_until_parked};
#[cfg(test)]
use crate::lsp::client::LspClientState;
use crate::root::scrollbar;
use crate::root::widgets::render_keycap;
#[cfg(test)]
use crate::test_support::open_test_app;
use std::time::Duration;

/// How long the pointer has to rest on one real token before a real `textDocument/hover` request
/// is sent for it (GitHub issue #186). Not a guessed number: `vendor/zed/crates/editor/src/
/// hover_popover.rs`'s own `hover_at` debounces on the user's `hover_popover_delay` setting, whose
/// real default in `vendor/zed/assets/settings/default.json` is `300` ms - the same value used
/// here, since this app has no per-user setting for it to read.
pub(crate) const HOVER_TRIGGER_DELAY: Duration = Duration::from_millis(300);

/// How long an already-visible [`AdeApp::hover`] card stays up after the pointer leaves its token
/// before it actually clears - the hide-side mirror of [`HOVER_TRIGGER_DELAY`]. Deliberately
/// shorter than that one (and shorter than `vendor/zed/crates/editor/src/hover_popover.rs`'s own
/// separate `hover_popover_hiding_delay` setting, whose real default is `300`ms, matching
/// [`HOVER_TRIGGER_DELAY`]) - real, reported feedback that a full 300ms hide made the whole
/// interaction feel unresponsive, lingering visibly after the pointer had genuinely moved on.
/// Still real, non-zero debounce: without *some* delay here, every real token boundary - or the
/// plain whitespace between two words on the same line - the pointer crosses while sweeping
/// toward some other target synchronously cleared an already-resolved, visible card, flashing on
/// every sweep rather than only on a deliberate re-hover.
pub(crate) const HOVER_HIDE_DELAY: Duration = Duration::from_millis(150);

impl AdeApp {
    /// Real, pointer-driven hover trigger (GitHub issue #186): arms [`HOVER_TRIGGER_DELAY`] for
    /// `anchor`, and only once that has genuinely elapsed with the pointer still on the same token
    /// does [`Self::request_hover`] actually go out. Called from [`Self::track_hover_pointer`] for
    /// every real mouse-move that lands on a real token.
    pub(in crate::code_surface) fn hover_over_token(
        &mut self,
        anchor: HoverAnchor,
        cx: &mut Context<Self>,
    ) {
        if self.hover_anchor_matches(&anchor) {
            self._hover_hide_task = None;
            return;
        }
        if self.hover_pending.as_ref() == Some(&anchor) {
            return;
        }
        self.schedule_hover_hide(cx);
        self.hover_pending = Some(anchor.clone());
        self._hover_debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOVER_TRIGGER_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                // The pointer moved on (or off) while the timer ran - `hover_pending` is either a
                // different token now or gone entirely, and either way this timer is stale.
                if this.hover_pending.as_ref() != Some(&anchor) {
                    return;
                }
                this.hover_pending = None;
                this.request_hover(
                    anchor.path.clone(),
                    anchor.line_number,
                    anchor.byte_range.clone(),
                    anchor.position,
                    cx,
                );
            });
        }));
    }

    /// Arms [`HOVER_HIDE_DELAY`] before genuinely clearing an already-visible [`Self::hover`] -
    /// the hide-side debounce; see that constant's own docs for the flash it fixes. Always clears
    /// [`Self::hover_pending`]/[`Self::_hover_debounce_task`] immediately regardless (there is no
    /// reason to keep a stale pending *request* alive once the pointer has left the token that
    /// would have triggered it - only the already-*visible* card gets the grace period).
    fn schedule_hover_hide(&mut self, cx: &mut Context<Self>) {
        self.hover_pending = None;
        self._hover_debounce_task = None;
        let Some(showing) = self.hover.as_ref().map(|entry| {
            (
                entry.path.clone(),
                entry.line_number,
                entry.byte_range.clone(),
            )
        }) else {
            return;
        };
        if self._hover_hide_task.is_some() {
            return;
        }
        self._hover_hide_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOVER_HIDE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this._hover_hide_task = None;
                let still_showing_old = this.hover.as_ref().is_some_and(|entry| {
                    (
                        entry.path.clone(),
                        entry.line_number,
                        entry.byte_range.clone(),
                    ) == showing
                });
                if still_showing_old {
                    this.hover = None;
                    cx.notify();
                }
            });
        }));
    }

    /// Whether [`Self::hover`]'s current entry (if any) is anchored on exactly `anchor`'s token.
    pub(in crate::code_surface) fn hover_anchor_matches(&self, anchor: &HoverAnchor) -> bool {
        self.hover.as_ref().is_some_and(|entry| {
            entry.path == anchor.path
                && entry.line_number == anchor.line_number
                && entry.byte_range == anchor.byte_range
        })
    }

    /// The single real dismissal path for Surface C's Hover popup (GitHub issue #186 - before it,
    /// there was none at all: an opened card only ever went away by switching tab/file/worktree).
    /// Clears the painted card, any armed [`HOVER_TRIGGER_DELAY`] timer, and any in-flight
    /// `textDocument/hover` request in one place, so no dismissal route can clear one and leave
    /// another to resurrect the card a moment later.
    pub(crate) fn dismiss_hover(&mut self) -> bool {
        let was_showing = self.hover.is_some() || self.hover_pending.is_some();
        self.hover = None;
        self.hover_pending = None;
        self.hover_card_bounds = None;
        self._hover_debounce_task = None;
        self._hover_hide_task = None;
        self._hover_request_task = None;
        was_showing
    }

    /// [`Self::dismiss_hover`] plus the `cx.notify()` a UI-driven dismissal needs - the shape
    /// every real event handler (mouse-move away, click, `Escape`) actually wants.
    pub(in crate::code_surface) fn dismiss_hover_and_notify(&mut self, cx: &mut Context<Self>) {
        if self.dismiss_hover() {
            cx.notify();
        }
    }

    /// The real per-pixel hover tracking GitHub issue #186 asks for, and the whole of it: one
    /// window-level `.on_mouse_move` (registered in [`crate::root::AdeApp::render`]) that answers
    /// "what real token, if any, is the pointer on right now" from state this app already
    /// maintains for its own click hit-testing - [`Self::file_view_row_layout`]'s real per-row
    /// `(Bounds, ShapedLine)` pairs, the same pair `crate::code_surface::editing`'s own
    /// `on_mouse_down`/drag handlers already localize a click against.
    pub(crate) fn track_hover_pointer(
        &mut self,
        event: &gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        // `hover.is_some()` as well as the bounds: `hover_card_bounds` is last frame's painted
        // rectangle, so without that gate a card that has since stopped painting would leave a
        // real dead zone the pointer could never hover a token through.
        if self.hover.is_some()
            && self
                .hover_card_bounds
                .is_some_and(|bounds| bounds.contains(&event.position))
        {
            // Definitively on the card itself - cancel any hide a moment spent off its own token
            // (crossing from the token onto the card's own chrome) might have armed.
            self._hover_hide_task = None;
            return;
        }
        // The Diagnostic card floats over the code area exactly like the Hover card does, and can
        // just as easily cover a real, different hoverable token underneath it. Without this, the
        // pointer resting on the Diagnostic card's own chrome would still resolve to that covered
        // token and call `Self::hover_over_token` for it - which, per `Self::render_diagnostic_card`'s
        // own hover-vs-diagnostic priority rule, hides the diagnostic the user is actually looking
        // at right now. `Self::diagnostic_target().is_some()` (not just the bounds alone) is the
        // same "is this stale" guard `hover_card_bounds` gets from `self.hover.is_some()` above -
        // see `Self::diagnostic_card_bounds`'s own docs for the dead-zone this prevents.
        if self.diagnostic_target().is_some()
            && self
                .diagnostic_card_bounds
                .is_some_and(|bounds| bounds.contains(&event.position))
        {
            return;
        }
        if event.pressed_button.is_some() {
            self.dismiss_hover_and_notify(cx);
            return;
        }
        let Some(anchor) = self.hover_anchor_at(event.position) else {
            self.schedule_hover_hide(cx);
            return;
        };
        self.hover_over_token(anchor, cx);
    }

    /// The real token under a real window-space pointer position, or `None` when there isn't one -
    /// the pointer is outside every painted code row, past the end of that row's real glyphs
    /// (`gpui::LineLayout::index_for_x` answers `None` for exactly that, unlike the
    /// `closest_index_for_x` the click path deliberately uses so a click past the last glyph still
    /// places a caret), on whitespace, or in a file with no ready language server to ask.
    fn hover_anchor_at(&self, position: gpui::Point<Pixels>) -> Option<HoverAnchor> {
        let relative_path = self.active_editable_path()?;
        let absolute_path = self.file_tree_root.join(&relative_path);
        // The same pure gate `render_file_view` applies before wiring any hover target at all
        // (its own `has_lsp`): a file whose extension has no LSP identity at all has genuinely
        // nothing to ask about. Deliberately *not* `lsp_connection_for_path` - whether a server
        // happens to be Ready right this instant is a race, and `Self::request_hover` already
        // degrades honestly (clears the card, sends nothing) when it isn't.
        language::lsp_language_id_for_extension(
            absolute_path.extension().and_then(|ext| ext.to_str()),
        )?;
        let buffer = self.edit_buffer(&relative_path)?;
        let (&line_number, (bounds, shaped)) = self
            .file_view_row_layout
            .iter()
            .find(|(_, (bounds, _))| bounds.contains(&position))?;
        let offset = shaped.index_for_x(position.x - bounds.left())?;
        let line = buffer.lines.get(line_number.checked_sub(1)?)?;
        let byte_range = editing::token_at_offset(&line.runs, offset)?;
        if line.text.get(byte_range.clone())?.trim().is_empty() {
            return None;
        }
        let lsp_position = hover_view::position_for_line_byte_offset(
            line_number as u32 - 1,
            &line.text,
            byte_range.start,
        );
        Some(HoverAnchor {
            path: absolute_path,
            line_number,
            byte_range,
            position: lsp_position,
        })
    }

    /// Sends a real `textDocument/hover` for one real token. `absolute_path`/`line_number`/
    /// `byte_range` identify it; `position` is the corresponding LSP `Position`, already computed
    /// by [`Self::hover_anchor_at`].
    pub(in crate::code_surface) fn request_hover(
        &mut self,
        absolute_path: PathBuf,
        line_number: usize,
        byte_range: Range<usize>,
        position: lsp_core::lsp_types::Position,
        cx: &mut Context<Self>,
    ) {
        let already_current = self.hover.as_ref().is_some_and(|entry| {
            entry.path == absolute_path
                && entry.line_number == line_number
                && entry.byte_range == byte_range
        });
        if already_current {
            return;
        }

        let Some(client) = self.lsp_connection_for_path(&absolute_path) else {
            // No ready LSP client for this file's language yet; nothing to show, so clear any
            // stale entry - a real completions popup is equally stale in that case (Revision
            // R8.5b audit finding 3), so it's dropped alongside `hover` here too.
            self.dismiss_hover();
            self.dismiss_completions();
            cx.notify();
            return;
        };

        let Ok(uri) = lsp_core::LspClient::uri_for_path(&absolute_path) else {
            self.dismiss_hover();
            self.dismiss_completions();
            cx.notify();
            return;
        };

        self.hover = Some(HoverEntry {
            path: absolute_path.clone(),
            line_number,
            byte_range: byte_range.clone(),
            position,
            status: HoverStatus::Loading,
        });
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let params = lsp_core::lsp_types::HoverParams {
                text_document_position_params: lsp_core::lsp_types::TextDocumentPositionParams {
                    text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_core::lsp_types::WorkDoneProgressParams::default(),
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    client.request::<lsp_core::lsp_types::request::HoverRequest>(
                        params,
                        LSP_QUERY_TIMEOUT,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Only apply if this is still the answer to the current click - a slower,
                // superseded request must not clobber a newer one.
                let still_current = this.hover.as_ref().is_some_and(|entry| {
                    entry.path == absolute_path
                        && entry.line_number == line_number
                        && entry.byte_range == byte_range
                });
                if !still_current {
                    return;
                }
                let status = match result {
                    Ok(Some(hover)) => {
                        HoverStatus::Ready(hover_view::build_hover_render_model(&hover))
                    }
                    Ok(None) => HoverStatus::Ready(None),
                    Err(error) => HoverStatus::Failed(error.to_string()),
                };
                this.hover = Some(HoverEntry {
                    path: absolute_path.clone(),
                    line_number,
                    byte_range: byte_range.clone(),
                    position,
                    status,
                });
                cx.notify();
            });
        });
        // A single slot, not a Vec: assigning drops (cancelling) any in-flight previous request.
        // Unlike `_goto_definition_tasks`, hover requests aren't independently concurrent.
        self._hover_request_task = Some(task);
    }

    /// The real `(absolute path, LSP position)` `F12` should ask about.
    fn goto_definition_target(&self) -> Option<(PathBuf, lsp_core::lsp_types::Position)> {
        if let Some(hover) = self.hover.as_ref() {
            return Some((hover.path.clone(), hover.position));
        }
        let relative_path = self.active_editable_path()?;
        let buffer = self.edit_buffer(&relative_path)?;
        let (line_index, byte_col) = buffer.line_col_for_offset(buffer.cursor_offset());
        let line = buffer.lines.get(line_index)?;
        Some((
            self.file_tree_root.join(&relative_path),
            hover_view::position_for_line_byte_offset(line_index as u32, &line.text, byte_col),
        ))
    }

    /// `F12`'s handler. Prefers whatever symbol the Hover card is currently describing (the
    /// pointer is literally on it), and otherwise falls back to the caret - see
    /// [`Self::goto_definition_target`]. No-op when neither exists.
    pub(in crate::code_surface) fn trigger_goto_definition(&mut self, cx: &mut Context<Self>) {
        let Some((path, position)) = self.goto_definition_target() else {
            return;
        };

        let Some(client) = self.lsp_connection_for_path(&path) else {
            return;
        };
        let Ok(uri) = lsp_core::LspClient::uri_for_path(&path) else {
            return;
        };

        let task = cx.spawn(async move |this, cx| {
            let params = lsp_core::lsp_types::GotoDefinitionParams {
                text_document_position_params: lsp_core::lsp_types::TextDocumentPositionParams {
                    text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_core::lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_core::lsp_types::PartialResultParams::default(),
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    client.request::<lsp_core::lsp_types::request::GotoDefinition>(
                        params,
                        LSP_QUERY_TIMEOUT,
                    )
                })
                .await;
            let Ok(Some(response)) = result else {
                return;
            };
            let Some((target_uri, target_range)) = hover_view::first_definition_location(&response)
            else {
                return;
            };
            let Ok(target_path) = lsp_core::LspClient::path_for_uri(&target_uri) else {
                // Non-`file://` target (e.g. a virtual macro-expansion buffer): nothing to
                // navigate to.
                return;
            };
            let target_line = target_range.start.line as usize + 1;
            // `open_file_at_line` needs `Window` access to move focus; `update_in` supplies
            // it by looking up the window this entity belongs to (see
            // vendor/zed/crates/gpui/src/app/async_context.rs `AsyncApp::with_window`), without
            // requiring this task to have been spawned via `cx.spawn_in`.
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_file_at_line(target_path, target_line, window, cx);
            });
        });
        self._goto_definition_tasks.push(task);
    }

    /// Opens `absolute_target_path` in Surface C's File view and lands the caret on
    /// `one_based_line` - the app's one "open that file, at that line" move.
    pub(crate) fn open_file_at_line(
        &mut self,
        absolute_target_path: PathBuf,
        one_based_line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_view(absolute_target_path.clone(), window, cx);
        // Use the same freshness check (path, mtime, len via `code_view::cache_is_fresh`) that
        // `render_file_view` uses, so the two decisions can't disagree.
        let metadata = std::fs::metadata(&absolute_target_path).ok();
        let mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
        let len = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let already_fresh = self.file_view_cache.as_ref().is_some_and(|cached| {
            code_view::cache_is_fresh(cached, &absolute_target_path, mtime, len)
        });
        if already_fresh {
            // Already cached (e.g. navigating within the open file), so render_file_view won't
            // reload and there's no completion handler to consume `pending_cursor_line`.
            self.code_cursor = Some(one_based_line);
            // GitHub issue #202: a visual row, not a line index, and the destination is expanded
            // first if a collapsed region is hiding it - see `AdeApp::scroll_file_view_to_line`.
            self.scroll_file_view_to_line(
                &absolute_target_path,
                one_based_line.saturating_sub(1),
                ScrollStrategy::Center,
            );
        } else {
            self.pending_cursor_line = Some((absolute_target_path, one_based_line));
        }
        cx.notify();
    }

    /// [`GotoDefinition`]'s bound `F12` action handler.
    pub(crate) fn handle_goto_definition_action(
        &mut self,
        _action: &GotoDefinition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.trigger_goto_definition(cx);
    }
}

/// 190px - a soft cap on the real Diagnostic popover's height, serving the exact same purpose
/// [`HOVER_CARD_MAX_HEIGHT`] and `crate::lsp::completion_popup::POPOVER_MAX_HEIGHT` serve for
/// their own popovers (see either constant's own docs): it gives the real "is there room below
/// the offending row" measurement a concrete number to compare real available space against, and
/// stops a genuinely enormous multi-paragraph `rustc` message from painting past the window.
/// Not from the design mockup, whose own diagnostic card (`design_handoff_jerry_ade/revision 3/
/// Jerry.dc.html`) is exactly as tall as its two real lines of text - just a practical ceiling
/// comfortably above a real message plus note plus footer.
const DIAGNOSTIC_CARD_MAX_HEIGHT: gpui::Pixels = px(190.0);

/// 470px - the design mockup's own diagnostic card width (`design_handoff_jerry_ade/revision 3/
/// Jerry.dc.html`: `width:470px` on the card itself).
const DIAGNOSTIC_CARD_WIDTH: gpui::Pixels = px(470.0);

/// How long the Diagnostic card's `copy` button reads `copied` after a real click before flipping
/// back (GitHub issue #204). Long enough to be genuinely readable if the pointer is already
/// leaving the card, short enough that a stale confirmation isn't still sitting there next time
/// the user looks. Not borrowed from any `vendor/zed` constant - Zed's own copy affordances
/// (`vendor/zed/crates/markdown`'s selection copy, the editor's `Copy` action) give no visual
/// confirmation at all, so there was nothing to match.
pub(crate) const DIAGNOSTIC_COPY_CONFIRM_DURATION: Duration = Duration::from_millis(1200);

impl AdeApp {
    /// Surface C's real, caret-anchored Diagnostic popover (GitHub issue #186).
    pub(crate) fn render_diagnostic_card(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let relative_path = self.active_editable_path()?;
        let (last_path, _) = self.file_view_last_layout_for.as_ref()?;
        if last_path != &relative_path {
            // The painted row layout belongs to some other file (e.g. a tab switch whose first
            // frame hasn't painted yet) - no real position to anchor to, so paint nothing rather
            // than guess one.
            return None;
        }
        let (line_number, diagnostic) = self.diagnostic_target()?;
        let (row_bounds, shaped) = self.file_view_row_layout.get(&line_number)?;

        // Flip above the offending row when there isn't real room below it - the same real
        // measurement `Self::render_hover_card` and `render_completions_popover` already make,
        // against the same real `Self::body_bounds`. See [`CardAnchor`] for why flipping pins the
        // card's *bottom* edge rather than computing a top from a worst-case height.
        let (anchor, max_height) = CardAnchor::for_row(
            row_bounds.top(),
            row_bounds.bottom(),
            self.body_bounds.top(),
            self.body_bounds.bottom(),
            window.viewport_size().height,
            DIAGNOSTIC_CARD_MAX_HEIGHT,
        );
        // Anchored under the real, offending span's own start column - the same real
        // `shaped.x_for_index(byte_range.start)` measurement `Self::render_hover_card` already
        // makes off the identical `(Bounds, ShapedLine)` pair, not the row's bare left edge. An
        // earlier version anchored at `row_bounds.left()` alone, which discarded `shaped` entirely
        // and put the card flush under the line-number gutter regardless of where in the line the
        // real error actually was - visibly wrong for anything past a short line, and the reason
        // this was still wrong after two design-fidelity passes that only ever touched the card's
        // own internal chrome, never its position.
        let anchor_x = row_bounds.left() + shaped.x_for_index(diagnostic.byte_range.start);

        // Built here rather than inside the content renderer so the "is *this* card the one whose
        // text is really on the clipboard" comparison can be made against `&self` (see
        // `Self::diagnostic_copy_confirmed`'s own docs for why the comparison is on the payload
        // rather than a bare flag).
        let copy_text = diagnostic_copy_text(diagnostic);
        let copy_confirmed = self.diagnostic_copy_confirmed.as_deref() == Some(copy_text.as_str());

        Some(render_diagnostic_card_content(
            diagnostic,
            anchor_x,
            anchor,
            max_height,
            copy_text,
            copy_confirmed,
            cx,
        ))
    }

    /// GitHub issue #204's real clipboard write: puts one diagnostic's own text on the real system
    /// clipboard (`gpui::App::write_to_clipboard`, the same call
    /// `crate::terminal::pane`'s terminal copy and `crate::sidebar::tree_ops::AdeApp::
    /// copy_path_to_system_clipboard` already make - there is one real clipboard mechanism in this
    /// app and this is it), then arms the momentary `copied` confirmation the button paints.
    pub(in crate::code_surface) fn copy_diagnostic_to_clipboard(
        &mut self,
        text: String,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        self.diagnostic_copy_confirmed = Some(text.clone());
        self._diagnostic_copy_confirm_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(DIAGNOSTIC_COPY_CONFIRM_DURATION)
                .await;
            let _ = this.update(cx, |this, cx| {
                this._diagnostic_copy_confirm_task = None;
                // A newer click may have replaced the payload while this timer ran (a fresh click
                // drops this task, but the two can still race by a frame) - a stale timer must
                // never reach in and cut the *new* confirmation short.
                if this.diagnostic_copy_confirmed.as_deref() == Some(text.as_str()) {
                    this.diagnostic_copy_confirmed = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// Whether the Diagnostic card is conceptually active right now, and if so, which real
    /// `(line_number, diagnostic)` it describes - the trigger-resolution half of
    /// [`Self::render_diagnostic_card`], split out so [`Self::track_hover_pointer`] can ask the
    /// exact same real question (`Self::diagnostic_card_bounds` is a real, painted rectangle, but
    /// it's only trustworthy while a real trigger for it still holds - the same real "is
    /// `Self::hover_card_bounds` even still meaningful" guard `self.hover.is_some()` gives the
    /// Hover card, since a stale rectangle from a card that stopped showing would otherwise
    /// silently swallow every real hover inside it forever, a dead zone with no way out).
    fn diagnostic_target(&self) -> Option<(usize, &diagnostics_view::LineDiagnostic)> {
        if self.completions.is_some() {
            return None;
        }
        match (self.hover.as_ref(), self.hovered_diagnostic()) {
            (Some(hover), Some(diagnostic)) => Some((hover.line_number, diagnostic)),
            (Some(_), None) => None,
            (None, _) => {
                let line_number = self.code_cursor?;
                let diagnostics = self.file_view_diagnostics.get(&line_number)?;
                let worst = diagnostics_view::Severity::worst(diagnostics)?;
                let diagnostic = diagnostics
                    .iter()
                    .find(|candidate| candidate.severity == worst)?;
                Some((line_number, diagnostic))
            }
        }
    }

    /// The real diagnostic [`Self::hover`]'s own hovered span genuinely overlaps on the same
    /// line, if any - the one real condition that flips the usual Hover-over-Diagnostic priority
    /// (see [`Self::render_diagnostic_card`]'s own docs for why), and that method's second real
    /// trigger for the Diagnostic card itself, independent of wherever the caret happens to be.
    /// Shared between that method and [`Self::render_hover_card`] so both sides of the swap agree
    /// on the exact same real overlap, rather than two independently-computed checks that could
    /// disagree at the edges.
    fn hovered_diagnostic(&self) -> Option<&diagnostics_view::LineDiagnostic> {
        let hover = self.hover.as_ref()?;
        let diagnostics = self.file_view_diagnostics.get(&hover.line_number)?;
        diagnostics.iter().find(|diagnostic| {
            hover.byte_range.start < diagnostic.byte_range.end
                && diagnostic.byte_range.start < hover.byte_range.end
        })
    }
}

/// Where a popover card sits relative to the row it describes, and - crucially - **which of its
/// own edges** that position pins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CardAnchor {
    /// Pin the card's top edge this far down the window - the row's own bottom.
    Below(Pixels),
    /// Pin the card's bottom edge this far up from the window's bottom, so the card grows upward
    /// away from the row rather than downward onto it.
    Above(Pixels),
}

impl CardAnchor {
    /// Resolves the flip for a card of at most `max_height`, given the row it describes and the
    /// window it lives in.
    fn for_row(
        row_top: Pixels,
        row_bottom: Pixels,
        body_top: Pixels,
        body_bottom: Pixels,
        window_bottom: Pixels,
        max_height: Pixels,
    ) -> (Self, Pixels) {
        if body_bottom - row_bottom >= max_height {
            return (Self::Below(row_bottom), max_height);
        }
        let available = (row_top - body_top).max(Pixels::ZERO);
        (
            Self::Above(window_bottom - row_top),
            max_height.min(available),
        )
    }

    /// Applies this anchor to a real card element - generic over the element type because
    /// `.id(..)` has already turned these cards into a `Stateful<Div>` by the time it is called.
    fn apply<E: gpui::Styled>(self, card: E) -> E {
        match self {
            Self::Below(top) => card.top(top),
            Self::Above(bottom) => card.bottom(bottom),
        }
    }
}

/// The real Diagnostic popover's own content, split out of [`AdeApp::render_diagnostic_card`] for
/// exactly the reason [`render_hover_card_content`] is split out of [`AdeApp::render_hover_card`]:
/// the positioning math needs `&self` and this doesn't.
fn render_diagnostic_card_content(
    diagnostic: &diagnostics_view::LineDiagnostic,
    anchor_x: Pixels,
    anchor: CardAnchor,
    max_height: Pixels,
    copy_text: String,
    copy_confirmed: bool,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    let source_code = diagnostic_source_code(diagnostic);
    // `rustc`/`rust-analyzer` messages are routinely multi-line: the first line is the headline
    // the design calls "message", everything after it is the design's dimmer "note".
    let mut lines = diagnostic.message.lines();
    let headline = lines.next().unwrap_or_default().to_string();
    let note = lines.collect::<Vec<_>>().join("\n");

    let mut body = div()
        .flex()
        .flex_1()
        .min_w_0()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .font(font(theme::font::MONO))
                .text_size(px(11.5))
                .text_color(diagnostic_card_message_color(diagnostic.severity))
                .child(headline),
        );
    if !note.trim().is_empty() {
        body = body.child(
            div()
                .font(font(theme::font::MONO))
                .text_size(px(11.0))
                .text_color(theme::text::DIMMER)
                .child(note),
        );
    }

    // Captures this card's own real painted bounds into `AdeApp::diagnostic_card_bounds` every
    // frame, mirroring `render_hover_card_content`'s own identical `bounds_probe` idiom -
    // `AdeApp::track_hover_pointer` reads it to keep the card alive while the pointer rests on it
    // even when it happens to be covering a real, different hoverable token underneath.
    let bounds_probe = {
        let entity = cx.entity();
        gpui::canvas(
            move |bounds, _window, cx| {
                entity.update(cx, |this, _cx| {
                    this.diagnostic_card_bounds = Some(bounds);
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    };

    let mut card = anchor
        .apply(
            div()
                .id("diagnostic-card")
                // Lets a real test measure this real popover's own painted bounds (`debug_bounds` reads
                // this, not `.id(..)`) - a no-op outside test builds, matching `"hover-card"` and every
                // other `debug_selector` in this crate.
                .debug_selector(|| "diagnostic-card".to_string())
                .child(bounds_probe)
                .absolute()
                .left(anchor_x)
                .flex_none()
                .flex()
                .flex_col()
                .w(DIAGNOSTIC_CARD_WIDTH)
                .max_h(max_height),
        )
        .overflow_hidden()
        .rounded(theme::radius::CARD_SM)
        .bg(theme::syntax::DIAGNOSTIC_ROW_BG)
        .border_1()
        .border_color(theme::border::DIAGNOSTIC_CARD)
        // See `render_hover_card_content`'s own identical `.occlude()` docs for why - a real
        // scroll/click over this card must never also reach the editor content behind it.
        .occlude()
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(8.0))
                .child(
                    // The mockup's own 5px severity dot, `margin-top:5px` so it optically centres
                    // on the message's first line rather than on the whole block.
                    div()
                        .flex_none()
                        .mt(px(5.0))
                        .size(px(5.0))
                        .rounded_full()
                        .bg(diagnostic_underline_color(diagnostic.severity)),
                )
                .child(body),
        );

    // `background:#141719;border-top:1px solid #2b2224` in the mockup - its own footer band,
    // the same `LSP_POPOVER_FOOTER` background the Hover card's footer uses, but with this
    // card's own darker `DIAGNOSTIC_CARD_FOOTER` border rather than the neutral `border::
    // INNER` the pre-review version used (which also painted no background band at all).
    //
    // Unconditional since GitHub issue #204, where it used to be drawn only for a diagnostic that
    // really had a `source`/`code`: the band now also carries the `copy` button, and a diagnostic
    // with no source and no code (real servers do send those) is exactly as worth copying as one
    // that has both. The `source · code` text itself is still conditional, so such a card gets a
    // band holding only the button rather than a band with an empty label in it.
    let mut footer_label = div().flex_1().min_w_0();
    if !source_code.is_empty() {
        footer_label = footer_label
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::FAINTER)
            .child(source_code);
    }
    card = card.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .py(px(6.0))
            .bg(theme::surface::LSP_POPOVER_FOOTER)
            .border_t_1()
            .border_color(theme::border::DIAGNOSTIC_CARD_FOOTER)
            .child(footer_label)
            .child(render_diagnostic_copy_button(copy_text, copy_confirmed, cx)),
    );

    card.into_any_element()
}

/// The `source · code` line the Diagnostic card's footer shows, e.g. `rust-analyzer · E0277` or
/// `eslint · no-unused-vars`. Empty when the server sent neither, which is a real case.
fn diagnostic_source_code(diagnostic: &diagnostics_view::LineDiagnostic) -> String {
    match (&diagnostic.source, &diagnostic.code) {
        (Some(source), Some(code)) => format!("{source} · {code}"),
        (Some(source), None) => source.clone(),
        (None, Some(code)) => code.clone(),
        (None, None) => String::new(),
    }
}

/// Exactly the text one Diagnostic card paints, as one clipboard payload (GitHub issue #204): the
/// server's full multi-line `message` - both the headline the card shows in the severity colour
/// *and* the dimmer "note" lines under it, which are one `message` split for display only - then
/// the footer's own `source · code` line when there is one.
fn diagnostic_copy_text(diagnostic: &diagnostics_view::LineDiagnostic) -> String {
    let source_code = diagnostic_source_code(diagnostic);
    if source_code.is_empty() {
        diagnostic.message.clone()
    } else {
        format!("{}\n{source_code}", diagnostic.message)
    }
}

/// The Diagnostic card's own real copy button (GitHub issue #204), and the only affordance on this
/// card that does anything at all.
fn render_diagnostic_copy_button(
    copy_text: String,
    copy_confirmed: bool,
    cx: &mut Context<AdeApp>,
) -> impl IntoElement {
    div()
        .id("diagnostic-card-copy")
        // Lets a real test measure this button's own painted box - both to click it and to prove
        // the `copied` state genuinely repaints (it is visibly wider than `copy`).
        .debug_selector(|| "diagnostic-card-copy".to_string())
        .flex_none()
        .h(px(16.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(theme::radius::BUTTON)
        .border_1()
        .border_color(theme::border::BUTTON)
        .cursor_pointer()
        .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
        .font(font(theme::font::MONO))
        .text_size(px(10.0))
        .text_color(if copy_confirmed {
            theme::text::SECONDARY
        } else {
            theme::text::MUTED
        })
        .child(if copy_confirmed { "copied" } else { "copy" })
        // The card is `.occlude()`d, so this never reaches the editor behind it, but the footer
        // band under the button is still a real parent - `stop_propagation` matches every other
        // button in this app painted on top of a clickable ancestor.
        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            this.copy_diagnostic_to_clipboard(copy_text.clone(), cx);
        }))
}

/// The Diagnostic popover's headline colour for one severity - `Error` keeps the design's own
/// [`theme::syntax::DIAGNOSTIC_CARD_MESSAGE`]; every other severity reuses
/// [`theme::text::SECONDARY`], for the same reason [`diagnostic_inline_message_color`] and
/// [`diagnostic_row_bg`] already de-escalate below `Error`: a warning or a hint must not read as
/// alarming as a real error.
pub(in crate::code_surface) fn diagnostic_card_message_color(
    severity: diagnostics_view::Severity,
) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::DIAGNOSTIC_CARD_MESSAGE.into(),
        _ => theme::text::SECONDARY.into(),
    }
}

/// 220px - a soft cap on the real Hover popover's height, purely so
/// [`AdeApp::render_hover_card`]'s own real "is there room below the hovered row" measurement
/// (mirroring [`AdeApp::render_completions_popover`]'s identical `POPOVER_MAX_HEIGHT` judgment -
/// see that constant's own docs) has a concrete number to compare real available space against,
/// and so a real, unusually long doc string can't paint past the window. Not derived from the
/// design mockup (`design_handoff_jerry_ade/revision/Jerry.dc.html`'s own hover card has no
/// fixed height - it's exactly as tall as its own real content), just a practical, generous
/// ceiling comfortably above what a real signature + doc + footer normally needs.
const HOVER_CARD_MAX_HEIGHT: gpui::Pixels = px(220.0);
/// 430px - the design mockup's own real hover card width
/// (`design_handoff_jerry_ade/revision 3/Jerry.dc.html`: `width:430px` on the card).
const HOVER_CARD_MAX_WIDTH: gpui::Pixels = px(430.0);
/// 10px - the header/body/footer bands' own real shared horizontal padding
/// (`Jerry.dc.html`: `padding:7px 10px 6px`/`padding:7px 10px`/`padding:6px 10px` - all three
/// bands agree on `10px` left/right). Named so [`render_hover_signature`]'s own real max-width
/// (card width minus both sides' padding) can be computed from the same real numbers the bands
/// themselves paint with, rather than a second, independently-guessed constant that could drift.
const HOVER_CARD_HORIZONTAL_PADDING: gpui::Pixels = px(10.0);

impl AdeApp {
    /// Surface C's real, token-anchored Hover popover (signature, doc prose, module path, `F12
    /// definition` footer) - mirrors [`Self::render_completions_popover`]'s own real positioning
    /// mechanism exactly, matching that method's own top-doc'd reasoning for why: both anchor off
    /// a real, already-painted `(Bounds, ShapedLine)` pair and both paint as a real, absolutely-
    /// positioned top-level sibling in [`Render::render`] (`crate::root::AdeApp::render`), never
    /// nested inside the File view's own virtualized `uniform_list` - a popup anchored to one row
    /// must not be clipped by that row's own scroll container.
    pub(crate) fn render_hover_card(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let hover = self.hover.as_ref()?;
        let active_relative = self.active_editable_path()?;
        if hover.path != self.file_tree_root.join(&active_relative) {
            return None;
        }
        if self.hovered_diagnostic().is_some() {
            return None;
        }
        // A real, answered hover with genuinely nothing to say paints nothing at all rather than
        // an empty "no symbol information here" card - that card told the user their pointer
        // landed somewhere real (a token) but useless, which in practice is indistinguishable
        // from having landed on plain whitespace between tokens; either way there's nothing to
        // show, so neither should show a popup.
        if matches!(hover.status, HoverStatus::Ready(None)) {
            return None;
        }
        let (row_bounds, shaped) = self.file_view_row_layout.get(&hover.line_number)?;

        let anchor_x = row_bounds.left() + shaped.x_for_index(hover.byte_range.start);
        let row_top = row_bounds.top();
        let row_bottom = row_bounds.bottom();

        // Flip above the hovered row when there isn't real room below it in the window body -
        // the same real "measure real available space, flip if it doesn't fit" judgment
        // `Self::render_completions_popover` already makes (see that method's own docs for the
        // `vendor/zed` precedent this follows), and the same [`CardAnchor`] the Diagnostic card
        // uses so a flipped card sits *against* its row rather than a worst-case height above it.
        let (anchor, max_height) = CardAnchor::for_row(
            row_top,
            row_bottom,
            self.body_bounds.top(),
            self.body_bounds.bottom(),
            window.viewport_size().height,
            HOVER_CARD_MAX_HEIGHT,
        );

        // The active file's own extension - the same one `Self::request_hover` resolved a
        // highlighter for when the code line itself was painted - so the signature reads with
        // the exact same grammar/colors as the code around it, not a guessed or absent one.
        let extension = active_relative
            .extension()
            .and_then(|extension| extension.to_str());

        Some(self.render_hover_card_content(hover, extension, anchor_x, anchor, max_height, cx))
    }

    /// The real Hover popover's own content - split out of [`Self::render_hover_card`] purely so
    /// the real positioning math there (an early-return chain resolving *whether*/*where* to
    /// anchor) stays visually separate from the real per-status content build here - mirrors
    /// [`Self::render_completions_popover`]'s own inline match, just factored into its own method
    /// since that one has a real early-return position/anchor computation ahead of it. Still a
    /// method, not a free function, since a genuinely tall `Ready(Some(_))` card needs
    /// [`Self::hover_card_scroll_handle`] and [`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`],
    /// both of which need `&self`.
    fn render_hover_card_content(
        &self,
        hover: &HoverEntry,
        extension: Option<&str>,
        anchor_x: Pixels,
        anchor: CardAnchor,
        max_height: Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // GitHub issue #186: captures this card's own real painted bounds into
        // `AdeApp::hover_card_bounds` every frame, the same `gpui::canvas` idiom
        // `crate::root::AdeApp::render_workspace_body` already uses for `AdeApp::body_bounds`.
        // `AdeApp::track_hover_pointer` reads it to keep the card alive while the pointer is on it.
        let bounds_probe = {
            let entity = cx.entity();
            gpui::canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |this, _cx| {
                        this.hover_card_bounds = Some(bounds);
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        // No `.p()`/`.gap()` here (unlike the pre-review version) - the design's own hover card has
        // no uniform padding at all: it is three independently-padded/bordered bands (signature
        // header, doc body, `module::path` + `F12 definition` footer), not a single padded flex
        // column. Only `HoverStatus::Ready(Some(_))` below builds those three bands; every other
        // status is a single line of plain text with no real card structure to speak of, so it gets
        // its own one-off `.p(px(10.0))` wrapper instead.
        let mut card = anchor
            .apply(
                div()
                    .id("hover-card")
                    // Lets a real test measure this real popover's own painted bounds (`debug_bounds` reads
                    // this, not `.id(..)` - see `hover_popover_position_tests`) - a no-op outside test
                    // builds, matching every other `debug_selector` in this crate.
                    .debug_selector(|| "hover-card".to_string())
                    .child(bounds_probe)
                    .absolute()
                    .left(anchor_x)
                    .flex_none()
                    .flex()
                    .flex_col()
                    .max_w(HOVER_CARD_MAX_WIDTH)
                    .max_h(max_height),
            )
            .overflow_hidden()
            .rounded(theme::radius::CARD_SM)
            .bg(theme::surface::POPOVER)
            .border_1()
            .border_color(theme::border::POPOVER)
            // Without this, a scroll over the card's own `overflow_y_scroll()` doc region also
            // reached the File view's own scrollable content behind it - `gpui`'s internal scroll
            // listener (`vendor/zed/crates/gpui/src/elements/div.rs`'s `paint_scroll_listener`)
            // never calls `cx.stop_propagation()` on its own, it only ever updates its own offset,
            // so *every* hitbox under the pointer that also registered a scroll listener handles
            // the same wheel event unless one of them is occluded. `.occlude()`
            // (`gpui::InteractiveElement::occlude`, `HitboxBehavior::BlockMouse`) is this app's own
            // established fix for exactly this class of bug (`crate::sidebar::render::
            // render_tree_context_menu`'s own scrim docs) - `Hitbox::should_handle_scroll`'s own
            // docs confirm it's scroll-aware, not just click-aware: "if a hitbox in front of this
            // sets HitboxBehavior::BlockMouse ... concretely, this is due to use-cases like
            // overlays".
            .occlude();

        match &hover.status {
            HoverStatus::Loading => {
                card = card.child(
                    div()
                        .p(px(10.0))
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::text::FAINT)
                        .child("loading hover..."),
                );
            }
            HoverStatus::Failed(message) => {
                card = card.child(
                    div()
                        .p(px(10.0))
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::status::FAIL)
                        .child(format!("hover failed: {message}")),
                );
            }
            // Unreachable in practice - `Self::render_hover_card` returns `None` itself for
            // `Ready(None)` rather than calling this function, so a genuinely empty hover shows no
            // popup at all.
            HoverStatus::Ready(None) => {}
            HoverStatus::Ready(Some(model)) => {
                // Header: the signature, `padding:7px 10px 6px;border-bottom:1px solid #23282c` in
                // the mockup - `theme::border::CARD` is that exact hex, already registered for
                // exactly this kind of internal card seam elsewhere in the app.
                let mut scroll_body = div()
                    .id("hover-card-scroll-body")
                    .flex()
                    .flex_col()
                    // `.flex_1().min_h_0()` directly on the scrolling element itself, not just on
                    // its `.relative()` wrapper below - a flex item's default `min-height: auto`
                    // otherwise refuses to shrink below its own content's natural size, which
                    // silently defeated `overflow_y_scroll()` here: the element measured its real,
                    // painted box at the wrapper's bound (matching `crate::rail::render`'s own
                    // identical `"agent-rail-list"` scrollable list, the precedent this mirrors).
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    // GitHub issue #30's real overlay scrollbar (below) reads its geometry straight
                    // off this same handle.
                    .track_scroll(&self.hover_card_scroll_handle)
                    .child(
                        div()
                            // `.max_w(HOVER_CARD_MAX_WIDTH)` is load-bearing, not decoration. A GPUI
                            // element with no explicit width sizes itself to its own content (shrink-
                            // to-fit) rather than stretching to fill its parent the way a CSS block
                            // element would - the same real bug class `crate::code_surface::editing`'s
                            // own `text_row` docs describe for an identical shrink-to-fit failure - so
                            // a plain `.w_full()` here turned out *not* to be enough on its own:
                            // percentage width resolves against a parent's own *resolved* width, and
                            // the card above is itself only `max_w`-bounded (auto/shrink-to-fit
                            // otherwise), so `100%` of an unresolved auto width is still effectively
                            // unbounded. A real, hard `max_w` gives this header (and
                            // `render_hover_signature`'s own row below it) a genuinely definite upper
                            // bound to wrap `flex_wrap()`'s content within, regardless of what the
                            // card's own width resolves to. Without it, a signature longer than 430px
                            // never gets a real width to wrap within, so `render_hover_signature`'s
                            // own `flex_wrap()` never actually reflows - the row just grows past 430px
                            // and the card's `overflow_hidden()` silently hard-clips it instead (a
                            // real, live-reproduced TypeScript symptom: a long union/generic type
                            // painted cut off mid-glyph).
                            .max_w(HOVER_CARD_MAX_WIDTH)
                            .pt(px(7.0))
                            .px(px(10.0))
                            .pb(px(6.0))
                            .border_b_1()
                            .border_color(theme::border::CARD)
                            .child(render_hover_signature(&model.signature, extension)),
                    );
                if let Some(doc) = &model.doc {
                    // Body: `padding:7px 10px;font:...'IBM Plex Sans';color:#8b9197` - `theme::
                    // text::DIM` is that exact hex (distinct from `DIMMER`/`FAINT`, which are darker
                    // and belong to other elements in this same card, not this one).
                    scroll_body = scroll_body.child(
                        div()
                            .px(px(10.0))
                            .py(px(7.0))
                            .text_size(px(11.5))
                            .child(render_doc_sections(doc, theme::text::DIM, extension)),
                    );
                }
                // `.flex_1().min_h_0()`: absorbs whatever height the footer below doesn't need, up to
                // the card's own real `max_h` - a genuinely multi-line signature (a pretty-printed
                // TypeScript utility/generic type) or a long doc comment can now overflow *this*
                // region and scroll, rather than the footer being pushed below the card's own visible
                // clip and simply disappearing (the real, reported bug this fix addresses). `.relative()`
                // is the positioning root the overlay scrollbar below anchors `.absolute()` against -
                // it must NOT be the same element as `scroll_body` itself, or the scrollbar would
                // scroll away with the content it's supposed to stay fixed against.
                card = card.child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(scroll_body)
                        .children(scrollbar::render_vertical_scrollbar(
                            "hover-card-scrollbar",
                            &self.hover_card_scroll_handle,
                            &[],
                            cx,
                        )),
                );
                // Footer: its own `background:#141719;border-top:1px solid #23282c` band, not a
                // transparent strip inside the card's own padding - `theme::surface::
                // LSP_POPOVER_FOOTER` is that exact background hex. `module::path` sits at the far
                // left and `F12 definition` at the far right (`gap:10px`, a `flex:1` spacer between
                // them in the mockup) - the pre-review version bunched both together with a plain
                // `gap`, which is the layout difference a purely color-focused pass missed.
                let mut footer = div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .bg(theme::surface::LSP_POPOVER_FOOTER)
                    .border_t_1()
                    .border_color(theme::border::CARD);
                if let Some(module_path) = &model.module_path {
                    // `color:#5e646a` in the mockup - `theme::text::FAINTER`, not `FAINT`
                    // (`#6b7178`), which the pre-review version used.
                    footer = footer.child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(module_path.clone()),
                    );
                }
                footer = footer.child(div().flex_1());
                footer = footer.child(
                    div()
                        .id("hover-card-goto-definition")
                        // Lets a real test measure this real chip's own painted bounds
                        // (`hover_card_footer_layout_tests`) - a no-op outside test builds, matching
                        // every other `debug_selector` in this crate.
                        .debug_selector(|| "hover-card-goto-definition".to_string())
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .cursor_pointer()
                        // `F12` is a function key, not one of `crate::keymap`'s modifier tokens, and
                        // is identical on both platforms, so it bypasses `keymap::resolve_combo`.
                        .child(render_keycap("F12"))
                        .child(
                            div()
                                // `color:#4a5057` in the mockup - `theme::text::PATH` is that exact
                                // hex (an existing token named for its other use elsewhere; the value
                                // is what matters here, not the name).
                                .text_size(px(10.0))
                                .text_color(theme::text::PATH)
                                .child("definition"),
                        )
                        .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.trigger_goto_definition(cx);
                        })),
                );
                card = card.child(footer);
            }
        }

        card.into_any_element()
    }
}

/// The Hover popover's own signature line, syntax-highlighted like real code rather than painted
/// as flat text (`design_handoff_jerry_ade/revision 3/Jerry.dc.html`'s own hover card shows `pub
/// trait Into<T>: Sized` with real per-token colors - keyword purple, type gold - not one flat
/// heading color).
fn render_hover_signature(signature: &str, extension: Option<&str>) -> gpui::AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .font(font(theme::font::MONO))
        .text_size(px(11.5));
    let mut run_index = 0usize;
    for line_runs in highlighted_signature_lines(signature, extension) {
        let mut row = div()
            .flex()
            .flex_wrap()
            // See the header wrapper's own `.max_w(HOVER_CARD_MAX_WIDTH)` doc comment in
            // `render_hover_card_content` for why this needs a real, hard max-width (not just
            // `.w_full()`) to make `flex_wrap()` actually reflow. This row sits inside that
            // header's own `HOVER_CARD_HORIZONTAL_PADDING` on both sides, so its own bound is
            // narrower by twice that.
            .max_w(
                HOVER_CARD_MAX_WIDTH
                    - HOVER_CARD_HORIZONTAL_PADDING
                    - HOVER_CARD_HORIZONTAL_PADDING,
            );
        for (run_text, kind) in line_runs {
            // A running index across every real line, not reset per line - a single-line
            // signature (still the overwhelming common case) numbers identically to before this
            // fix, so an existing `hover-signature-token-N` selector in a test keeps meaning the
            // same real token it always did.
            let index = run_index;
            run_index += 1;
            row = row.child(
                div()
                    .id(("hover-signature-token", index))
                    // Lets a real test measure this real token's own painted bounds
                    // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test builds,
                    // matching every other `debug_selector` in this crate.
                    .debug_selector(move || format!("hover-signature-token-{index}"))
                    .text_color(code_view::color_for_kind(kind))
                    .child(run_text),
            );
        }
        column = column.child(row);
    }
    column.into_any_element()
}

/// The pure half of [`render_hover_signature`] - just the `signature` -> colored-run computation,
/// split out so it's directly `#[test]`-able without a `gpui::Window`/`TestAppContext` (mirroring
/// how `code_view::highlight_block` itself is tested at the pure level, not by painting). One
/// inner `Vec` per real source line in `signature` - see [`render_hover_signature`]'s own docs for
/// why a multi-line signature can't be flattened into one.
fn highlighted_signature_lines(
    signature: &str,
    extension: Option<&str>,
) -> Vec<Vec<(gpui::SharedString, code_view::HighlightKind)>> {
    code_view::highlight_block(
        std::iter::once(signature),
        extension,
        code_view::HighlightOptions::default(),
    )
    .into_iter()
    .map(|line| line.runs)
    .collect()
}

/// One run of [`render_doc_prose`]' output that is not ordinary prose.
enum DocSpan {
    /// A JSDoc tag (`@param`, `{@link ...}`) - painted in its own accent colour.
    Tag(std::ops::Range<usize>),
    /// A real Markdown inline link - painted as just its visible text, underlined, and genuinely
    /// clickable (GitHub issue #201).
    Link(markdown_preview::InlineLinkSpan),
}

impl DocSpan {
    fn range(&self) -> std::ops::Range<usize> {
        match self {
            DocSpan::Tag(range) => range.clone(),
            DocSpan::Link(span) => span.markup.clone(),
        }
    }
}

/// GitHub issue #201's hover/completion half: every real Markdown inline link in `doc`, plus every
/// real JSDoc tag range that doesn't sit inside one, sorted into a single ordered span list.
fn doc_spans(doc: &str) -> Vec<DocSpan> {
    let links = markdown_preview::inline_link_spans(doc);
    let mut spans: Vec<DocSpan> = code_view::doc_tag_ranges(doc)
        .into_iter()
        .filter(|tag| {
            !links
                .iter()
                .any(|link| tag.start < link.markup.end && link.markup.start < tag.end)
        })
        .map(DocSpan::Tag)
        .collect();
    spans.extend(links.into_iter().map(DocSpan::Link));
    spans.sort_by_key(|span| span.range().start);
    spans
}

/// A doc paragraph's own plain-text body (`HoverRenderModel::doc`/`completion_documentation_text`,
/// see either's own docs for why this is plain text, not real Markdown, in the first place), with
/// every real `code_view::doc_tag_ranges` span (`@param`, `{@link ...}`, ...) painted in
/// `HighlightKind::CommentDocTag`'s own accent colour and a heavier weight - GitHub issue #200's
/// rendered-side half - and every real Markdown inline link painted as a real, clickable link
/// (GitHub issue #201, see [`render_doc_link`]). `base_color` is every ordinary-prose run's own
/// colour, so the Hover card (`theme::text::DIM`) and the Completions detail pane
/// (`theme::text::DIMMER`) each keep their own already-designed doc-paragraph colour.
pub(crate) fn render_doc_prose(
    doc: &str,
    base_color: impl Into<gpui::Hsla> + Copy,
    next_id: &mut usize,
) -> gpui::AnyElement {
    let spans = doc_spans(doc);
    if spans.is_empty() {
        return div()
            .text_color(base_color)
            .child(doc.to_string())
            .into_any_element();
    }
    let mut wrapper = div().flex().flex_wrap();
    let mut cursor = 0;
    for span in spans {
        let range = span.range();
        if range.start > cursor {
            wrapper = wrapper.child(
                div()
                    .text_color(base_color)
                    .child(doc[cursor..range.start].to_string()),
            );
        }
        wrapper = wrapper.child(match span {
            DocSpan::Tag(_) => {
                let index = take_doc_span_id(next_id);
                div()
                    .id(("doc-prose-tag", index))
                    // Lets a real test measure this real tag run's own painted bounds
                    // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test builds,
                    // matching every other `debug_selector` in this crate.
                    .debug_selector(move || format!("doc-prose-tag-{index}"))
                    .text_color(code_view::color_for_kind(
                        code_view::HighlightKind::CommentDocTag,
                    ))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(doc[range.clone()].to_string())
                    .into_any_element()
            }
            DocSpan::Link(link) => render_doc_link(&link, base_color, next_id),
        });
        cursor = range.end;
    }
    if cursor < doc.len() {
        wrapper = wrapper.child(
            div()
                .text_color(base_color)
                .child(doc[cursor..].to_string()),
        );
    }
    wrapper.into_any_element()
}

/// Hands out the next card-unique doc-span id - see [`render_doc_link`]'s own docs for why these
/// have to be unique across the *whole* card rather than per `render_doc_prose` call.
fn take_doc_span_id(next_id: &mut usize) -> usize {
    let id = *next_id;
    *next_id += 1;
    id
}

/// One real Markdown link inside an LSP doc body (GitHub issue #201: "In markdown preview but also
/// other places where we render it"). Before this, a link in a real docstring - `[MDN](https://
/// developer.mozilla.org/...)`, which is exactly how TypeScript's own lib docs and countless
/// rustdoc comments cite references - reached the hover card as *literal* `[...](...)` bracket and
/// paren syntax: `crate::lsp::hover::degrade_markdown_to_plain_text` strips `**`, backticks,
/// headings and bullets, but has never touched link markup. It now renders as just its visible
/// text, underlined in the link colour, and really opens.
fn render_doc_link(
    link: &markdown_preview::InlineLinkSpan,
    base_color: impl Into<gpui::Hsla> + Copy,
    next_id: &mut usize,
) -> gpui::AnyElement {
    let index = take_doc_span_id(next_id);
    // A destination this app cannot honestly open (a relative path, a bare fragment - see
    // `markdown_preview::openable_url`) still sheds its literal `[...](...)` syntax, which was the
    // visible half of the bug, but is painted as ordinary prose rather than advertised as a link.
    let Some(url) = markdown_preview::openable_url(&link.destination) else {
        return div()
            .text_color(base_color)
            .child(link.text.clone())
            .into_any_element();
    };
    let url = url.to_string();
    div()
        .id(("doc-prose-link", index))
        // Lets a real test measure this real link run's own painted bounds and click it
        // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test builds.
        .debug_selector(move || format!("doc-prose-link-{index}"))
        .cursor_pointer()
        .text_color(theme::syntax::LINK)
        .border_b_1()
        .border_color(theme::syntax::LINK)
        .child(link.text.clone())
        .on_click(move |_event, _window, cx| {
            // The same real `gpui::App::open_url` the Markdown preview and
            // `crate::title_bar::menu` open URLs with - one mechanism, not a third copy.
            cx.open_url(&url);
        })
        .into_any_element()
}

/// A doc body's own real, structured JSDoc rendering (GitHub issue #200: "params/returns/example
/// ... displayed like code in their own section") - [`hover_view::parse_doc_sections`]'s own real
/// description/params/returns/examples/other split, each painted as its own real, visually
/// distinct block, replacing what used to be one flat, undifferentiated paragraph. Shared between
/// the Hover card and the Completions detail pane, the same two real callers [`render_doc_prose`]
/// already had.
pub(crate) fn render_doc_sections(
    doc: &str,
    base_color: impl Into<gpui::Hsla> + Copy,
    extension: Option<&str>,
) -> gpui::AnyElement {
    let sections = hover_view::parse_doc_sections(doc);
    let mut column = div().flex().flex_col().gap(px(10.0));
    // One counter for the whole card - see `render_doc_link`'s own docs for why every interactive
    // doc span across every section below has to draw its id from the same sequence.
    let next_id = &mut 0usize;

    if let Some(description) = &sections.description {
        column = column.child(render_doc_prose(description, base_color, next_id));
    }
    if !sections.params.is_empty() {
        column = column.child(render_doc_params_section(
            &sections.params,
            base_color,
            next_id,
        ));
    }
    if let Some(returns) = &sections.returns {
        let body = render_doc_prose(returns, base_color, next_id);
        column = column.child(render_doc_labeled_section("Returns", body));
    }
    for example in &sections.examples {
        column = column.child(render_doc_example_section(example, extension));
    }
    for (tag, body) in &sections.other {
        let body = render_doc_prose(body, base_color, next_id);
        column = column.child(render_doc_labeled_section(
            &doc_tag_section_label(tag),
            body,
        ));
    }

    column.into_any_element()
}

/// One doc-section header - `font:600 9.5px 'IBM Plex Sans', uppercase` - the exact real small-
/// caps label idiom this app already uses for a group header (`crate::palette::render`'s own
/// command-palette group headers, `crate::rail::render`'s own repo-name headers), reused here
/// rather than inventing a second one, plus `body` below it.
fn render_doc_labeled_section(label: &str, body: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_size(px(9.5))
                .text_color(theme::text::FAINT)
                .child(label.to_uppercase()),
        )
        .child(body)
        .into_any_element()
}

/// `@throws`/`@deprecated`/`@see`/... - any tag [`render_doc_sections`] has no dedicated section
/// for - as a real, readable section label: `"see"` -> `"See"`. ASCII-only (every real JSDoc tag
/// word is a bare ASCII identifier), so a byte-index capitalize is safe here without a
/// grapheme-aware pass.
fn doc_tag_section_label(tag: &str) -> String {
    let mut chars = tag.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// The `@param` section - one row per real `(name, description)` pair, the name in mono/
/// `HighlightKind::VariableParameter`'s own colour (matching how a real parameter reads inside a
/// highlighted signature line elsewhere in this same card) immediately followed by its prose
/// description, rather than either buried inline in a flat paragraph or missing the visual tie to
/// "this is a parameter name" a plain-text render gave it before this fix.
fn render_doc_params_section(
    params: &[(String, String)],
    base_color: impl Into<gpui::Hsla> + Copy,
    next_id: &mut usize,
) -> gpui::AnyElement {
    let label = if params.len() > 1 {
        "Parameters"
    } else {
        "Parameter"
    };
    let mut rows = div().flex().flex_col().gap(px(3.0));
    for (index, (name, description)) in params.iter().enumerate() {
        let mut row = div()
            .id(("doc-param-row", index))
            // Lets a real test measure this real row's own painted bounds (`debug_bounds` reads
            // this, not `.id(..)`) - a no-op outside test builds, matching every other
            // `debug_selector` in this crate.
            .debug_selector(move || format!("doc-param-row-{index}"))
            .flex()
            .flex_wrap()
            .gap(px(5.0))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(code_view::color_for_kind(
                        code_view::HighlightKind::VariableParameter,
                    ))
                    .child(name.clone()),
            );
        if !description.is_empty() {
            row = row.child(render_doc_prose(description, base_color, next_id));
        }
        rows = rows.child(row);
    }
    render_doc_labeled_section(label, rows.into_any_element())
}

/// An `@example` body - a real, syntax-highlighted (when `extension` is known) code block, mono
/// font over a tinted background, visually distinct from surrounding prose the same way a fenced
/// code block reads in any other real doc-rendering tool - not flat, undifferentiated paragraph
/// text the way this app rendered doc bodies before this fix. `code_view::highlight_block` is the
/// exact same real highlighter [`render_hover_signature`] already uses for the signature line
/// above it, so an example genuinely gets the same real per-token colours the rest of the popup
/// does, not a second, separately-maintained rendering path.
fn render_doc_example_section(example: &str, extension: Option<&str>) -> gpui::AnyElement {
    let lines = code_view::highlight_block(
        example.lines(),
        extension,
        code_view::HighlightOptions::default(),
    );
    let mut code = div().flex().flex_col();
    for line in lines {
        let mut row = div().flex().flex_wrap();
        for (run_text, kind) in line.runs {
            row = row.child(
                div()
                    .text_color(code_view::color_for_kind(kind))
                    .child(run_text),
            );
        }
        code = code.child(row);
    }
    render_doc_labeled_section(
        "Example",
        div()
            .id("doc-example-block")
            // Lets a real test measure this real code block's own painted bounds
            // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test builds, matching
            // every other `debug_selector` in this crate.
            .debug_selector(|| "doc-example-block".to_string())
            .rounded(theme::radius::CARD_SM)
            .bg(theme::surface::CURRENT_LINE)
            .px(px(8.0))
            .py(px(6.0))
            .font(font(theme::font::MONO))
            .text_size(px(10.5))
            .child(code)
            .into_any_element(),
    )
}

/// One File view code row: a 52px right-aligned line-number gutter, a 3px git-gutter marker
/// (tinted `theme::diff::GIT_GUTTER` for `is_changed`, transparent otherwise), and the
/// syntax-highlighted line content (`line.runs`, via `code_view::color_for_kind`). `is_current`
/// tints the whole row and brightens the gutter number.
pub(in crate::code_surface) fn diagnostic_underline_color(
    severity: diagnostics_view::Severity,
) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::ERROR_UNDERLINE.into(),
        diagnostics_view::Severity::Warning => theme::term::WARN.into(),
        diagnostics_view::Severity::Information => theme::text::DIM.into(),
        diagnostics_view::Severity::Hint => theme::text::FAINT.into(),
    }
}

/// The File view row's background tint for a diagnostic of `severity` - `None` means no tint.
/// Only `Error` gets one ([`theme::syntax::DIAGNOSTIC_ROW_BG`]); the other three are
/// distinguished from a clean line by their dotted underline alone, keeping every non-error
/// severity visibly less alarming than an error.
pub(in crate::code_surface) fn diagnostic_row_bg(
    severity: diagnostics_view::Severity,
) -> Option<gpui::Rgba> {
    match severity {
        diagnostics_view::Severity::Error => Some(theme::syntax::DIAGNOSTIC_ROW_BG.into()),
        _ => None,
    }
}

/// The File view row's inline end-of-line message color for a diagnostic of `severity` - `Error`
/// keeps [`theme::syntax::DIAGNOSTIC_INLINE_MESSAGE`]; every other severity reuses
/// [`theme::text::FAINT`].
pub(in crate::code_surface) fn diagnostic_inline_message_color(
    severity: diagnostics_view::Severity,
) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::DIAGNOSTIC_INLINE_MESSAGE.into(),
        _ => theme::text::FAINT.into(),
    }
}

/// The File view row's real, dim end-of-line diagnostic message (`design_handoff_jerry_ade/
/// revision 3/README.md`'s Diagnostic state: "dim inline message at end of line") - shared by both
/// row renderers, the read-only [`crate::code_surface::file_view::render_file_view_line`] and the
/// live-buffer `crate::code_surface::editing::render_editable_file_view_line`, so the two can't
/// drift apart on this.
pub(in crate::code_surface) fn render_inline_diagnostic_message(
    first_line: &str,
    severity: diagnostics_view::Severity,
    line_number: usize,
) -> gpui::AnyElement {
    div()
        .id(("file-view-diagnostic-message", line_number))
        // Lets a real test measure this element's own real painted width against a real narrow
        // pane (see `inline_diagnostic_message_tests`) - a no-op outside test builds.
        .debug_selector(move || format!("file-view-diagnostic-message-{line_number}"))
        .min_w_0()
        .max_w(px(320.0))
        .truncate()
        .pl(px(10.0))
        .text_color(diagnostic_inline_message_color(severity))
        .child(first_line.to_string())
        .tooltip(crate::root::widgets::text_tooltip(first_line.to_string()))
        .into_any_element()
}

/// Asserts the four severities produce visibly distinct colors, against the same color-mapping
/// functions [`render_file_view_line`] calls (not a reimplemented duplicate). Regression guard:
/// every severity used to collapse onto the same underline/row-bg treatment.
#[cfg(test)]
mod diagnostic_severity_color_tests {
    use super::*;

    #[test]
    fn every_severity_gets_a_visibly_distinct_underline_color() {
        let severities = [
            diagnostics_view::Severity::Error,
            diagnostics_view::Severity::Warning,
            diagnostics_view::Severity::Information,
            diagnostics_view::Severity::Hint,
        ];
        let colors: Vec<gpui::Rgba> = severities
            .iter()
            .map(|severity| diagnostic_underline_color(*severity))
            .collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "{:?} and {:?} must render with visibly distinct underline colors, not \
                     collapse onto the same treatment",
                    severities[i], severities[j]
                );
            }
        }
    }

    #[test]
    fn only_error_gets_a_real_row_background_tint() {
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Error).is_some());
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Warning).is_none());
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Information).is_none());
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Hint).is_none());
    }

    #[test]
    fn a_real_hint_is_visibly_distinct_from_a_real_error_on_every_dimension() {
        assert_ne!(
            diagnostic_underline_color(diagnostics_view::Severity::Error),
            diagnostic_underline_color(diagnostics_view::Severity::Hint),
            "underline colour must differ"
        );
        assert_ne!(
            diagnostic_row_bg(diagnostics_view::Severity::Error),
            diagnostic_row_bg(diagnostics_view::Severity::Hint),
            "row background tint must differ"
        );
        assert_ne!(
            diagnostic_inline_message_color(diagnostics_view::Severity::Error),
            diagnostic_inline_message_color(diagnostics_view::Severity::Hint),
            "inline message colour must differ"
        );
    }
}

/// End-to-end proof of the hover/go-to-definition feature: mirrors
/// [`lsp_diagnostics_wiring_tests`]'s shape (spawn a real `rust-analyzer` through this app's code
/// path, wait for an async result, assert on real content), applied to `request_hover`/
/// `trigger_goto_definition`. Calls `request_hover` directly - the same method
/// `render_file_view_line`'s click handler calls - rather than synthesizing a mouse click on a
/// virtualized row's pixel position.
#[cfg(test)]
mod lsp_hover_wiring_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    fn write_scratch_project(main_rs: &str) -> crate::code_surface::fixtures::TempRepo {
        let dir = temp_repo();
        dir.write(
            "Cargo.toml",
            "[package]\nname = \"app_hover_wiring_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        dir.write("src/main.rs", main_rs);
        dir
    }

    /// A bounded retry loop that re-sends `request_hover` for the same click until it resolves
    /// to a non-empty `HoverRenderModel`, or `deadline` passes. A single request can honestly
    /// come back `Ready(None)` while rust-analyzer is still mid-index; `app.hover` is reset to
    /// `None` between attempts so `request_hover`'s caching doesn't turn a retry into a no-op.
    fn request_hover_until_resolved(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        path: PathBuf,
        deadline: Instant,
    ) -> hover_view::HoverRenderModel {
        loop {
            app.update(cx, |app, cx| {
                app.hover = None;
                app.request_hover(
                    path.clone(),
                    CALL_SITE_LINE,
                    CALL_SITE_BYTE_RANGE,
                    CALL_SITE_POSITION,
                    cx,
                );
            });
            // Each attempt waits for its *own* round trip to settle before the retry below
            // sends another one, so a slow server is waited on rather than flooded.
            let settled = wait_until_parked(cx, ONE_REQUEST_WINDOW, |cx| {
                cx.run_until_parked();
                app.read_with(cx, |app, _| {
                    matches!(
                        app.hover.as_ref().map(|entry| &entry.status),
                        Some(HoverStatus::Ready(_)) | Some(HoverStatus::Failed(_))
                    )
                })
            });
            assert!(
                settled || Instant::now() < deadline,
                "AdeApp::hover never left its real Loading state within the real deadline"
            );
            let resolved = app.read_with(cx, |app, _| match &app.hover {
                Some(HoverEntry {
                    status: HoverStatus::Ready(Some(model)),
                    ..
                }) => Some(model.clone()),
                _ => None,
            });
            if let Some(model) = resolved {
                return model;
            }
            assert!(
                Instant::now() < deadline,
                "AdeApp::request_hover never resolved to a real, non-empty hover for the real \
                 call site within the real deadline - rust-analyzer kept answering with real \
                 \"nothing here\"/errors past its own indexing window"
            );
        }
    }

    /// How long one real request is given to settle before the enclosing retry loop sends
    /// another. Its own bound, separate from the whole test's `deadline`.
    const ONE_REQUEST_WINDOW: Duration = Duration::from_secs(5);

    /// Drives real renders until `key`'s language server reports `Ready`, or the tier's real
    /// deadline passes. Re-rendering (not just waiting) is load-bearing: `render_file_view` is
    /// what spawns the client and, once it is Ready, what dispatches `didOpen` - and there is no
    /// window compositor driving repaints in a headless test.
    fn wait_for_ready_client(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        root: &Path,
        key: &str,
    ) {
        let ready = wait_until_parked(cx, CLIENT_HANDSHAKE_WINDOW, |cx| {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            app.read_with(cx, |app, _| {
                matches!(
                    app.lsp_clients.get(&(root.to_path_buf(), key)),
                    Some(LspClientState::Ready(_))
                )
            })
        });
        assert!(
            ready,
            "the real {key} client never became Ready within {CLIENT_HANDSHAKE_WINDOW:?}"
        );
    }

    const CLIENT_HANDSHAKE_WINDOW: Duration = Duration::from_secs(120);

    /// Click position for `"    let result = add_one(41);"` (line index 8, 0-based), reused from
    /// `lsp_core::client::tests::rust_analyzer_returns_a_real_hover_for_a_documented_function`'s
    /// fixture. The `add_one` call-site identifier spans bytes 17..24 of that line (`"    let
    /// result = "` is 17 bytes, `"add_one"` is 7 more) - computed by hand since this test calls
    /// `request_hover` directly rather than through a render/click.
    const FIXTURE_SOURCE: &str = "/// Adds one to the given number.\n\
         ///\n\
         /// Returns the incremented value.\n\
         fn add_one(x: i32) -> i32 {\n    x + 1\n}\n\n\
         fn main() {\n    let result = add_one(41);\n    println!(\"{}\", result);\n}\n";
    const CALL_SITE_LINE: usize = 9; // 1-based - `AdeApp::code_cursor`'s own convention.
    const CALL_SITE_BYTE_RANGE: Range<usize> = 17..24;
    const CALL_SITE_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 8,
        character: 20,
    };

    #[gpui::test]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    fn a_real_click_resolves_to_a_real_hover_render_model(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();
        // `render_file_view` spawns the LspClient but only runs as part of a render; there's no
        // window compositor driving repaints in this headless test, so drive it directly.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        // Keeps re-rendering (the trigger for `dispatch_did_open` once the client is Ready)
        // while waiting for the handshake - a single "wait, then render once" could leave
        // `didOpen` never sent for a client that only just became Ready.
        wait_for_ready_client(&app, cx, project.path(), "rust-analyzer");

        let deadline = Instant::now() + Duration::from_secs(120);
        let model = request_hover_until_resolved(&app, cx, main_rs.clone(), deadline);

        assert!(
            model.signature.contains("add_one"),
            "expected the real signature to mention the real function name, got: {:?}",
            model.signature
        );
        assert!(
            model.signature.contains("i32"),
            "expected the real signature to mention the real i32 type, got: {:?}",
            model.signature
        );
        let doc = model.doc.as_deref().unwrap_or_default();
        assert!(
            doc.contains("Adds one to the given number"),
            "expected the real doc comment text to reach the render model, got: {doc:?}"
        );
        app.read_with(cx, |app, _| {
            let entry = app.hover.as_ref().expect("a real hover entry should exist");
            assert_eq!(entry.path, main_rs);
        });
    }

    #[gpui::test]
    fn f12_action_reaches_the_real_handler_on_a_fresh_window(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        assert_eq!(
            app.read_with(cx, |app, _| app.hover.clone()),
            None,
            "sanity check: a fresh window has no real hover entry yet"
        );

        cx.dispatch_action(GotoDefinition);
        cx.run_until_parked();

        // `handle_goto_definition_action`'s only effect with `hover == None` is a harmless early
        // return; confirming the app is unchanged is the available proof dispatch reached it.
        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
    }

    #[gpui::test]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    fn f12_action_navigates_to_the_real_definition_line(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        // See the hover test above's identical loop for why re-rendering, not just waiting,
        // matters.
        wait_for_ready_client(&app, cx, project.path(), "rust-analyzer");

        // `trigger_goto_definition` reads `hover`'s path/position regardless of whether the
        // hover content itself has resolved - a `Loading` entry already carries a valid request
        // target, so this only needs `request_hover` to have been called once.
        app.update(cx, |app, cx| {
            app.request_hover(
                main_rs.clone(),
                CALL_SITE_LINE,
                CALL_SITE_BYTE_RANGE,
                CALL_SITE_POSITION,
                cx,
            );
        });
        cx.run_until_parked();

        // Retried rather than called once: a `textDocument/definition` response can honestly be
        // empty while rust-analyzer is still mid-index, and a real user would just press F12
        // again. The request itself runs on a background OS thread, so each attempt polls (which
        // re-drains the executor between tries) rather than reading once.
        let navigated = wait_until_parked(cx, DEFINITION_WINDOW, |cx| {
            app.update(cx, |app, cx| {
                app.trigger_goto_definition(cx);
            });
            wait_until_parked(cx, ONE_REQUEST_WINDOW, |cx| {
                cx.run_until_parked();
                // `fn add_one` is on line 4 (1-based), different from `CALL_SITE_LINE` (9),
                // proving this is real navigation, not a no-op that left the cursor where it was.
                app.read_with(cx, |app, _| app.code_cursor == Some(4))
            })
        });
        assert!(
            navigated,
            "trigger_goto_definition never navigated AdeApp::code_cursor to the real definition \
             line within {DEFINITION_WINDOW:?} - last observed code_cursor: {:?}",
            app.read_with(cx, |app, _| app.code_cursor)
        );
    }

    /// How long the whole retry sequence for one real `textDocument/definition` answer is given.
    const DEFINITION_WINDOW: Duration = Duration::from_secs(120);

    /// Real, end-to-end coverage for the Ctrl/Cmd+click go-to-definition affordance: a real
    /// simulated mouse click, with a real secondary modifier held, on the real painted call-site
    /// token, against a genuinely spawned rust-analyzer - not a call straight into
    /// `trigger_goto_definition` the way [`f12_action_navigates_to_the_real_definition_line`]
    /// tests the mechanism itself. This is what actually proves the click *routes* there.
    #[gpui::test]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    fn ctrl_click_on_a_real_token_navigates_to_its_real_definition(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = open_test_app(cx, project.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        wait_for_ready_client(&app, cx, project.path(), "rust-analyzer");

        let (row_bounds, shaped) = app
            .read_with(cx, |app, _| {
                app.file_view_row_layout.get(&CALL_SITE_LINE).cloned()
            })
            .expect("the call-site row must have real painted layout by now");
        let click_point = gpui::point(
            row_bounds.left() + shaped.x_for_index(CALL_SITE_BYTE_RANGE.start + 1),
            row_bounds.center().y,
        );
        // The real modifier the production click handler checks for (`Modifiers::secondary()`),
        // which is Cmd on macOS and Ctrl elsewhere - held for real, not stood in for.
        let ctrl_click = gpui::Modifiers::secondary_key();

        // Retried the same real way `f12_action_navigates_to_the_real_definition_line` retries
        // its own equivalent request: a real `textDocument/definition` response can honestly come
        // back empty while rust-analyzer is still mid-index, and a real user would just click
        // again. Re-clicking (not just re-waiting) is load-bearing here - a single stale response
        // means nothing will ever change without a fresh request.
        let navigated = wait_until_parked(cx, DEFINITION_WINDOW, |cx| {
            cx.simulate_click(click_point, ctrl_click);
            wait_until_parked(cx, ONE_REQUEST_WINDOW, |cx| {
                cx.run_until_parked();
                // `fn add_one` is on line 4 (1-based), different from `CALL_SITE_LINE` (9) - real
                // navigation, not the click's own plain caret placement being mistaken for it.
                app.read_with(cx, |app, _| app.code_cursor == Some(4))
            })
        });
        assert!(
            navigated,
            "a real Ctrl+click on the real call-site token never navigated AdeApp::code_cursor \
             to the real definition line within {DEFINITION_WINDOW:?} - last observed \
             code_cursor: {:?}",
            app.read_with(cx, |app, _| app.code_cursor)
        );
    }

    #[gpui::test]
    fn a_plain_click_with_no_modifier_does_not_trigger_navigation(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn one() {}\nfn two() {}\n").expect("write sample.rs");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        // A real, deliberately stale hover entry, seeded directly (the same established pattern
        // `hover_popover_position_tests` uses) - if a plain click ever started dismissing it via
        // the Ctrl+click path by mistake, this would still catch that as a real behavior change,
        // even though this test's main point is that nothing *navigates*.
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path.clone(),
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn one()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();

        let row_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&2).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 2's real row should have painted real layout by now");
        cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.code_cursor),
            Some(2),
            "a plain click must still move the real caret to the clicked line"
        );
        assert!(
            app.read_with(cx, |app, _| app.hover.is_none()),
            "a plain click must still dismiss a real, stale hover card - GitHub issue #186's own \
             existing behavior, which the Ctrl+click addition must not have disturbed"
        );
    }
}

/// Real regression coverage for bug 2 in this revision's brief: the Hover popover used to render
/// at a fixed position (an in-flow card below the code, effectively at whatever a given render
/// pass's flow put it - in practice, the bottom of the visible content) instead of anchored near
/// the real hovered token. These tests bypass the real `textDocument/hover` round trip entirely
/// (seeding [`AdeApp::hover`] directly, the same established pattern
/// `stale_completions_popup_tests::fake_ready_entry` already uses for completions) since the bug
/// and its fix are purely about *where* an already-resolved hover result paints, never about the
/// real LSP request/response plumbing itself (out of scope per this revision's own brief).
#[cfg(test)]
mod hover_popover_position_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    fn seed_ready_hover_for_line(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        path: PathBuf,
        line_number: usize,
    ) {
        seed_ready_hover_with_signature(app, cx, path, line_number, "fn real_symbol()");
    }

    fn seed_ready_hover_with_signature(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        path: PathBuf,
        line_number: usize,
        signature: &str,
    ) {
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path,
                line_number,
                byte_range: 0..3,
                position: lsp_core::lsp_types::Position {
                    line: line_number as u32 - 1,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: signature.to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_genuinely_empty_hover_result_paints_no_popup_at_all(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write sample.rs");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path.clone(),
                line_number: 1,
                byte_range: 0..3,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Loading,
            });
            cx.notify();
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "sanity check: the loading state itself still paints a real card"
        );

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..3,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(None),
            });
            cx.notify();
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("hover-card").is_none(),
            "a genuinely empty real hover answer must paint nothing at all, not an empty \"no \
             symbol information here\" card"
        );
    }

    #[gpui::test]
    fn a_real_long_signature_wraps_inside_the_card_instead_of_being_clipped(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write sample.rs");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        // A real, long, genuinely multi-token Rust signature - comfortably wider than the card's
        // own `max_w(430px)` if painted on a single line at 11.5px monospace, with enough real
        // keyword/type/punctuation tokens for tree-sitter to split into many separate runs.
        let long_signature = "pub fn process(&self, input: Result<HashMap<String, \
                               Vec<Option<ComplexNestedGenericType>>>, \
                               MyCustomErrorTypeWithALongName>) -> AnotherReallyLongReturnTypeName";
        seed_ready_hover_with_signature(&app, cx, file_path, 1, long_signature);

        let card = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds");
        let first_token = cx
            .debug_bounds("hover-signature-token-0")
            .expect("the real signature's own first painted run should have painted real bounds");
        // Real, verified-by-inspection tree-sitter-rust output for this exact fixture: 33 real
        // runs, with the 25th (index 24, the `AnotherReallyLongReturnTypeName` return type)
        // landing on the real card's second painted line - comfortably past both the first
        // line's own real token count and any plausible off-by-a-few tokenizer drift.
        let later_token = cx.debug_bounds("hover-signature-token-24").expect(
            "a real signature this long and this token-dense must genuinely produce at least 25 \
             separate highlighted runs - if this fails, either the fixture stopped being \
             realistic or highlight_block's own tokenization regressed",
        );

        assert!(
            later_token.right() <= card.right() + gpui::px(1.0),
            "no real signature token may paint past the real card's own right edge (token right \
             {:?}, card right {:?}) - a token painting past the card's edge is exactly the \
             pre-fix clipping bug",
            later_token.right(),
            card.right()
        );
        assert!(
            later_token.top() > first_token.top(),
            "a real signature this long must genuinely wrap onto more than one line - a later \
             real run must paint on a real, lower line than the first, not side-by-side on one \
             unbroken (and therefore clipped) line (first token top {:?}, later token top {:?})",
            first_token.top(),
            later_token.top()
        );
        assert!(
            card.size.height > px(60.0),
            "a real signature that genuinely wraps across several lines must grow the real \
             card's own painted height well past a single line's (~31px) - a card that stayed \
             short would mean the wrap never really happened (got {:?})",
            card.size.height
        );
    }

    #[gpui::test]
    fn a_card_with_no_room_below_sits_against_its_row_not_a_card_height_above_it(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.txt");
        // Long enough that its later rows are genuinely near the bottom of the painted body -
        // the only way to make a card flip for real rather than by faking bounds.
        let source: String = (1..=200).map(|index| format!("line {index}\n")).collect();
        std::fs::write(&file_path, &source).expect("write sample.txt");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        // The lowest row this frame actually painted - by construction the one with the least
        // room beneath it, so it is the row whose card must flip.
        let (target, row_top, body_bottom) = app.read_with(cx, |app, _| {
            let body_bottom = app.body_bounds.bottom();
            let (line, (bounds, _)) = app
                .file_view_row_layout
                .iter()
                .filter(|(_, (bounds, _))| bounds.bottom() <= body_bottom)
                .max_by(|left, right| {
                    f32::from(left.1 .0.top())
                        .partial_cmp(&f32::from(right.1 .0.top()))
                        .expect("real, finite painted bounds")
                })
                .expect("a real painted row");
            (*line, bounds.top(), body_bottom)
        });

        seed_ready_hover_for_line(&app, cx, file_path, target);
        let card = cx
            .debug_bounds("hover-card")
            .expect("a real hover on a real row must paint a real card");

        assert!(
            body_bottom - row_top < HOVER_CARD_MAX_HEIGHT,
            "sanity check: this row must genuinely have too little room below it, or the test is \
             measuring the un-flipped case (row top {row_top:?}, body bottom {body_bottom:?})"
        );
        assert!(
            card.bottom() <= row_top + px(1.0),
            "a flipped card must sit above the row it describes, not over it (card bottom {:?}, \
             row top {row_top:?})",
            card.bottom()
        );
        let gap = row_top - card.bottom();
        assert!(
            gap <= px(2.0),
            "a flipped card must sit *against* its row, not a worst-case card height above it - \
             got a {gap:?} gap, which is the reported bug (card {card:?}, row top {row_top:?})"
        );
    }

    #[gpui::test]
    fn the_real_painted_hover_card_moves_with_the_real_hovered_row(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample.txt");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        let row_2_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&2).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 2's real row should have painted real layout by now");
        let row_4_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&4).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 4's real row should have painted real layout by now");
        assert!(
            row_4_bounds.top() > row_2_bounds.top(),
            "sanity check: line 4 must really paint below line 2"
        );

        seed_ready_hover_for_line(&app, cx, file_path.clone(), 2);
        let card_for_line_2 = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for line 2");
        assert!(
            (card_for_line_2.top() - row_2_bounds.bottom()).abs() < gpui::px(2.0),
            "the real hover card for line 2 must paint directly under line 2's own real row \
             (row bottom {:?}, card top {:?})",
            row_2_bounds.bottom(),
            card_for_line_2.top()
        );

        seed_ready_hover_for_line(&app, cx, file_path, 4);
        let card_for_line_4 = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for line 4");
        assert!(
            (card_for_line_4.top() - row_4_bounds.bottom()).abs() < gpui::px(2.0),
            "the real hover card for line 4 must paint directly under line 4's own real row \
             (row bottom {:?}, card top {:?})",
            row_4_bounds.bottom(),
            card_for_line_4.top()
        );

        assert!(
            card_for_line_4.top() > card_for_line_2.top(),
            "hovering a real, later line must move the real painted popover further down - a \
             fixed-position popover (the real bug this revision fixes) would paint both at the \
             exact same spot: line 2's card was at {:?}, line 4's card was at {:?}",
            card_for_line_2.top(),
            card_for_line_4.top()
        );
    }

    #[gpui::test]
    fn the_real_painted_hover_card_moves_horizontally_with_the_real_hovered_column(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.txt");
        std::fs::write(&file_path, "aaaa bbbb cccc dddd\n").expect("write sample.txt");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        let row_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&1).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 1's real row should have painted real layout by now");

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path.clone(),
                line_number: 1,
                byte_range: 0..4,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn real_symbol()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
        let card_for_first_token = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for the first token");

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 15..19,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 15,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn real_symbol()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
        let card_for_last_token = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for the last token");

        assert!(
            card_for_last_token.left() > card_for_first_token.left(),
            "hovering a real token further into the same line must move the real painted card's \
             own real left edge to the right - a fixed-position popover would paint both at the \
             exact same horizontal spot: first-token card was at {:?}, last-token card was at \
             {:?} (row bounds: {:?})",
            card_for_first_token.left(),
            card_for_last_token.left(),
            row_bounds
        );
    }
}

/// The real, live end-to-end proof for this app's two-server (primary + companion) LSP support:
/// a genuine `.vue` file, analyzed by a genuinely spawned `vue-language-server` **and** a
/// genuinely spawned `typescript-language-server` carrying the real `@vue/typescript-plugin`,
/// coordinated by this app's own real relay - reaching real diagnostics and a real hover through
/// nothing but the production code path (`AdeApp::open_file_view` -> `render_file_view` ->
/// `ensure_lsp_client` (both halves) -> `dispatch_did_open` (both halves) -> render).
#[cfg(test)]
mod vue_two_server_wiring_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    /// A real `<script setup lang="ts">` type error (`bad`) plus a real template compile error
    /// (the mismatched `</span>`), in one real single-file component. The `shape`/`picked` lines
    /// exist for the real go-to-definition and completion probes below - a genuine cross-line
    /// reference and a genuine member-access position, both valid TypeScript so they can't disturb
    /// the two deliberate errors above them.
    const FIXTURE_VUE: &str = "<script setup lang=\"ts\">\n\
         const bad: number = \"not a number\"\n\
         const shape = { alpha: 1, beta: 2 }\n\
         const picked = shape.alpha\n\
         </script>\n\
         \n\
         <template>\n\
         \x20 <div>{{ bad }}</span>\n\
         </template>\n";

    /// 0-based line 1, character 7 - inside the real `bad` identifier of
    /// `const bad: number = "not a number"`. `request_hover`'s `line_number` is 1-based (see
    /// `AdeApp::code_cursor`'s own convention) and its `byte_range` is within that line's text:
    /// `"const "` is 6 bytes, `"bad"` 3 more.
    const SCRIPT_LINE: usize = 2;
    const SCRIPT_BYTE_RANGE: Range<usize> = 6..9;
    const SCRIPT_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 1,
        character: 7,
    };

    /// 0-based line 3, inside the `shape` of `const picked = shape.alpha` (`"const picked = "` is
    /// 15 characters, `shape` the next 5) - a real reference whose real declaration is one line up.
    const REFERENCE_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 3,
        character: 17,
    };
    /// The same line, immediately after the real `.` (character 20) - a genuine member-access
    /// completion context on a genuinely typed object.
    const MEMBER_ACCESS_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 3,
        character: 21,
    };

    /// Writes the real scratch Vue project and performs the real, project-local
    /// `npm install typescript@5` this app's own `--tsdk` resolution genuinely needs (see
    /// `crate::language::vue_dynamic_args`: it existence-checks
    /// `node_modules/typescript/lib/typescript.js` specifically, and refuses to spawn without it).
    /// A stubbed stand-in would be worse than useless here - the real `vue-language-server` really
    /// does load this file, and a fake one would produce a different, equally fake failure.
    fn write_scratch_vue_project() -> crate::code_surface::fixtures::TempRepo {
        let dir = temp_repo();
        dir.write(
            "tsconfig.json",
            "{\"compilerOptions\": {\"strict\": true, \"target\": \"ES2020\", \
             \"module\": \"ESNext\", \"moduleResolution\": \"Bundler\", \"jsx\": \"preserve\"}, \
             \"include\": [\"**/*.ts\", \"**/*.vue\"]}\n",
        );
        dir.write("App.vue", FIXTURE_VUE);
        let status = std::process::Command::new("npm")
            .args([
                "install",
                "typescript@5",
                "--no-audit",
                "--no-fund",
                "--silent",
            ])
            .current_dir(dir.path())
            .status()
            .expect("npm should be on PATH in this sandbox (real, live network install)");
        assert!(status.success(), "npm install typescript@5 failed");
        assert!(
            dir.path()
                .join("node_modules/typescript/lib/typescript.js")
                .is_file(),
            "the real --tsdk target this app resolves must genuinely exist after the install"
        );
        dir
    }

    /// Re-renders, advances the deterministic clock past one `LSP_DIAGNOSTICS_POLL_INTERVAL`, and
    /// drains, until `predicate` holds or `deadline` passes.
    ///
    /// Advancing the clock is load-bearing here and not in the single-server tests: this app's
    /// relay dispatch lives in `AdeApp::ensure_lsp_poll_task`'s own `timer(..)`-driven loop, which
    /// on GPUI's deterministic test executor only ticks when the clock is actually advanced - and
    /// the real `vue-language-server` will not produce a single diagnostic until its relayed
    /// `tsserver/request` has been answered.
    fn wait_for(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        deadline: Duration,
        message: &str,
        predicate: impl Fn(&AdeApp) -> bool,
    ) {
        let held = wait_until_parked(cx, deadline, |cx| {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.background_executor
                .advance_clock(LSP_DIAGNOSTICS_POLL_INTERVAL + Duration::from_millis(10));
            cx.run_until_parked();
            app.read_with(cx, |app, _| predicate(app))
        });
        assert!(held, "{message}");
    }

    fn diagnostic_messages(app: &AdeApp) -> Vec<String> {
        app.file_view_diagnostics
            .values()
            .flatten()
            .map(|diagnostic| diagnostic.message.clone())
            .collect()
    }

    #[gpui::test]
    #[ignore = "external: vue-language-server, typescript-language-server, npm; see docs/testing.md"]
    fn a_real_vue_file_gets_real_diagnostics_from_both_servers_and_a_real_hover(
        cx: &mut TestAppContext,
    ) {
        // Silent unless `RUST_LOG` is actually set, so this doesn't spam the normal suite - but
        // `RUST_LOG=app::lsp::client=debug cargo test ...` then surfaces the real relay round-trip
        // timing `AdeApp::dispatch_companion_relay` logs (measured live at ~223ms for the real
        // `_vue:projectInfo` query while building this).
        let _ = env_logger::builder().parse_default_env().try_init();
        let project = write_scratch_vue_project();
        let app_vue = project.path().join("App.vue");
        let (app, cx) = open_test_app(cx, project.path().to_path_buf());

        let opened_at = Instant::now();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(app_vue.clone(), window, cx);
        });
        cx.run_until_parked();

        // Both halves must genuinely be spawned by the production path - two real, independently
        // keyed clients, not one client pretending to be two.
        let companion_key = language::companion_for_extension(Some("vue"))
            .expect("vue has a real companion")
            .client_key;
        wait_for(
            &app,
            cx,
            Duration::from_secs(180),
            "the real vue-language-server and its real typescript-language-server companion were \
             never both Ready - check that both are installed and that @vue/typescript-plugin \
             resolved",
            |app| {
                let root = &app.file_tree_root;
                [("vue-language-server"), (companion_key)]
                    .iter()
                    .all(|key| {
                        matches!(
                            app.lsp_clients.get(&(root.clone(), *key)),
                            Some(LspClientState::Ready(_))
                        )
                    })
            },
        );

        // The companion's own contribution: a real TypeScript semantic error for `.vue` content,
        // which only exists because the real `@vue/typescript-plugin` is genuinely loaded.
        wait_for(
            &app,
            cx,
            Duration::from_secs(180),
            "no real TypeScript diagnostic for the genuine `.vue` script type error ever reached \
             file_view_diagnostics",
            |app| {
                diagnostic_messages(app)
                    .iter()
                    .any(|message| message.to_lowercase().contains("not assignable"))
            },
        );
        let companion_diagnostic_at = opened_at.elapsed();

        // The primary's own contribution: a real Vue template compile error, which the companion
        // does not and cannot report.
        wait_for(
            &app,
            cx,
            Duration::from_secs(120),
            "no real Vue template diagnostic ever reached file_view_diagnostics - without it, \
             only one of the two real servers is genuinely contributing",
            |app| {
                diagnostic_messages(app)
                    .iter()
                    .any(|message| message.to_lowercase().contains("end tag"))
            },
        );

        app.read_with(cx, |app, _| {
            let messages = diagnostic_messages(app);
            println!(
                "vue e2e: real merged diagnostics after {:?}: {messages:?}",
                opened_at.elapsed()
            );
            assert!(
                messages
                    .iter()
                    .any(|message| message.to_lowercase().contains("not assignable")),
                "the companion's real TypeScript diagnostic must still be present in the merged \
                 view alongside the primary's, got: {messages:?}"
            );
            assert_eq!(
                app.lsp_clients.len(),
                2,
                "exactly two real clients - one primary, one companion - should exist for this \
                 repo root, got: {:?}",
                app.lsp_clients.keys().collect::<Vec<_>>()
            );
        });
        println!(
            "vue e2e: first real companion diagnostic reached the render path in \
             {companion_diagnostic_at:?} from open_file_view"
        );

        // A real hover, through the facade's real companion fallback: the primary answers `null`
        // for every position in a `.vue` file (real, expected hybrid-mode behavior), so a
        // non-`None` result here can only have come from the companion via `LspConnection`.
        let hover_started = Instant::now();
        let mut resolved = None;
        // Each attempt waits out its own real round trip before the next one is sent, so a
        // still-indexing server is waited on rather than flooded with fresh requests.
        let answered = wait_until_parked(cx, Duration::from_secs(120), |cx| {
            app.update(cx, |app, cx| {
                app.hover = None;
                app.request_hover(
                    app_vue.clone(),
                    SCRIPT_LINE,
                    SCRIPT_BYTE_RANGE,
                    SCRIPT_POSITION,
                    cx,
                );
            });
            wait_until_parked(cx, Duration::from_secs(5), |cx| {
                cx.run_until_parked();
                app.read_with(cx, |app, _| {
                    matches!(
                        app.hover.as_ref().map(|entry| &entry.status),
                        Some(HoverStatus::Ready(_)) | Some(HoverStatus::Failed(_))
                    )
                })
            });
            resolved = app.read_with(cx, |app, _| match &app.hover {
                Some(HoverEntry {
                    status: HoverStatus::Ready(Some(model)),
                    ..
                }) => Some(model.clone()),
                _ => None,
            });
            resolved.is_some()
        });
        assert!(
            answered,
            "no real, non-empty hover ever came back for the genuine `bad` identifier in the \
             real .vue script block - the companion fallback in LspConnection::request is \
             what has to supply it, since the primary genuinely answers null there"
        );
        let model = resolved.expect("the wait above only succeeds with a real resolved hover");
        println!(
            "vue e2e: real hover resolved in {:?}: {model:?}",
            hover_started.elapsed()
        );
        let rendered = format!("{model:?}");
        assert!(
            rendered.contains("bad") && rendered.contains("number"),
            "the real hover should describe the genuine `const bad: number` declaration, got: \
             {rendered}"
        );

        // Revision R11 audit finding 1, proven against the real toolchain rather than only against
        // `lsp_connection_facade_tests`' small real servers: hover was never the only request the
        // real primary answers emptily inside a `.vue` script block. Both of these go through the
        // exact same `LspConnection::request` the production F12 handler and
        // `AdeApp::schedule_lsp_sync`'s completion request use, and before this fix both came back
        // empty here - go-to-definition as an empty *array* and completion as an empty
        // `CompletionList`, neither of which the old null-only check recognized.
        let connection = app
            .read_with(cx, |app, _| app.lsp_connection_for_path(&app_vue))
            .expect("both real halves are Ready by now");
        let uri = lsp_core::LspClient::uri_for_path(&app_vue).expect("a real file:// uri");

        let definition = retry_until_some(
            Duration::from_secs(120),
            "no real go-to-definition ever came back for `shape` in the real .vue script block - \
             the real vue-language-server answers an empty array there, so only the companion \
             fallback in LspConnection::request can supply one",
            || {
                let params = lsp_core::lsp_types::GotoDefinitionParams {
                    text_document_position_params:
                        lsp_core::lsp_types::TextDocumentPositionParams {
                            text_document: lsp_core::lsp_types::TextDocumentIdentifier {
                                uri: uri.clone(),
                            },
                            position: REFERENCE_POSITION,
                        },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                };
                match connection
                    .request::<lsp_core::lsp_types::request::GotoDefinition>(
                        params,
                        LSP_QUERY_TIMEOUT,
                    )
                    .ok()
                    .flatten()
                {
                    Some(lsp_core::lsp_types::GotoDefinitionResponse::Array(locations))
                        if locations.is_empty() =>
                    {
                        None
                    }
                    other => other,
                }
            },
        );
        println!("vue e2e: real go-to-definition answer: {definition:?}");

        let completions = retry_until_some(
            Duration::from_secs(120),
            "no real completions ever came back after `shape.` in the real .vue script block - \
             the real vue-language-server answers an empty items list there",
            || {
                let params = lsp_core::lsp_types::CompletionParams {
                    text_document_position: lsp_core::lsp_types::TextDocumentPositionParams {
                        text_document: lsp_core::lsp_types::TextDocumentIdentifier {
                            uri: uri.clone(),
                        },
                        position: MEMBER_ACCESS_POSITION,
                    },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                    context: None,
                };
                let items = match connection
                    .request::<lsp_core::lsp_types::request::Completion>(params, LSP_QUERY_TIMEOUT)
                    .ok()
                    .flatten()
                {
                    Some(lsp_core::lsp_types::CompletionResponse::Array(items)) => items,
                    Some(lsp_core::lsp_types::CompletionResponse::List(list)) => list.items,
                    None => Vec::new(),
                };
                (!items.is_empty()).then_some(items)
            },
        );
        let labels: Vec<String> = completions.iter().map(|item| item.label.clone()).collect();
        println!("vue e2e: real completion labels after `shape.`: {labels:?}");
        assert!(
            labels.iter().any(|label| label == "alpha")
                && labels.iter().any(|label| label == "beta"),
            "the real companion knows both members of the genuine `{{ alpha, beta }}` object, \
             got: {labels:?}"
        );
    }

    /// Calls `attempt` until it returns a real `Some` or `deadline` passes. Both real servers are
    /// still settling for a while after the first diagnostic lands, so a single shot would be a
    /// race; nothing is fabricated on timeout, the assertion just fails.
    fn retry_until_some<T>(
        deadline: Duration,
        message: &str,
        attempt: impl Fn() -> Option<T>,
    ) -> T {
        let mut resolved = None;
        let answered = test_support::wait_until(deadline, || {
            resolved = attempt();
            resolved.is_some()
        });
        assert!(answered, "{message}");
        resolved.expect("the wait above only succeeds with a real answer")
    }
}

/// GitHub issue #186's real coverage for Surface C's Hover popup: that it is triggered by the real
/// pointer resting on a real token (not by a click, which is what it used to be), and that it
/// genuinely goes away again - by moving the pointer off the token, by clicking elsewhere in the
/// editor, and by `Escape`. Before this issue there was no dismissal path of any kind: an opened
/// card only ever went away by switching tab/file/worktree.
#[cfg(test)]
mod hover_pointer_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::spawn_fake_server;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    fn open_file<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        source: &str,
    ) -> (
        crate::code_surface::fixtures::TempRepo,
        PathBuf,
        Entity<AdeApp>,
        &'a mut VisualTestContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join(name);
        std::fs::write(&file_path, source).expect("write fixture");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        (repo, file_path, app, cx)
    }

    /// The real painted `(Bounds, ShapedLine)` for `line_number`, panicking with a useful message
    /// rather than silently skipping the assertions if the row never painted.
    fn row_layout(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        line_number: usize,
    ) -> (gpui::Bounds<Pixels>, gpui::ShapedLine) {
        app.read_with(cx, |app, _| {
            app.file_view_row_layout.get(&line_number).cloned()
        })
        .unwrap_or_else(|| panic!("line {line_number}'s real row should have painted real layout"))
    }

    /// A real window-space point in the middle of `byte_range`'s real glyphs on `line_number`.
    fn point_on_token(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        line_number: usize,
        byte_range: Range<usize>,
    ) -> gpui::Point<Pixels> {
        let (bounds, shaped) = row_layout(app, cx, line_number);
        let start = shaped.x_for_index(byte_range.start);
        let end = shaped.x_for_index(byte_range.end);
        gpui::point(bounds.left() + (start + end) / 2.0, bounds.center().y)
    }

    fn seed_ready_hover(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        path: PathBuf,
        line_number: usize,
        byte_range: Range<usize>,
    ) {
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path,
                line_number,
                byte_range,
                position: lsp_core::lsp_types::Position {
                    line: line_number as u32 - 1,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn real_symbol()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_real_mouse_move_onto_a_real_token_arms_a_real_hover_and_a_real_click_does_not(
        cx: &mut TestAppContext,
    ) {
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.rs", "fn alpha() {}\nfn beta() {}\n");

        let on_alpha = point_on_token(&app, cx, 1, 3..8);
        cx.simulate_mouse_move(on_alpha, None, gpui::Modifiers::none());
        cx.run_until_parked();

        let pending = app
            .read_with(cx, |app, _| app.hover_pending.clone())
            .expect("a real mouse move onto a real token must arm a real hover anchor");
        assert_eq!(pending.path, file_path);
        assert_eq!(pending.line_number, 1);
        assert!(
            pending.byte_range.contains(&4),
            "the armed anchor must cover the real `alpha` token the pointer is actually on, got \
             bytes {:?} of `fn alpha() {{}}`",
            pending.byte_range
        );
        assert_eq!(pending.position.line, 0);
        assert!(
            app.read_with(cx, |app, _| app.hover.is_none()),
            "nothing may be requested or painted before the real trigger delay has elapsed - a \
             pointer merely sweeping across a line must not fire a real LSP request per token"
        );

        cx.background_executor
            .advance_clock(HOVER_TRIGGER_DELAY + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.hover_pending.is_none()),
            "once the real trigger delay elapses the armed anchor must be consumed by a real \
             `request_hover`, not left armed forever"
        );

        app.update(cx, |app, _| {
            app.dismiss_hover();
        });
        cx.simulate_click(on_alpha, gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.hover_pending.is_none()
                && app.hover.is_none()),
            "a real click must no longer open the Hover popup - that was the trigger this issue \
             replaces, and it is now what dismisses it"
        );
    }

    #[gpui::test]
    fn moving_the_real_pointer_off_the_real_anchor_token_dismisses_the_real_card(
        cx: &mut TestAppContext,
    ) {
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.txt", "alpha bravo\ncharlie delta\n");
        seed_ready_hover(&app, cx, file_path, 1, 0..5);
        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "sanity check: the real card must be painted before this test dismisses it"
        );

        let (row_bounds, _) = row_layout(&app, cx, 1);
        let away = gpui::point(row_bounds.left(), row_bounds.top() - gpui::px(120.0));
        cx.simulate_mouse_move(away, None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "the real dismiss is debounced (HOVER_HIDE_DELAY) - the card must still be up the \
             instant the pointer leaves, not vanish synchronously"
        );

        cx.background_executor
            .advance_clock(HOVER_HIDE_DELAY + std::time::Duration::from_millis(10));
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.hover.is_none()),
            "once the real hide delay elapses, moving the pointer away from the real anchor \
             token must dismiss the real card"
        );
        assert!(
            cx.debug_bounds("hover-card").is_none(),
            "and the real card must genuinely stop painting, not just lose its state"
        );
    }

    #[gpui::test]
    fn a_brief_pass_through_whitespace_does_not_flash_the_card_off(cx: &mut TestAppContext) {
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.txt", "alpha bravo\ncharlie delta\n");
        seed_ready_hover(&app, cx, file_path, 1, 0..5);
        assert!(cx.debug_bounds("hover-card").is_some());

        // The real space character between "alpha" and "bravo" - `hover_anchor_at` returns `None`
        // here (real whitespace, no token), matching `track_hover_pointer`'s "anything else" arm.
        let (row_bounds, shaped) = row_layout(&app, cx, 1);
        let gap_x = (shaped.x_for_index(5) + shaped.x_for_index(6)) / 2.0;
        let gap = gpui::point(row_bounds.left() + gap_x, row_bounds.center().y);
        cx.simulate_mouse_move(gap, None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "a brief pass through plain whitespace must not clear an already-visible card before \
             HOVER_HIDE_DELAY genuinely elapses"
        );
        assert!(
            app.read_with(cx, |app, _| app.hover.is_some()),
            "the underlying state must likewise still describe the original token"
        );
    }

    #[gpui::test]
    fn moving_the_real_pointer_onto_the_real_card_itself_keeps_it_open(cx: &mut TestAppContext) {
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.txt", "alpha bravo\ncharlie delta\n");
        seed_ready_hover(&app, cx, file_path, 1, 0..5);
        let card = cx
            .debug_bounds("hover-card")
            .expect("the real card must be painted before this test moves onto it");

        cx.simulate_mouse_move(card.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.hover.is_some()),
            "the real card must survive the pointer moving onto it - it is the only way to reach \
             its own `F12 definition` footer"
        );
        assert!(cx.debug_bounds("hover-card").is_some());
    }

    #[gpui::test]
    fn hovering_a_real_token_visually_covered_by_the_card_does_not_switch_or_dismiss_it(
        cx: &mut TestAppContext,
    ) {
        // `"fn alpha() {}\nfn beta() {}\n"` - `alpha` is on line 1 (bytes 3..8), `beta` on line 2
        // (bytes 3..7), directly beneath it.
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.rs", "fn alpha() {}\nfn beta() {}\n");
        seed_ready_hover(&app, cx, file_path, 1, 3..8);

        let card = cx
            .debug_bounds("hover-card")
            .expect("the real card must be painted before this test moves within it");
        let beta_point = point_on_token(&app, cx, 2, 3..7);
        assert!(
            card.contains(&beta_point),
            "sanity check: the real card anchored under line 1 must genuinely cover line 2's own \
             real row underneath it for this test to prove anything - card {card:?}, beta's real \
             point {beta_point:?}"
        );

        cx.simulate_mouse_move(beta_point, None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app
                .hover
                .as_ref()
                .is_some_and(
                    |entry| entry.line_number == 1 && entry.byte_range == (3..8)
                )),
            "the original card (line 1's real \"alpha\" token) must survive a pointer move that \
             lands on a real, different token the card merely happens to be covering - it must \
             neither dismiss nor switch to describing \"beta\" instead"
        );
        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "and the real card must still be painting, not just its state surviving"
        );
    }

    #[gpui::test]
    fn the_full_real_pipeline_also_survives_hovering_a_covered_token(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn alpha() {}\nfn beta() {}\n").expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "hover");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let alpha_point = point_on_token(&app, cx, 1, 3..8);
        cx.simulate_mouse_move(alpha_point, None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(HOVER_TRIGGER_DELAY + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let card = cx.debug_bounds("hover-card").expect(
            "the real card must have painted after a real trigger delay and a real, resolved \
             hover round trip",
        );
        let beta_point = point_on_token(&app, cx, 2, 3..7);
        assert!(
            card.contains(&beta_point),
            "sanity check: the real card anchored under line 1 must genuinely cover line 2's own \
             real row underneath it for this test to prove anything - card {card:?}, beta's real \
             point {beta_point:?}"
        );

        cx.simulate_mouse_move(beta_point, None, gpui::Modifiers::none());
        cx.run_until_parked();
        // Advance past both real delays - the trigger delay (in case anything tried to arm a real
        // request for "beta") and the hide delay (in case anything tried to hide the card for
        // "alpha") - so this assertion reflects where things genuinely settle, not a snapshot
        // mid-debounce that happens to still look right.
        cx.background_executor.advance_clock(
            HOVER_TRIGGER_DELAY.max(HOVER_HIDE_DELAY) + std::time::Duration::from_millis(10),
        );
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app
                .hover
                .as_ref()
                .is_some_and(
                    |entry| entry.line_number == 1 && entry.byte_range == (3..8)
                )),
            "the original real card (line 1's real \"alpha\" token) must survive the real \
             pipeline's own debounce/hide machinery once the pointer rests on a real, different \
             token the card merely happens to be covering"
        );
        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "and the real card must still be painting, not just its state surviving"
        );
    }

    #[gpui::test]
    fn a_real_click_in_the_editor_dismisses_the_real_card(cx: &mut TestAppContext) {
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.txt", "alpha bravo\ncharlie delta\n");
        seed_ready_hover(&app, cx, file_path, 1, 0..5);
        assert!(cx.debug_bounds("hover-card").is_some());

        let (row_bounds, _) = row_layout(&app, cx, 2);
        cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.hover.is_none()),
            "a real click on another row must dismiss the real card - this is the exact \
             \"click and it stays open, can't close it\" report this issue is about"
        );
    }

    #[gpui::test]
    fn a_real_escape_keystroke_dismisses_the_real_card(cx: &mut TestAppContext) {
        let (_repo, file_path, app, cx) =
            open_file(cx, "sample.txt", "alpha bravo\ncharlie delta\n");
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        seed_ready_hover(&app, cx, file_path, 1, 0..5);
        assert!(cx.debug_bounds("hover-card").is_some());

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.hover.is_none()),
            "a real `Escape` keystroke must dismiss the real card"
        );
        assert!(cx.debug_bounds("hover-card").is_none());
    }
}

/// GitHub issue #186's real coverage for Surface C's Diagnostic popup: that it is a genuinely
/// floating, anchored popover which does not take layout space away from the code view (it used to
/// be a plain in-flow child at the bottom of the File view's own flex column - "the ugly thing that
/// opens at the bottom of the editor"), that it shows the diagnostic at the caret rather than every
/// diagnostic in the file, and that it honours the design's `lsp_popup ... one at a time` rule.
#[cfg(test)]
mod diagnostic_popover_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::{
        publish_and_wait, publish_at_and_wait, publish_with_source_and_wait, spawn_fake_server,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};

    const SOURCE: &str = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\nfn delta() {}\n";

    /// Two real render passes around a `run_until_parked`, preceded by a real `cx.notify()` so the
    /// window genuinely redraws (and `debug_bounds` genuinely re-measures) rather than answering
    /// from a stale frame - the same "render, park, render" shape every File-view test in this
    /// crate already uses.
    fn render_twice(app: &Entity<AdeApp>, cx: &mut VisualTestContext) {
        app.update(cx, |_app, cx| {
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
    }

    fn set_caret(app: &Entity<AdeApp>, cx: &mut VisualTestContext, line_number: usize) {
        app.update(cx, |app, cx| {
            app.code_cursor = Some(line_number);
            cx.notify();
        });
        render_twice(app, cx);
    }

    #[gpui::test]
    fn the_real_diagnostic_card_floats_over_the_code_without_reflowing_it(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        // A real, minimal LSP server installed *before* the first render, so `ensure_lsp_client`
        // finds a Ready entry for this key and never spawns a real rust-analyzer for the fixture.
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);
        set_caret(&app, cx, 1);

        let clean_code_list = cx
            .debug_bounds("file-view-code-list")
            .expect("the real code list should have painted real bounds");
        assert!(
            cx.debug_bounds("diagnostic-card").is_none(),
            "sanity check: a clean file must paint no card at all, not an empty one"
        );

        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_and_wait(
            &client,
            &uri.to_string(),
            "the trait bound `&str: Into<Column>` is not satisfied",
        );
        render_twice(&app, cx);

        app.read_with(cx, |app, _| {
            assert!(
                app.file_view_diagnostics.contains_key(&1),
                "sanity check: the real published diagnostic must have reached the real render \
                 path's own per-line index, got: {:?}",
                app.file_view_diagnostics.keys().collect::<Vec<_>>()
            );
        });

        let card = cx
            .debug_bounds("diagnostic-card")
            .expect("a real diagnostic at the caret must paint a real card");
        let code_list_with_card = cx
            .debug_bounds("file-view-code-list")
            .expect("the real code list should still be painting");

        assert_eq!(
            code_list_with_card, clean_code_list,
            "the real code view's own painted box must be completely unaffected by a real \
             diagnostic being present - the old in-flow card genuinely shrank it, which is the \
             whole bug"
        );
        assert!(
            card.top() < code_list_with_card.bottom() && card.bottom() > code_list_with_card.top(),
            "a real floating card overlaps the real code area it is anchored into (card {card:?}, \
             code list {code_list_with_card:?}) - a docked one would sit entirely below it"
        );

        let row_1 = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&1).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 1's real row should have painted real layout");
        assert!(
            (card.top() - row_1.bottom()).abs() < gpui::px(2.0),
            "the real card must paint directly under the real offending row (row bottom {:?}, \
             card top {:?})",
            row_1.bottom(),
            card.top()
        );
    }

    #[gpui::test]
    fn the_real_diagnostic_card_is_anchored_under_the_offending_span_not_the_row_edge(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);
        set_caret(&app, cx, 1);

        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_at_and_wait(&client, &uri.to_string(), "cannot find value `alpha`", 3, 8);
        render_twice(&app, cx);

        let card = cx
            .debug_bounds("diagnostic-card")
            .expect("a real diagnostic at the caret must paint a real card");
        let (row_1_bounds, row_1_shaped) = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&1).cloned())
            .expect("line 1's real row should have painted real layout");
        let expected_left = row_1_bounds.left() + row_1_shaped.x_for_index(3);

        assert!(
            (card.left() - expected_left).abs() < gpui::px(2.0),
            "the real card must be anchored under the real offending span's own start column \
             (expected left {expected_left:?}, got {:?})",
            card.left()
        );
        assert!(
            (card.left() - row_1_bounds.left()).abs() > gpui::px(5.0),
            "sanity check: this diagnostic starts well past column 0, so a card genuinely \
             anchored to it must land well past the row's own bare left edge too (row left {:?}, \
             card left {:?}) - equal here would mean the fix regressed back to ignoring the real \
             column entirely",
            row_1_bounds.left(),
            card.left()
        );
    }

    #[gpui::test]
    fn the_real_card_shows_the_caret_line_only_and_yields_to_the_other_lsp_popups(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);

        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_and_wait(&client, &uri.to_string(), "mismatched types");
        set_caret(&app, cx, 1);
        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "sanity check: the caret is on the real offending line, so the card must be up"
        );

        // The caret moves to a genuinely clean line - the real diagnostic is still in the file,
        // and the card must nonetheless go away.
        set_caret(&app, cx, 3);
        assert!(
            cx.debug_bounds("diagnostic-card").is_none(),
            "the card must describe the caret's own line, not dump every diagnostic in the file - \
             line 3 is clean, so nothing should paint even though line 1 is still broken"
        );

        set_caret(&app, cx, 1);
        assert!(cx.debug_bounds("diagnostic-card").is_some());
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path.clone(),
                line_number: 1,
                byte_range: 3..8,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 3,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn alpha()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        render_twice(&app, cx);
        assert!(
            cx.debug_bounds("hover-card").is_some(),
            "sanity check: the real Hover popup is the one that should be showing now"
        );
        assert!(
            cx.debug_bounds("diagnostic-card").is_none(),
            "`lsp_popup: None | Completions | Diagnostic | Hover | one at a time` - a real Hover \
             popup and a real Diagnostic popup must never paint simultaneously"
        );

        app.update(cx, |app, cx| {
            app.dismiss_hover();
            cx.notify();
        });
        render_twice(&app, cx);
        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "and the ambient Diagnostic card comes back once the requested popup it yielded to \
             has gone"
        );
    }

    #[gpui::test]
    fn moving_the_real_pointer_onto_the_diagnostic_card_does_not_hide_it(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);

        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_and_wait(&client, &uri.to_string(), "mismatched types");
        set_caret(&app, cx, 1);

        let card = cx.debug_bounds("diagnostic-card").expect(
            "sanity check: the caret is on the real offending line, so the card must be \
                      up before this test moves the pointer onto it",
        );
        let (row_2_bounds, row_2_shaped) = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&2).cloned())
            .expect("line 2's real row should have painted real layout");
        let beta_point = gpui::point(
            row_2_bounds.left() + (row_2_shaped.x_for_index(3) + row_2_shaped.x_for_index(7)) / 2.0,
            row_2_bounds.center().y,
        );
        assert!(
            card.contains(&beta_point),
            "sanity check: the real card anchored under line 1 must genuinely cover line 2's own \
             real \"beta\" token underneath it for this test to prove anything - card {card:?}, \
             beta's real point {beta_point:?}"
        );

        cx.simulate_mouse_move(beta_point, None, gpui::Modifiers::none());
        render_twice(&app, cx);
        // The real pointer resting on the diagnostic card's own chrome must survive not just the
        // instant it lands, but genuinely resting there - `Self::hover_over_token`'s own real
        // `HOVER_TRIGGER_DELAY` debounce means the covered "beta" token's own hover request would
        // only actually fire after this real delay elapses; without this, the test would pass
        // trivially regardless of whether the real bounds guard exists at all.
        cx.background_executor
            .advance_clock(HOVER_TRIGGER_DELAY + std::time::Duration::from_millis(10));
        render_twice(&app, cx);

        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "the Diagnostic card must survive a real pointer move that lands on a real, \
             different token it merely happens to be covering - it must not be hidden by that \
             covered token's own hover superseding it"
        );
    }

    #[gpui::test]
    fn hovering_directly_over_the_diagnostic_span_shows_the_diagnostic_not_an_empty_hover(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);

        // `"fn alpha() {}"` - `alpha` spans columns 3..8. The real diagnostic is published at
        // exactly that span, and the real hover is seeded to genuinely overlap it.
        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_at_and_wait(&client, &uri.to_string(), "cannot find value `alpha`", 3, 8);
        set_caret(&app, cx, 1);
        render_twice(&app, cx);
        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "sanity check: the caret is on the real offending line, so the card must be up \
             before any hover is involved"
        );

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 3..8,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 3,
                },
                status: HoverStatus::Ready(None),
            });
            cx.notify();
        });
        render_twice(&app, cx);

        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "a real diagnostic card must keep showing while the pointer hovers its own real \
             offending span, even though a real hover entry now exists for that exact position"
        );
        assert!(
            cx.debug_bounds("hover-card").is_none(),
            "a genuinely empty hover result (\"no symbol information here\") for the diagnostic's \
             own span must never paint over the real, useful diagnostic card"
        );
    }

    #[gpui::test]
    fn hovering_directly_over_the_diagnostic_span_shows_the_diagnostic_over_real_hover_content_too(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);

        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_at_and_wait(&client, &uri.to_string(), "cannot find value `alpha`", 3, 8);
        set_caret(&app, cx, 1);
        render_twice(&app, cx);

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 3..8,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 3,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn alpha()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        render_twice(&app, cx);

        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "the real diagnostic card must win over a real, non-empty hover result too, once \
             that hover genuinely overlaps the diagnostic's own span"
        );
        assert!(
            cx.debug_bounds("hover-card").is_none(),
            "and the real hover card must not paint alongside it, or the two would read as the \
             same information duplicated"
        );
    }

    #[gpui::test]
    fn hovering_a_diagnostic_span_shows_the_card_even_while_the_caret_is_elsewhere(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);

        // The real diagnostic lands on line 1 ("alpha", columns 3..8); the caret sits on the
        // genuinely clean line 3 the whole time.
        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_at_and_wait(&client, &uri.to_string(), "cannot find value `alpha`", 3, 8);
        set_caret(&app, cx, 3);
        assert!(
            cx.debug_bounds("diagnostic-card").is_none(),
            "sanity check: the caret's own line is clean and nothing is hovered, so no card \
             should be up yet"
        );

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 3..8,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 3,
                },
                status: HoverStatus::Ready(None),
            });
            cx.notify();
        });
        render_twice(&app, cx);

        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "the real pointer is resting directly on the diagnostic's own span - the card must \
             show even though the caret never moved off the clean line"
        );
    }

    /// Everything GitHub issue #204's copy button needs to exist, driven all the way from a real
    /// `publishDiagnostics` push: a real file open in a real window, the caret on the offending
    /// line, and the card genuinely painted. Returns the app and its window so each test below can
    /// click the real button rather than re-deriving this setup five times.
    fn open_with_real_diagnostic<'a>(
        cx: &'a mut TestAppContext,
        message: &str,
        source_and_code: Option<(&str, &str)>,
    ) -> (
        Entity<AdeApp>,
        &'a mut VisualTestContext,
        crate::code_surface::fixtures::TempRepo,
        std::sync::Arc<lsp_core::LspClient>,
        String,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        render_twice(&app, cx);

        let uri = lsp_core::LspClient::uri_for_path(&file_path)
            .expect("a real file:// uri")
            .to_string();
        match source_and_code {
            Some((source, code)) => {
                publish_with_source_and_wait(&client, &uri, message, source, code)
            }
            None => publish_and_wait(&client, &uri, message),
        }
        set_caret(&app, cx, 1);
        assert!(
            cx.debug_bounds("diagnostic-card").is_some(),
            "sanity check: the caret is on the real offending line, so the card must be up before \
             any of these tests touch its copy button"
        );
        (app, cx, repo, client, uri)
    }

    #[gpui::test]
    fn clicking_the_real_copy_button_puts_the_real_diagnostic_text_on_the_real_clipboard(
        cx: &mut TestAppContext,
    ) {
        let message = "the trait bound `&str: Into<Column>` is not satisfied";
        let (_app, cx, _repo, _client, _uri) =
            open_with_real_diagnostic(cx, message, Some(("rust-analyzer", "E0277")));

        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("stale".into()))
        });

        let button = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("a real diagnostic card must paint a real copy button");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let copied = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            copied.as_deref(),
            Some(format!("{message}\nrust-analyzer · E0277").as_str()),
            "a real click on the card's copy button must put exactly what the card shows - the \
             server's own message plus its `source · code` line - on the real system clipboard"
        );
    }

    #[gpui::test]
    fn a_diagnostic_with_no_source_or_code_still_gets_a_real_working_copy_button(
        cx: &mut TestAppContext,
    ) {
        let message = "mismatched types";
        let (_app, cx, _repo, _client, _uri) = open_with_real_diagnostic(cx, message, None);

        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("stale".into()))
        });

        let button = cx.debug_bounds("diagnostic-card-copy").expect(
            "a diagnostic with no source and no code must still paint a real copy button - it \
             used to paint no footer band at all",
        );
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let copied = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            copied.as_deref(),
            Some(message),
            "with no source and no code there is nothing to append - the clipboard must hold the \
             bare message, not a message with a dangling newline after it"
        );
    }

    #[gpui::test]
    fn the_real_copy_button_shows_a_real_confirmation_and_then_really_goes_back(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, _repo, _client, _uri) =
            open_with_real_diagnostic(cx, "mismatched types", Some(("rustc", "E0308")));

        let before = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("a real diagnostic card must paint a real copy button");
        cx.simulate_click(before.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        render_twice(&app, cx);

        let confirming = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("the copy button must still be painted while it confirms");
        assert!(
            confirming.size.width > before.size.width + px(4.0),
            "the confirmation must be genuinely visible on screen: `copied` is a wider word than \
             `copy`, so the real painted button must grow (was {:?}, now {:?})",
            before.size.width,
            confirming.size.width
        );

        // The confirmation is time-limited, on a real timer rather than left up until something
        // else happens to clear it.
        cx.executor()
            .advance_clock(DIAGNOSTIC_COPY_CONFIRM_DURATION + Duration::from_millis(50));
        cx.run_until_parked();
        render_twice(&app, cx);

        let after = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("the copy button must still be painted after the confirmation lapses");
        assert_eq!(
            after.size.width, before.size.width,
            "once the confirmation window has really elapsed the button must be back to exactly \
             its `copy` size, not left reading `copied` forever"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.diagnostic_copy_confirmed.is_none(),
                "and the armed confirmation state must genuinely have been cleared by its own \
                 timer, not merely stopped being rendered"
            );
        });
    }

    #[gpui::test]
    fn the_confirmation_does_not_leak_onto_a_different_diagnostic(cx: &mut TestAppContext) {
        let (app, cx, _repo, client, uri) =
            open_with_real_diagnostic(cx, "mismatched types", Some(("rustc", "E0308")));

        let unconfirmed = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("a real diagnostic card must paint a real copy button");
        cx.simulate_click(unconfirmed.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        render_twice(&app, cx);
        let confirming = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("the copy button must still be painted while it confirms");
        assert!(
            confirming.size.width > unconfirmed.size.width + px(4.0),
            "sanity check: the click must genuinely have armed a visible confirmation, or the \
             rest of this test proves nothing"
        );

        // A real, different diagnostic replaces the first one on the same real line. `publish_*`'s
        // own wait only proves *some* diagnostic has landed for this uri, and one already had - so
        // this polls the real render path's own index for the new message specifically.
        let replacement = "cannot find value `alpha` in this scope";
        publish_with_source_and_wait(&client, &uri, replacement, "rustc", "E0425");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            render_twice(&app, cx);
            let showing = app.read_with(cx, |app, _| {
                app.file_view_diagnostics
                    .get(&1)
                    .and_then(|line| line.first())
                    .map(|diagnostic| diagnostic.message.clone())
            });
            if showing.as_deref() == Some(replacement) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the replacement diagnostic never reached the real render path's own per-line \
                 index (showing {showing:?})"
            );
        }
        // The loop above breaks on *state*, and the frame painted during the `render_twice` that
        // observed it can predate the diagnostic actually landing - `debug_bounds` reads the last
        // painted frame, so without this the assertion below can measure the old card. Caught for
        // real: this test passed standalone and failed in the full suite, where the replacement
        // arrives later relative to the renders.
        render_twice(&app, cx);

        let other_card = cx
            .debug_bounds("diagnostic-card-copy")
            .expect("the replacement diagnostic's card must paint its own copy button");
        assert_eq!(
            other_card.size.width, unconfirmed.size.width,
            "a different diagnostic's card must read `copy`, not inherit the previous card's \
             `copied` - nothing of *this* diagnostic's text is on the clipboard"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.diagnostic_copy_confirmed.is_some(),
                "sanity check: the armed confirmation must still be live (its timer has not been \
                 advanced) - this test must be proving the payload comparison, not a lapsed timer"
            );
        });
    }
}

/// GitHub issue #186, bug 3: the dim end-of-line diagnostic message must not collide with the code
/// text it annotates. It used to live inside the row's `flex_none` code-run box - which, per that
/// box's own comment, "never shrinks" - with no wrap, truncation or ellipsis of its own, so on a
/// narrow pane a real `rustc` message simply overflowed and painted over the glyphs.
#[cfg(test)]
mod inline_diagnostic_message_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::{publish_and_wait, spawn_fake_server};
    use gpui::TestAppContext;

    /// Long enough that it cannot possibly fit beside the code on a narrow pane - which is exactly
    /// the real case that used to overflow. Real `rustc` messages are routinely this long.
    const LONG_MESSAGE: &str = "the trait bound `&str: Into<Column>` is not satisfied, and the \
                                trait `Into<Column>` is not implemented for `&str`, but it is \
                                implemented for `String`";

    #[gpui::test]
    fn a_real_long_inline_message_truncates_instead_of_overlapping_the_real_code_text(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn alpha() {}\nfn beta() {}\n").expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (repo.path().to_path_buf(), "rust-analyzer"),
                LspClientState::Ready(client.clone()),
            );
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        cx.simulate_resize(gpui::size(px(1100.0), px(400.0)));
        cx.run_until_parked();

        let uri = lsp_core::LspClient::uri_for_path(&file_path).expect("a real file:// uri");
        publish_and_wait(&client, &uri.to_string(), LONG_MESSAGE);
        for _ in 0..2 {
            app.update(cx, |app, cx| {
                cx.notify();
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
        }

        let code_text = cx
            .debug_bounds("file-view-code-text-1")
            .expect("line 1's real painted glyph box");
        let message = cx
            .debug_bounds("file-view-diagnostic-message-1")
            .expect("the real inline diagnostic message should have painted real bounds");
        // The row's own real text column - the box both the code glyphs and the message have to
        // stay inside.
        let text_row = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's real text column");

        assert!(
            message.left() >= code_text.right() - px(0.5),
            "the real message must begin at or after the real code glyphs end, never on top of \
             them (code text {code_text:?}, message {message:?})"
        );
        assert!(
            message.right() <= text_row.right() + px(0.5),
            "the real message must be cut off at the real text column's right edge rather than \
             overflowing past it (message {message:?}, text column {text_row:?})"
        );
        assert!(
            message.size.width < px(320.0) + px(0.5),
            "and it must never grow past its own real cap, however long the real message is - \
             got {:?} for a {} character message",
            message.size.width,
            LONG_MESSAGE.len()
        );
        assert!(
            code_text.right() <= text_row.right() + px(0.5),
            "the real code glyphs must still be the ones that keep their natural width - the \
             message is the shrinkable sibling, not the code (code text {code_text:?}, text \
             column {text_row:?})"
        );
    }
}

/// Regression coverage for the follow-up to GitHub issue #186 (design review): the hover
/// signature must be genuinely syntax-highlighted, and the Diagnostic card must genuinely wear
/// the design's own red-tinted chrome rather than the neutral Hover/Completions popover chrome.
#[cfg(test)]
mod hover_signature_and_diagnostic_chrome_tests {
    use super::*;

    #[test]
    fn a_real_rust_signature_is_split_into_real_distinctly_kinded_runs() {
        let lines = highlighted_signature_lines("pub fn where_eq(col: &str) -> Self", Some("rs"));
        assert_eq!(
            lines.len(),
            1,
            "a single-line signature must yield exactly one line group"
        );
        let runs = &lines[0];
        assert!(
            runs.len() > 1,
            "a real signature with a keyword, an identifier and a type must yield more than one \
             run, got: {runs:?}"
        );
        let pub_kind = runs
            .iter()
            .find(|(text, _)| text.as_ref() == "pub")
            .map(|(_, kind)| *kind)
            .expect("a real 'pub' token must be its own run");
        assert_eq!(
            pub_kind,
            code_view::HighlightKind::Keyword,
            "'pub' must be classified as a real keyword, the same way it is in the code editor \
             itself"
        );
        let fn_kind = runs
            .iter()
            .find(|(text, _)| text.as_ref() == "fn")
            .map(|(_, kind)| *kind)
            .expect("a real 'fn' token must be its own run");
        assert_eq!(fn_kind, code_view::HighlightKind::Keyword);
    }

    #[test]
    fn no_extension_still_returns_the_real_full_text_as_one_unhighlighted_run() {
        let lines = highlighted_signature_lines("let x: i32", None);
        assert_eq!(lines.len(), 1, "got: {lines:?}");
        let runs = &lines[0];
        assert_eq!(runs.len(), 1, "got: {runs:?}");
        assert_eq!(runs[0].0.as_ref(), "let x: i32");
        assert_eq!(runs[0].1, code_view::HighlightKind::Text);
    }

    #[test]
    fn a_genuinely_multi_line_signature_keeps_every_real_line_not_just_the_first() {
        let signature = "const x: Pick<{\n    a: string;\n    b: number;\n}, \"a\">";
        let lines = highlighted_signature_lines(signature, Some("ts"));
        assert_eq!(
            lines.len(),
            4,
            "a real 4-line signature must yield exactly 4 line groups, got: {lines:?}"
        );
        let joined_text = |line: &Vec<(gpui::SharedString, code_view::HighlightKind)>| {
            line.iter()
                .map(|(text, _)| text.as_ref())
                .collect::<String>()
        };
        assert!(
            joined_text(&lines[0]).contains("Pick"),
            "the first real line must still contain its own real text, got: {:?}",
            joined_text(&lines[0])
        );
        assert!(
            joined_text(&lines[1]).contains("a: string"),
            "the real second line - past the first newline the old bug truncated at - must \
             survive, got: {:?}",
            joined_text(&lines[1])
        );
        assert!(
            joined_text(&lines[2]).contains("b: number"),
            "got: {:?}",
            joined_text(&lines[2])
        );
        assert!(
            joined_text(&lines[3]).contains("\"a\""),
            "got: {:?}",
            joined_text(&lines[3])
        );
    }

    #[test]
    fn the_diagnostic_card_border_is_the_real_design_red_not_the_shared_popover_border() {
        assert_ne!(
            theme::border::DIAGNOSTIC_CARD,
            theme::border::POPOVER,
            "the Diagnostic card must not reuse Hover/Completions' neutral border"
        );
        assert_eq!(
            theme::border::DIAGNOSTIC_CARD.default,
            theme::hex_rgba(0x3a2224),
            "must match the mockup's own `border:1px solid #3a2224` exactly"
        );
    }

    #[test]
    fn the_diagnostic_row_bg_token_matches_the_real_design_card_background() {
        assert_eq!(
            theme::syntax::DIAGNOSTIC_ROW_BG.default,
            theme::hex_rgba(0x191416),
            "must match the mockup's own `background:#191416` exactly"
        );
    }

    #[test]
    fn the_lsp_popover_footer_token_matches_the_real_design_footer_band_background() {
        assert_eq!(
            theme::surface::LSP_POPOVER_FOOTER.default,
            theme::hex_rgba(0x141719),
            "must match the mockup's own `background:#141719` exactly"
        );
        assert_ne!(
            theme::surface::LSP_POPOVER_FOOTER,
            theme::surface::CARD_SUNK,
            "the Hover/Diagnostic footer band is a real, different tone from every other card's \
             footer, not a duplicate of CARD_SUNK"
        );
    }

    #[test]
    fn the_diagnostic_card_footer_border_is_its_own_real_shade_distinct_from_the_outer_border() {
        assert_eq!(
            theme::border::DIAGNOSTIC_CARD_FOOTER.default,
            theme::hex_rgba(0x2b2224),
            "must match the mockup's own `border-top:1px solid #2b2224` exactly"
        );
        assert_ne!(
            theme::border::DIAGNOSTIC_CARD_FOOTER,
            theme::border::DIAGNOSTIC_CARD,
            "the footer seam and the outer card outline are two real, different reds in the \
             mockup, not the same token reused twice"
        );
    }

    #[test]
    fn the_diagnostic_card_message_color_matches_the_real_design_headline_not_the_old_typo() {
        assert_eq!(
            theme::syntax::DIAGNOSTIC_CARD_MESSAGE.default,
            theme::hex_rgba(0xe3908b),
            "must match the mockup's own diagnostic headline `color:#e3908b` exactly"
        );
        assert_ne!(
            theme::syntax::DIAGNOSTIC_CARD_MESSAGE.default,
            theme::hex_rgba(0xf07f77),
            "must not still be the old, uncaught typo'd value"
        );
    }
}

/// Regression coverage for the hover card's real, mockup-shaped internal layout: three
/// independently-padded/bordered bands (signature header, doc body, module-path/`F12 definition`
/// footer), not one uniformly-padded flex column - see [`AdeApp::render_hover_card`]'s own docs
/// for why a purely color-focused first pass missed this.
#[cfg(test)]
mod hover_card_footer_layout_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_module_path_and_definition_chip_sit_at_opposite_ends_of_the_real_footer(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: Some("core".to_string()),
                    signature: "fn add_one(x: i32) -> i32".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();

        let card = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds");
        let definition_chip = cx
            .debug_bounds("hover-card-goto-definition")
            .expect("the real F12/definition chip should have painted real bounds");

        assert!(
            (card.right() - definition_chip.right()).abs() < px(15.0),
            "the F12/definition chip must sit near the real footer's right edge (card right \
             {:?}, chip right {:?}) - a bunched-together footer with no spacer would leave it \
             far short of the edge",
            card.right(),
            definition_chip.right()
        );
    }

    #[gpui::test]
    fn clicking_through_the_hover_card_never_also_reaches_the_file_view_row_behind_it(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn line_one() {}\nfn line_two() {}\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn line_one()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();

        let card = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds");
        let cursor_before = app.read_with(cx, |app, _| app.code_cursor);

        cx.simulate_click(card.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let cursor_after = app.read_with(cx, |app, _| app.code_cursor);
        assert_eq!(
            cursor_before, cursor_after,
            "a real click over the hover card must never also reach a real File view row \
             underneath it - the same real hitbox-blocking mechanism a scroll wheel event relies \
             on to avoid also scrolling the content behind the card"
        );
    }

    /// Mounts a real hover card carrying `doc` as its real doc body, exactly as
    /// `a_real_jsdoc_tag_in_the_hover_doc_body_paints_its_own_tag_run` does - shared so the link
    /// tests below differ only in the doc text they exercise.
    fn show_hover_with_doc<'a>(
        cx: &'a mut TestAppContext,
        doc: &str,
    ) -> (
        gpui::Entity<AdeApp>,
        &'a mut gpui::VisualTestContext,
        crate::code_surface::fixtures::TempRepo,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let doc = doc.to_string();
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn add_one(x: i32) -> i32".to_string(),
                    doc: Some(doc),
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
        (app, cx, repo)
    }

    #[gpui::test]
    fn a_real_markdown_link_in_a_hover_doc_body_really_opens(cx: &mut TestAppContext) {
        let (_app, cx, _repo) = show_hover_with_doc(
            cx,
            "Fetches a resource. See [MDN](https://developer.mozilla.org/fetch) for details.",
        );

        let link = cx
            .debug_bounds("doc-prose-link-0")
            .expect("a real Markdown link in the hover doc body must paint its own real link run");
        assert_eq!(
            cx.opened_url(),
            None,
            "sanity: nothing has been opened before the click"
        );

        cx.simulate_click(link.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://developer.mozilla.org/fetch"),
            "clicking a real link in a real hover card must really open its real destination"
        );
    }

    #[gpui::test]
    fn a_relative_link_in_a_hover_doc_body_is_not_painted_as_clickable(cx: &mut TestAppContext) {
        let (_app, cx, _repo) = show_hover_with_doc(cx, "See [the guide](./guide.md) for details.");

        assert!(
            cx.debug_bounds("doc-prose-link-0").is_none(),
            "a relative destination must not be advertised as a clickable link"
        );
    }

    #[gpui::test]
    fn a_real_jsdoc_tag_in_the_hover_doc_body_paints_its_own_tag_run(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn add_one(x: i32) -> i32".to_string(),
                    doc: Some("Adds one. See {@link add_two} for more.".to_string()),
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("doc-prose-tag-0").is_some(),
            "a real inline {{@link ...}} tag inside the hover doc body's own description must \
             still paint its own real `doc-prose-tag` run, even after block tags moved into \
             their own real sections"
        );
    }

    #[gpui::test]
    fn real_jsdoc_block_tags_in_the_hover_doc_body_paint_their_own_structured_sections(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn add_one(x: i32) -> i32".to_string(),
                    doc: Some(
                        "Adds one.\n\n@param x the input\n@returns x + 1\n@example\nadd_one(1)"
                            .to_string(),
                    ),
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("doc-param-row-0").is_some(),
            "a real @param tag must paint its own real parameter row"
        );
        assert!(
            cx.debug_bounds("doc-example-block").is_some(),
            "a real @example tag must paint its own real, syntax-highlighted code block"
        );
    }

    #[gpui::test]
    fn a_tall_signature_keeps_the_footer_pinned_and_shows_a_real_scrollbar(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        // A real, many-real-line signature - comfortably taller than `HOVER_CARD_MAX_HEIGHT`
        // (220px) at any plausible line height, the same real shape a wide TypeScript object/
        // union type pretty-prints as.
        let tall_signature = (0..30)
            .map(|index| format!("    field_{index}: string;"))
            .collect::<Vec<_>>()
            .join("\n");
        let signature = format!("const x: {{\n{tall_signature}\n}}");

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature,
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
        // `AdeApp::render_vertical_scrollbar` reads its geometry off the scroll handle's *last
        // painted* bounds/`max_offset` (see that method's own docs) - the very first frame after
        // the tall signature appears never has a scrollbar yet, by design. A second real frame
        // (matching `completion_popup`'s own identical `open_with_seeded_popup` settling step)
        // lets that settle before the scrollbar-visibility assertion below reads it.
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        let card = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds");
        let definition_chip = cx.debug_bounds("hover-card-goto-definition").expect(
            "the real F12/definition footer chip must still paint even though the real \
             signature above it is far taller than the card's own max height",
        );
        assert!(
            (card.bottom() - definition_chip.bottom()).abs() < px(10.0),
            "the footer must stay pinned near the card's own real bottom edge regardless of how \
             tall the content above it is (card bottom {:?}, chip bottom {:?}) - the old bug \
             pushed it below the card's own overflow_hidden() clip instead",
            card.bottom(),
            definition_chip.bottom()
        );
        assert!(
            cx.debug_bounds("hover-card-scrollbar").is_some(),
            "a real scrollbar must appear for the header/doc region once its own real content \
             genuinely overflows"
        );
    }

    #[gpui::test]
    fn a_short_signature_paints_no_scrollbar(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 0..2,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: Some("core".to_string()),
                    signature: "fn add_one(x: i32) -> i32".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("hover-card-goto-definition").is_some(),
            "sanity check: the real card must have painted"
        );
        assert!(
            cx.debug_bounds("hover-card-scrollbar").is_none(),
            "an ordinary short signature must never paint a real scrollbar - only genuinely \
             overflowing content should"
        );
    }
}
