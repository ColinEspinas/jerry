//! The language-server UI drawn *over* Surface C: the hover card, the diagnostics card,
//! the per-severity decoration colours, and go-to-definition. The client that produces
//! the responses these draw lives in `crate::lsp`; this module is only their UI.

use super::*;
// Only this module's own tests read `LspClientState` directly now - the render path goes through
// `AdeApp::lsp_connection_for_path`'s facade instead of raw client states (see
// `crate::lsp::client::LspConnection`), so a non-test import here would be genuinely unused.
#[cfg(test)]
use crate::lsp::client::LspClientState;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_keycap;
use std::time::Duration;

/// How long the pointer has to rest on one real token before a real `textDocument/hover` request
/// is sent for it (GitHub issue #186). Not a guessed number: `vendor/zed/crates/editor/src/
/// hover_popover.rs`'s own `hover_at` debounces on the user's `hover_popover_delay` setting, whose
/// real default in `vendor/zed/assets/settings/default.json` is `300` ms - the same value used
/// here, since this app has no per-user setting for it to read.
pub(crate) const HOVER_TRIGGER_DELAY: Duration = Duration::from_millis(300);

/// How long an already-visible [`AdeApp::hover`] card stays up after the pointer leaves its token
/// before it actually clears - the hide-side mirror of [`HOVER_TRIGGER_DELAY`], matching
/// `vendor/zed/crates/editor/src/hover_popover.rs`'s own separate `hover_popover_hiding_delay`
/// setting, whose real default (`vendor/zed/assets/settings/default.json`) is also `300`ms.
/// Without this, every real token boundary - or the plain whitespace between two words on the
/// same line - the pointer crosses while sweeping toward some other target synchronously cleared
/// an already-resolved, visible card: a real, reported flash on every sweep, not just on a
/// deliberate re-hover.
pub(crate) const HOVER_HIDE_DELAY: Duration = Duration::from_millis(300);

impl AdeApp {
    /// Real, pointer-driven hover trigger (GitHub issue #186): arms [`HOVER_TRIGGER_DELAY`] for
    /// `anchor`, and only once that has genuinely elapsed with the pointer still on the same token
    /// does [`Self::request_hover`] actually go out. Called from [`Self::track_hover_pointer`] for
    /// every real mouse-move that lands on a real token.
    ///
    /// Two real no-ops keep a pointer sweep from doing any work at all: the same token already
    /// showing a real [`Self::hover`] entry (which also cancels any [`HOVER_HIDE_DELAY`] a brief
    /// earlier detour off it might have armed - the pointer is back), and the same token already
    /// having an armed timer. Everything else is a genuinely different token: the *old* card, if
    /// any, is not cleared here - [`Self::schedule_hover_hide`] debounces that - while the new
    /// token's own show timer re-arms.
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
    ///
    /// No-ops the actual hide-arming half when there is no visible card to hide, or one is
    /// already armed (a pointer sweeping across several short-lived tokens in a row must not keep
    /// resetting the same countdown - see [`Self::hover_over_token`]'s "different anchor" path,
    /// the most common caller).
    ///
    /// The armed closure captures exactly which token it is hiding and only clears
    /// [`Self::hover`] if that entry is *still* the one showing when the timer fires - if a newer
    /// anchor's own request has since resolved and replaced it (a real race: this delay and
    /// [`HOVER_TRIGGER_DELAY`] can land within milliseconds of each other on a direct A-to-B
    /// sweep), this stale timer must not reach in and clear the *new*, unrelated card.
    fn schedule_hover_hide(&mut self, cx: &mut Context<Self>) {
        self.hover_pending = None;
        self._hover_debounce_task = None;
        let Some(showing) = self.hover.as_ref().map(|entry| {
            (entry.path.clone(), entry.line_number, entry.byte_range.clone())
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
                    (entry.path.clone(), entry.line_number, entry.byte_range.clone()) == showing
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
    ///
    /// Returns whether there was genuinely anything to dismiss, so callers that need to know
    /// (`Escape`, which falls through to other behaviour when the popup wasn't showing) can tell.
    /// Deliberately does **not** call `cx.notify()`, matching [`Self::dismiss_completions`]'s own
    /// established convention: every caller already has a surrounding state change to notify for.
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
    ///
    /// One window-level handler rather than a per-row one (the shape the drag-extend handler in
    /// `crate::code_surface::editing` uses) for a real reason: a per-row `.on_mouse_move` only
    /// fires while that row's own hitbox is the top-most one under the pointer (`vendor/zed/
    /// crates/gpui/src/elements/div.rs`'s `on_mouse_move` wraps every listener in
    /// `hitbox.is_hovered(window)`), so it can never observe the pointer *leaving* the code area
    /// for the sidebar, the terminal, or the title bar - exactly the dismissal case that has to
    /// work.
    ///
    /// The real dismissal rule, in order:
    /// 1. Pointer inside the real painted [`Self::hover_card_bounds`] - keep the card, so moving
    ///    onto it to press its own `F12 definition` footer doesn't close it first.
    /// 2. Pointer on a real, non-whitespace token of a real row - hover that token (debounced).
    /// 3. Anything else - debounced dismiss (see [`Self::schedule_hover_hide`]/
    ///    [`HOVER_HIDE_DELAY`]): the plain whitespace between two words on the same line is
    ///    "anything else" too, so an un-debounced dismiss here flashed the card off and back on
    ///    for every gap a sweep crossed, not just genuine token-to-token moves.
    ///
    /// A held mouse button means a real drag (selection extension, a pane resize, a tab drag), not
    /// a hover, so it dismisses immediately rather than debouncing.
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
    ///
    /// No longer called directly from a click (GitHub issue #186): the only production caller is
    /// [`Self::hover_over_token`]'s own debounce timer, once the pointer has genuinely rested on
    /// this token for [`HOVER_TRIGGER_DELAY`].
    ///
    /// No-ops if `(absolute_path, line_number, byte_range)` already matches the current
    /// [`Self::hover`] entry, so re-entering the same token doesn't redo a `rust-analyzer` round
    /// trip. Runs on `cx.background_executor()`, never inline: [`lsp_core::LspClient::request`]
    /// blocks the calling thread and must not block the GPUI foreground thread.
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
    ///
    /// Before GitHub issue #186 this was [`Self::hover`] and nothing else, which worked only
    /// because a click both moved the caret *and* opened the Hover card. Now that hover is a real
    /// pointer-rest gesture and a click dismisses it, `F12` needs a target that a keyboard-only
    /// user actually has: the caret's own position, derived from the live [`EditBuffer`]'s real
    /// cursor offset through the same [`hover_view::position_for_line_byte_offset`] the pointer
    /// path uses, so the two can't produce differently-encoded positions for the same place.
    ///
    /// Hover still wins when it is showing: the pointer is on that exact symbol, which is more
    /// specific than "wherever the caret was left".
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
                // Timeout, error, or no definition found - either way, no navigation.
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
            // `navigate_to_definition` needs `Window` access to move focus; `update_in` supplies
            // it by looking up the window this entity belongs to (see
            // vendor/zed/crates/gpui/src/app/async_context.rs `AsyncApp::with_window`), without
            // requiring this task to have been spawned via `cx.spawn_in`.
            let _ = this.update_in(cx, |this, window, cx| {
                this.navigate_to_definition(target_path, target_line, window, cx);
            });
        });
        self._goto_definition_tasks.push(task);
    }

    /// Navigates to a go-to-definition result. `absolute_target_path` may be under
    /// [`Self::file_tree_root`] or entirely outside it (e.g. another crate `rust-analyzer` sees);
    /// either way `Self::open_file_view`'s own `strip_prefix` handles it, since `PathBuf::join` with
    /// an already-absolute path just becomes that path.
    ///
    /// ## Avoiding a cursor-line race
    ///
    /// [`Self::open_file_view`] alone lands on the right file but not the right line: if the file
    /// wasn't already open, its background load unconditionally sets [`Self::code_cursor`] to 1
    /// once it completes, which would clobber a line set directly here before the load even
    /// starts. [`Self::pending_cursor_line`] is the one-shot instruction that survives the load
    /// instead; `Self::spawn_file_load`'s completion handler consumes it.
    pub(in crate::code_surface) fn navigate_to_definition(
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
            self.file_view_scroll_handle
                .scroll_to_item(one_based_line.saturating_sub(1), ScrollStrategy::Center);
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

impl AdeApp {
    /// Surface C's real, caret-anchored Diagnostic popover (GitHub issue #186).
    ///
    /// ## What changed, and why
    ///
    /// This used to be `render_diagnostics_card`: a plain in-flow child appended to the bottom of
    /// the File view's own flex column, listing *every* diagnostic anywhere in the open file. Both
    /// halves of that were wrong. Being an ordinary flex child (never `.absolute()`), it took real,
    /// permanent vertical space away from the code view - the design's `lsp_popup` state model
    /// (`design_handoff_jerry_ade/revision 3/README.md`) describes an anchored popup, not a docked
    /// panel. And the design's own Diagnostic state is one card for one offending span ("a card
    /// below: message, note, `rust-analyzer · E0277`"), not a whole-file dump.
    ///
    /// It now paints exactly the way [`Self::render_hover_card`] and
    /// [`crate::lsp::completion_popup::AdeApp::render_completions_popover`] already do: a real,
    /// absolutely-positioned top-level sibling in [`crate::root::AdeApp::render`], anchored off a
    /// real, already-painted `(Bounds, ShapedLine)` pair from [`Self::file_view_row_layout`],
    /// never nested inside the File view's own virtualized `uniform_list` (a popup anchored to one
    /// row must not be clipped by that row's own scroll container).
    ///
    /// ## Which diagnostic, and when
    ///
    /// Whichever diagnostics sit on the caret's own line - the caret is what "wherever the user
    /// is" means for a keyboard-driven surface, and it is also the only anchor that survives the
    /// pointer leaving the window. `None` (nothing painted at all) whenever there's no caret, the
    /// caret's line is clean, that row isn't currently painted, or - per the design's own
    /// `lsp_popup: None | Completions | Diagnostic | Hover | one at a time` - another LSP popup
    /// owns the slot.
    ///
    /// ## The one-at-a-time priority order
    ///
    /// Completions always wins outright - the user typed, and is waiting for that specific
    /// answer. Hover and Diagnostic are more nuanced: both are ambient in the general case, but
    /// hovering *directly over the offending span itself* is a real, deliberate ask for the
    /// diagnostic, not for whatever generic symbol info Hover would otherwise show there - so the
    /// diagnostic card wins that one specific case, not Hover, even though Hover is normally the
    /// more "requested" of the two. Two real, live-reproduced bugs motivated this: hovering
    /// directly over a real error span could show a genuinely empty `HoverStatus::Ready(None)`
    /// card ("no symbol information here") - real, honest, but useless right where the user is
    /// most likely looking for the diagnostic - and a real hover response that overlaps a real
    /// diagnostic could show mostly-duplicate text right next to (previously, *instead of*) the
    /// diagnostic's own message. Elsewhere on the same line - a different symbol the diagnostic
    /// doesn't cover - Hover keeps its normal priority, the same as it always did.
    pub(crate) fn render_diagnostic_card(&self) -> Option<gpui::AnyElement> {
        if self.completions.is_some() {
            return None;
        }
        let relative_path = self.active_editable_path()?;
        let (last_path, _) = self.file_view_last_layout_for.as_ref()?;
        if last_path != &relative_path {
            // The painted row layout belongs to some other file (e.g. a tab switch whose first
            // frame hasn't painted yet) - no real position to anchor to, so paint nothing rather
            // than guess one.
            return None;
        }
        // Two real triggers, not one: the caret's own line (keyboard nav, or a click that moved
        // it there) still shows the card exactly as before, and - since the follow-up to GitHub
        // issue #186 - the pointer resting directly on a diagnostic's own span shows it too, even
        // when the caret is sitting somewhere else entirely. A real hover showing something else
        // (an unrelated symbol) still wins the screen rather than stacking a caret-driven
        // diagnostic underneath it.
        let (line_number, diagnostic) = match (self.hover.as_ref(), self.hovered_diagnostic()) {
            (Some(hover), Some(diagnostic)) => (hover.line_number, diagnostic),
            (Some(_), None) => return None,
            (None, _) => {
                let line_number = self.code_cursor?;
                let diagnostics = self.file_view_diagnostics.get(&line_number)?;
                let worst = diagnostics_view::Severity::worst(diagnostics)?;
                let diagnostic = diagnostics
                    .iter()
                    .find(|candidate| candidate.severity == worst)?;
                (line_number, diagnostic)
            }
        };
        let (row_bounds, shaped) = self.file_view_row_layout.get(&line_number)?;

        let row_top = row_bounds.top();
        let row_bottom = row_bounds.bottom();
        // Flip above the offending row when there isn't real room below it - the same real
        // measurement `Self::render_hover_card` and `render_completions_popover` already make,
        // against the same real `Self::body_bounds`.
        let space_below = self.body_bounds.bottom() - row_bottom;
        let top = if space_below >= DIAGNOSTIC_CARD_MAX_HEIGHT {
            row_bottom
        } else {
            (row_top - DIAGNOSTIC_CARD_MAX_HEIGHT).max(self.body_bounds.top())
        };
        // Anchored under the real, offending span's own start column - the same real
        // `shaped.x_for_index(byte_range.start)` measurement `Self::render_hover_card` already
        // makes off the identical `(Bounds, ShapedLine)` pair, not the row's bare left edge. An
        // earlier version anchored at `row_bounds.left()` alone, which discarded `shaped` entirely
        // and put the card flush under the line-number gutter regardless of where in the line the
        // real error actually was - visibly wrong for anything past a short line, and the reason
        // this was still wrong after two design-fidelity passes that only ever touched the card's
        // own internal chrome, never its position.
        let anchor_x = row_bounds.left() + shaped.x_for_index(diagnostic.byte_range.start);

        Some(render_diagnostic_card_content(diagnostic, anchor_x, top))
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

/// The real Diagnostic popover's own content, split out of [`AdeApp::render_diagnostic_card`] for
/// exactly the reason [`render_hover_card_content`] is split out of [`AdeApp::render_hover_card`]:
/// the positioning math needs `&self` and this doesn't.
///
/// Follows the design mockup's own Diagnostic card structure (`design_handoff_jerry_ade/revision
/// 3/Jerry.dc.html`): a severity dot, the message's own first line, the rest of the message as a
/// dimmer note, and a `source · code` footer. The mockup's `quick fix: wrap in Column::from ⌘.`
/// chip is deliberately **not** drawn - this app has no `textDocument/codeAction` support at all,
/// so a chip there would be a button bound to nothing.
///
/// Chrome follows the mockup's own diagnostic card exactly, not [`AdeApp::render_hover_card`]'s
/// neutral popover chrome: `theme::syntax::DIAGNOSTIC_ROW_BG` (`#191416`) for the background and
/// `theme::border::DIAGNOSTIC_CARD` (`#3a2224`) for the border, both read directly off
/// `design_handoff_jerry_ade/revision 3/Jerry.dc.html`'s card (`background:#191416;border:1px
/// solid #3a2224`) - the red tint is how this card reads as *ambient/alarming* at a glance,
/// distinct from Hover/Completions' neutral `theme::surface::POPOVER`/`theme::border::POPOVER`.
/// `theme::radius::CARD_SM` and no shadow still match every other popover (the design's own
/// "Design tokens" section: "**one** [shadow] in the whole product - the completion popup").
fn render_diagnostic_card_content(
    diagnostic: &diagnostics_view::LineDiagnostic,
    anchor_x: Pixels,
    top: Pixels,
) -> gpui::AnyElement {
    let source_code = match (&diagnostic.source, &diagnostic.code) {
        (Some(source), Some(code)) => format!("{source} · {code}"),
        (Some(source), None) => source.clone(),
        (None, Some(code)) => code.clone(),
        (None, None) => String::new(),
    };
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

    let mut card = div()
        .id("diagnostic-card")
        // Lets a real test measure this real popover's own painted bounds (`debug_bounds` reads
        // this, not `.id(..)`) - a no-op outside test builds, matching `"hover-card"` and every
        // other `debug_selector` in this crate.
        .debug_selector(|| "diagnostic-card".to_string())
        .absolute()
        .left(anchor_x)
        .top(top)
        .flex_none()
        .flex()
        .flex_col()
        .w(DIAGNOSTIC_CARD_WIDTH)
        .max_h(DIAGNOSTIC_CARD_MAX_HEIGHT)
        .overflow_hidden()
        .rounded(theme::radius::CARD_SM)
        .bg(theme::syntax::DIAGNOSTIC_ROW_BG)
        .border_1()
        .border_color(theme::border::DIAGNOSTIC_CARD)
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

    if !source_code.is_empty() {
        // `background:#141719;border-top:1px solid #2b2224` in the mockup - its own footer band,
        // the same `LSP_POPOVER_FOOTER` background the Hover card's footer uses, but with this
        // card's own darker `DIAGNOSTIC_CARD_FOOTER` border rather than the neutral `border::
        // INNER` the pre-review version used (which also painted no background band at all).
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
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::text::FAINTER)
                        .child(source_code),
                ),
        );
    }

    card.into_any_element()
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
    ///
    /// The one real difference from [`Self::render_completions_popover`]: this reads
    /// [`Self::file_view_row_layout`] (every currently *visible* row, keyed by 1-based line
    /// number) rather than [`Self::file_view_last_layout`]/[`Self::file_view_last_bounds`] (the
    /// *caret's own* row alone). Since GitHub issue #186 the hovered line is wherever the
    /// **pointer** is, which is genuinely independent of the caret - reading the hovered row's own
    /// real layout entry is the only correct anchor, not the caret row's.
    ///
    /// `None` whenever there's nothing real to anchor to: no [`Self::hover`] entry, the entry
    /// belongs to a file that isn't the one currently on screen, the hovered row's own real
    /// layout isn't in [`Self::file_view_row_layout`] right now (e.g. scrolled out of view since
    /// the pointer landed) - the same honest "degrade to nothing rather than paint at a guessed
    /// position" discipline [`Self::render_completions_popover`] already established - or the
    /// hovered span genuinely overlaps a real diagnostic (see [`Self::render_diagnostic_card`]'s
    /// own docs for why that one real case flips the usual priority): a real, live-reproduced bug
    /// otherwise let a genuinely empty `HoverStatus::Ready(None)` ("no symbol information here")
    /// paint right over a real, useful diagnostic, and let a real hover response whose own text
    /// happened to overlap the diagnostic's paint alongside it, reading as duplicated.
    pub(crate) fn render_hover_card(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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
        // `vendor/zed` precedent this follows).
        let space_below = self.body_bounds.bottom() - row_bottom;
        let fits_below = space_below >= HOVER_CARD_MAX_HEIGHT;
        let top = if fits_below {
            row_bottom
        } else {
            (row_top - HOVER_CARD_MAX_HEIGHT).max(self.body_bounds.top())
        };

        // The active file's own extension - the same one `Self::request_hover` resolved a
        // highlighter for when the code line itself was painted - so the signature reads with
        // the exact same grammar/colors as the code around it, not a guessed or absent one.
        let extension = active_relative
            .extension()
            .and_then(|extension| extension.to_str());

        Some(render_hover_card_content(hover, extension, anchor_x, top, cx))
    }
}

/// The real Hover popover's own content - split out of [`AdeApp::render_hover_card`] purely so
/// the real positioning math above (which needs `&self`) stays visually separate from the real
/// per-status content build below (which doesn't) - mirrors
/// [`AdeApp::render_completions_popover`]'s own inline match, just factored into its own function
/// since this one has a real early-return position/anchor computation ahead of it.
fn render_hover_card_content(
    hover: &HoverEntry,
    extension: Option<&str>,
    anchor_x: Pixels,
    top: Pixels,
    cx: &mut Context<AdeApp>,
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
    let mut card = div()
        .id("hover-card")
        // Lets a real test measure this real popover's own painted bounds (`debug_bounds` reads
        // this, not `.id(..)` - see `hover_popover_position_tests`) - a no-op outside test
        // builds, matching every other `debug_selector` in this crate.
        .debug_selector(|| "hover-card".to_string())
        .child(bounds_probe)
        .absolute()
        .left(anchor_x)
        .top(top)
        .flex_none()
        .flex()
        .flex_col()
        .max_w(HOVER_CARD_MAX_WIDTH)
        .max_h(HOVER_CARD_MAX_HEIGHT)
        .overflow_hidden()
        .rounded(theme::radius::CARD_SM)
        .bg(theme::surface::POPOVER)
        .border_1()
        .border_color(theme::border::POPOVER);

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
            card = card.child(
                div()
                    // `.max_w(HOVER_CARD_MAX_WIDTH)` is load-bearing, not decoration. A GPUI
                    // element with no explicit width sizes itself to its own content (shrink-to-
                    // fit) rather than stretching to fill its parent the way a CSS block element
                    // would - the same real bug class `crate::code_surface::editing`'s own
                    // `text_row` docs describe for an identical shrink-to-fit failure - so a
                    // plain `.w_full()` here turned out *not* to be enough on its own: percentage
                    // width resolves against a parent's own *resolved* width, and the card above
                    // is itself only `max_w`-bounded (auto/shrink-to-fit otherwise), so `100%` of
                    // an unresolved auto width is still effectively unbounded. A real, hard
                    // `max_w` gives this header (and `render_hover_signature`'s own row below it)
                    // a genuinely definite upper bound to wrap `flex_wrap()`'s content within,
                    // regardless of what the card's own width resolves to. Without it, a
                    // signature longer than 430px never gets a real width to wrap within, so
                    // `render_hover_signature`'s own `flex_wrap()` never actually reflows - the
                    // row just grows past 430px and the card's `overflow_hidden()` silently
                    // hard-clips it instead (a real, live-reproduced TypeScript symptom: a long
                    // union/generic type painted cut off mid-glyph).
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
                card = card.child(
                    div()
                        .px(px(10.0))
                        .py(px(7.0))
                        .text_size(px(11.5))
                        .text_color(theme::text::DIM)
                        .child(doc.clone()),
                );
            }
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

/// The Hover popover's own signature line, syntax-highlighted like real code rather than painted
/// as flat text (`design_handoff_jerry_ade/revision 3/Jerry.dc.html`'s own hover card shows `pub
/// trait Into<T>: Sized` with real per-token colors - keyword purple, type gold - not one flat
/// heading color).
///
/// Runs `signature` through [`code_view::highlight_block`] as a single-line, standalone fragment,
/// the exact same "highlight a fragment on its own, not as part of a real open file" recipe the
/// Diff and Merge views already use for a hunk/conflict side (see that function's own docs), then
/// walks the resulting runs the same way [`crate::code_surface::file_view::render_file_view_line`]
/// walks a real code row's `line.runs`: one `div` per run, colored via
/// [`code_view::color_for_kind`]. `extension` comes from the active file
/// ([`AdeApp::render_hover_card`]), so a Rust hover highlights as Rust, a TypeScript hover as
/// TypeScript, and so on - never a guessed language.
///
/// `extension: None` (no active file extension resolved, which shouldn't happen in practice since
/// a hover only exists for a file that's open) still renders correctly: `highlight_block` returns
/// the text as one unhighlighted [`code_view::HighlightKind::Text`] run rather than nothing, so
/// the signature is never silently dropped.
fn render_hover_signature(signature: &str, extension: Option<&str>) -> gpui::AnyElement {
    let mut row = div()
        .flex()
        .flex_wrap()
        // See the header wrapper's own `.max_w(HOVER_CARD_MAX_WIDTH)` doc comment in
        // `render_hover_card_content` for why this needs a real, hard max-width (not just
        // `.w_full()`) to make `flex_wrap()` actually reflow. This row sits inside that header's
        // own `HOVER_CARD_HORIZONTAL_PADDING` on both sides, so its own bound is narrower by
        // twice that.
        .max_w(HOVER_CARD_MAX_WIDTH - HOVER_CARD_HORIZONTAL_PADDING - HOVER_CARD_HORIZONTAL_PADDING)
        .font(font(theme::font::MONO))
        .text_size(px(11.5));
    for (run_index, (run_text, kind)) in
        highlighted_signature_runs(signature, extension).into_iter().enumerate()
    {
        row = row.child(
            div()
                .id(("hover-signature-token", run_index))
                // Lets a real test measure this real token's own painted bounds (`debug_bounds`
                // reads this, not `.id(..)`) - a no-op outside test builds, matching every other
                // `debug_selector` in this crate.
                .debug_selector(move || format!("hover-signature-token-{run_index}"))
                .text_color(code_view::color_for_kind(kind))
                .child(run_text),
        );
    }
    row.into_any_element()
}

/// The pure half of [`render_hover_signature`] - just the `signature` -> colored-run computation,
/// split out so it's directly `#[test]`-able without a `gpui::Window`/`TestAppContext` (mirroring
/// how `code_view::highlight_block` itself is tested at the pure level, not by painting).
fn highlighted_signature_runs(
    signature: &str,
    extension: Option<&str>,
) -> Vec<(gpui::SharedString, code_view::HighlightKind)> {
    code_view::highlight_block(
        std::iter::once(signature),
        extension,
        code_view::HighlightOptions::default(),
    )
    .into_iter()
    .next()
    .map(|line| line.runs)
    .unwrap_or_default()
}

/// One File view code row: a 52px right-aligned line-number gutter, a 3px git-gutter marker
/// (tinted `theme::diff::GIT_GUTTER` for `is_changed`, transparent otherwise), and the
/// syntax-highlighted line content (`line.runs`, via `code_view::color_for_kind`). `is_current`
/// tints the whole row and brightens the gutter number.
///
/// Clicking a row sets `AdeApp::code_cursor` to `line_number`. There is no column tracking here
/// (not a fabricated `col 1`) - see `AdeApp::code_cursor`'s docs: per-character column tracking
/// is out of scope for this phase.
///
/// `diagnostics` (possibly empty, for this line - see `AdeApp::file_view_diagnostics`) drives
/// three per-row treatments: a row tint (`theme::syntax::DIAGNOSTIC_ROW_BG`, when `is_current`
/// isn't already tinting the row), a `.border_dashed()` underline under the offending span
/// (`crate::lsp::diagnostics::overlay_diagnostic_runs`; GPUI has no true dotted border, see
/// vendor/zed/crates/gpui/src/styled.rs's `border_dashed`), and a dim inline message from the
/// first diagnostic on the line (the full message is in `AdeApp::render_diagnostic_card`'s real
/// anchored popover, not repeated per-row - see `render_inline_diagnostic_message`). The design
/// only specifies an underline color for the error case; `Warning` reuses [`theme::term::WARN`], `Information`/`Hint` reuse
/// [`theme::text::DIM`]/[`theme::text::FAINT`] with `Hint` dimmer, matching the convention that
/// LSP hints are the least severe/most subtle diagnostic kind.
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
///
/// GitHub issue #186: this used to be an unconstrained `div` in a `flex_none` container that (per
/// that container's own comment) "never shrinks", so on a narrow pane a real `rustc` message
/// simply overflowed and painted straight over the code text. It is now a shrinkable sibling that
/// ellipsizes instead, using this codebase's own established `min_w_0()` + `max_w()` +
/// `truncate()` + `text_tooltip(..)` combination - the exact one
/// [`crate::code_surface::blame_view::render_inline_blame_span`] (the *other* thing placed at the
/// end of a code row) already uses, so the two shrink the same way and the untruncated text stays
/// reachable on the tooltip.
///
/// `max_w` is the same `320px` cap the blame span uses: a diagnostic message must never crowd out
/// the code it is about, however wide the pane gets.
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

    /// Regression guard: a Hint used to render pixel-identical to an Error (same underline,
    /// row tint, and inline message color) - all three dimensions must now differ.
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

    fn write_scratch_project(main_rs: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"app_hover_wiring_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir src");
        std::fs::write(dir.path().join("src").join("main.rs"), main_rs).expect("write main.rs");
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
            loop {
                cx.run_until_parked();
                let settled = app.read_with(cx, |app, _| {
                    matches!(
                        app.hover.as_ref().map(|entry| &entry.status),
                        Some(HoverStatus::Ready(_)) | Some(HoverStatus::Failed(_))
                    )
                });
                if settled {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "AdeApp::hover never left its real Loading state within the real deadline"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            let resolved = app.update(cx, |app, _| match &app.hover {
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
            std::thread::sleep(Duration::from_millis(300));
        }
    }

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

    /// A click-equivalent `request_hover` call against a running rust-analyzer (spawned through
    /// `render_file_view`'s `ensure_lsp_client` path) resolves to a `HoverRenderModel` whose
    /// signature and doc text match the fixture's documented function.
    #[gpui::test]
    fn a_real_click_resolves_to_a_real_hover_render_model(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

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
        let client_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            let ready = app.read_with(cx, |app, _| {
                matches!(
                    app.lsp_clients
                        .get(&(project.path().to_path_buf(), "rust-analyzer")),
                    Some(LspClientState::Ready(_))
                )
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < client_deadline,
                "the real rust-analyzer client never became Ready within 120s"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

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

    /// Proves the `F12` keybinding is wired end to end at the GPUI action-dispatch layer
    /// (`cx.bind_keys`, `AdeApp::render`'s `.on_action`, `handle_goto_definition_action`) by
    /// observing a distinguishing side effect: with no file open, `hover` is `None`, so
    /// `trigger_goto_definition` returns immediately without spawning anything - a harmless,
    /// deterministic no-op this test can assert didn't panic or spawn a stray background task.
    ///
    /// ## Why full navigation is proven a different way
    ///
    /// Mounting a File view via `render_center_pane` was found, while writing this test, to leave
    /// `TestAppContext::dispatch_action` unable to reach any `on_action` handler at all:
    /// `render_center_pane` stops rendering the active agent's terminal pane the instant a File
    /// view mounts, leaving `Window::focus` dangling on a `FocusId` the last frame no longer
    /// contains - the same bug class [`palette_focus_tests`]/[`settings_focus_tests`] describe,
    /// fixed for Surface C by [`AdeApp::code_focus_handle`] (see [`code_focus_tests`] for
    /// interactive coverage that `GotoDefinition` reaches its handler with a File view open).
    /// This test stays minimal (fresh window, no file opened); full navigation is proven by
    /// [`f12_action_navigates_to_the_real_definition_line`] calling `trigger_goto_definition`
    /// directly instead.
    #[gpui::test]
    fn f12_action_reaches_the_real_handler_on_a_fresh_window(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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

    /// `trigger_goto_definition` sends a `textDocument/definition` request using `hover`'s
    /// position, and the response navigates `code_cursor` to the function's definition line, not
    /// the call site the request was sent from. Calls `trigger_goto_definition` directly rather
    /// than `cx.dispatch_action(GotoDefinition)` - see this module's docs above.
    #[gpui::test]
    fn f12_action_navigates_to_the_real_definition_line(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        // See the hover test above's identical loop for why re-rendering, not just waiting,
        // matters.
        let client_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            let ready = app.read_with(cx, |app, _| {
                matches!(
                    app.lsp_clients
                        .get(&(project.path().to_path_buf(), "rust-analyzer")),
                    Some(LspClientState::Ready(_))
                )
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < client_deadline,
                "the real rust-analyzer client never became Ready within 120s"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

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

        // Retried on a timer rather than called once: a `textDocument/definition` response can
        // honestly be empty while rust-analyzer is still mid-index, and a real user would just
        // press F12 again.
        let definition_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.trigger_goto_definition(cx);
            });
            cx.run_until_parked();
            // The definition request runs on a background OS thread; GPUI's deterministic test
            // executor's `run_until_parked` only drains its own scheduled queue, which is empty
            // again the instant that background call is dispatched, so a wall-clock sleep is
            // needed before a second `run_until_parked` can observe the completion callback.
            std::thread::sleep(Duration::from_millis(300));
            cx.run_until_parked();
            // `fn add_one` is on line 4 (1-based), different from `CALL_SITE_LINE` (9), proving
            // this is real navigation, not a no-op that left the cursor where it was.
            let navigated = app.read_with(cx, |app, _| app.code_cursor == Some(4));
            if navigated {
                break;
            }
            assert!(
                Instant::now() < definition_deadline,
                "trigger_goto_definition never navigated AdeApp::code_cursor to the real \
                 definition line within 120s - last observed code_cursor: {:?}",
                app.read_with(cx, |app, _| app.code_cursor)
            );
        }
    }

    /// Real, end-to-end coverage for the Ctrl/Cmd+click go-to-definition affordance: a real
    /// simulated mouse click, with a real secondary modifier held, on the real painted call-site
    /// token, against a genuinely spawned rust-analyzer - not a call straight into
    /// `trigger_goto_definition` the way [`f12_action_navigates_to_the_real_definition_line`]
    /// tests the mechanism itself. This is what actually proves the click *routes* there.
    #[gpui::test]
    fn ctrl_click_on_a_real_token_navigates_to_its_real_definition(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        let client_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            let ready = app.read_with(cx, |app, _| {
                matches!(
                    app.lsp_clients
                        .get(&(project.path().to_path_buf(), "rust-analyzer")),
                    Some(LspClientState::Ready(_))
                )
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < client_deadline,
                "the real rust-analyzer client never became Ready within 120s"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

        let (row_bounds, shaped) = app
            .read_with(cx, |app, _| {
                app.file_view_row_layout.get(&CALL_SITE_LINE).cloned()
            })
            .expect("the call-site row must have real painted layout by now");
        let click_point = gpui::point(
            row_bounds.left() + shaped.x_for_index(CALL_SITE_BYTE_RANGE.start + 1),
            row_bounds.center().y,
        );
        // `Modifiers::secondary()` reads `control` on every platform this test actually runs on
        // (non-macOS - see that method's own docs) - a real, held Ctrl, not a stand-in for it.
        let ctrl_click = gpui::Modifiers {
            control: true,
            ..Default::default()
        };

        // Retried the same real way `f12_action_navigates_to_the_real_definition_line` retries
        // its own equivalent request: a real `textDocument/definition` response can honestly come
        // back empty while rust-analyzer is still mid-index, and a real user would just click
        // again. Re-clicking (not just re-waiting) is load-bearing here - a single stale response
        // means nothing will ever change without a fresh request.
        let definition_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            cx.simulate_click(click_point, ctrl_click);
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(300));
            cx.run_until_parked();
            // `fn add_one` is on line 4 (1-based), different from `CALL_SITE_LINE` (9) - real
            // navigation, not the click's own plain caret placement being mistaken for it.
            let navigated = app.read_with(cx, |app, _| app.code_cursor == Some(4));
            if navigated {
                break;
            }
            assert!(
                Instant::now() < definition_deadline,
                "a real Ctrl+click on the real call-site token never navigated \
                 AdeApp::code_cursor to the real definition line within 120s - last observed \
                 code_cursor: {:?}",
                app.read_with(cx, |app, _| app.code_cursor)
            );
        }
    }

    /// A plain click still just places the caret and dismisses `Self::hover` - it must *not*
    /// also fire `trigger_goto_definition`. No real LSP round trip needed: this is purely about
    /// which branch of the click handler ran, provable from the caret's own real position and
    /// `Self::hover`'s own state alone.
    #[gpui::test]
    fn a_plain_click_with_no_modifier_does_not_trigger_navigation(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn one() {}\nfn two() {}\n").expect("write sample.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// A real, answered hover with genuinely nothing to say (`HoverStatus::Ready(None)`) must
    /// paint no popup at all - not an empty "no symbol information here" card. The `Loading` ->
    /// `Ready(None)` transition genuinely happens (the loading card was up, then the real answer
    /// arrived empty), and the card must disappear rather than swap to a different, still-empty
    /// message.
    #[gpui::test]
    fn a_genuinely_empty_hover_result_paints_no_popup_at_all(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write sample.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// Regression coverage for a real, live-reproduced TypeScript symptom: a long, complex type
    /// signature painted cut off mid-glyph instead of wrapping inside the card. Proven the same
    /// way this module's other position tests prove real layout - real painted bounds, not a
    /// description of intent.
    ///
    /// Opens a real `.rs` file (not `.txt`): [`render_hover_signature`] only has real, separate
    /// flex items for `flex_wrap()` to wrap *between* when the signature is genuinely tokenized
    /// into multiple syntax-highlighted runs - an unhighlighted fallback (no real extension
    /// resolved) is exactly one run/one flex item, which `flex_wrap()` structurally cannot wrap
    /// within on its own (that would need the text itself to reflow, a different real GPUI
    /// behavior this fix doesn't touch). The real-world TS/Rust symptom this fixes always does
    /// have a real extension resolved, so this is the real shape of the actual bug, not an
    /// artificial best case.
    #[gpui::test]
    fn a_real_long_signature_wraps_inside_the_card_instead_of_being_clipped(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write sample.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The real, load-bearing proof that the popover is anchored to the real hovered row, not
    /// painted at a fixed position: seeding the exact same [`HoverEntry`] content for two
    /// different real lines must move the real painted popover to two different real positions,
    /// each one genuinely close to that line's own real painted row - a fixed-position popover
    /// (the pre-fix bug) would paint at the same spot regardless of which line was hovered.
    #[gpui::test]
    fn the_real_painted_hover_card_moves_with_the_real_hovered_row(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The real popover's horizontal anchor tracks the real hovered token's own column, not a
    /// fixed left offset either - hovering a token further into a line must shift the real
    /// painted card's own real left edge to the right by roughly the same real pixel distance.
    #[gpui::test]
    fn the_real_painted_hover_card_moves_horizontally_with_the_real_hovered_column(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.txt");
        std::fs::write(&file_path, "aaaa bbbb cccc dddd\n").expect("write sample.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // Hover the first token (byte 0..4, "aaaa").
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

        // Hover the last token (byte 15..19, "dddd") on the exact same line.
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
///
/// Nothing here is stubbed: two real Node processes, a real `npm install typescript` for the real
/// `--tsdk` this app resolves on its own, and real assertions on the actual diagnostic text and
/// hover markdown the real servers produce. It is genuinely slow for that reason, and kept in the
/// normal (non-`#[ignore]`) suite on purpose - this project has no separate slow-test lane.
///
/// The fixture carries **two** deliberately different real errors, because in Vue's hybrid mode
/// each server answers a genuinely different class of question (see `crate::language`'s own docs):
/// a template compile error only `vue-language-server` reports, and a TypeScript type error only
/// the companion reports. Asserting on both is what actually proves the merge is a real union of
/// two live contributors rather than one server's answer dressed up as two.
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
    fn write_scratch_vue_project() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\": {\"strict\": true, \"target\": \"ES2020\", \
             \"module\": \"ESNext\", \"moduleResolution\": \"Bundler\", \"jsx\": \"preserve\"}, \
             \"include\": [\"**/*.ts\", \"**/*.vue\"]}\n",
        )
        .expect("write tsconfig.json");
        std::fs::write(dir.path().join("App.vue"), FIXTURE_VUE).expect("write App.vue");
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
    fn wait_until(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        deadline: Instant,
        message: &str,
        predicate: impl Fn(&AdeApp) -> bool,
    ) {
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.background_executor
                .advance_clock(LSP_DIAGNOSTICS_POLL_INTERVAL + Duration::from_millis(10));
            cx.run_until_parked();
            if app.read_with(cx, |app, _| predicate(app)) {
                return;
            }
            assert!(Instant::now() < deadline, "{message}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn diagnostic_messages(app: &AdeApp) -> Vec<String> {
        app.file_view_diagnostics
            .values()
            .flatten()
            .map(|diagnostic| diagnostic.message.clone())
            .collect()
    }

    #[gpui::test]
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
        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

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
        wait_until(
            &app,
            cx,
            Instant::now() + Duration::from_secs(180),
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
        wait_until(
            &app,
            cx,
            Instant::now() + Duration::from_secs(180),
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
        wait_until(
            &app,
            cx,
            Instant::now() + Duration::from_secs(120),
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
        let hover_deadline = Instant::now() + Duration::from_secs(120);
        let hover_started = Instant::now();
        let model = loop {
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
            loop {
                cx.run_until_parked();
                let settled = app.read_with(cx, |app, _| {
                    matches!(
                        app.hover.as_ref().map(|entry| &entry.status),
                        Some(HoverStatus::Ready(_)) | Some(HoverStatus::Failed(_))
                    )
                });
                if settled {
                    break;
                }
                assert!(
                    Instant::now() < hover_deadline,
                    "AdeApp::hover never left its real Loading state within the real deadline"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            let resolved = app.update(cx, |app, _| match &app.hover {
                Some(HoverEntry {
                    status: HoverStatus::Ready(Some(model)),
                    ..
                }) => Some(model.clone()),
                _ => None,
            });
            if let Some(model) = resolved {
                break model;
            }
            assert!(
                Instant::now() < hover_deadline,
                "no real, non-empty hover ever came back for the genuine `bad` identifier in the \
                 real .vue script block - the companion fallback in LspConnection::request is \
                 what has to supply it, since the primary genuinely answers null there"
            );
            std::thread::sleep(Duration::from_millis(300));
        };
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
            Instant::now() + Duration::from_secs(120),
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
            Instant::now() + Duration::from_secs(120),
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
    fn retry_until_some<T>(deadline: Instant, message: &str, attempt: impl Fn() -> Option<T>) -> T {
        loop {
            if let Some(value) = attempt() {
                return value;
            }
            assert!(Instant::now() < deadline, "{message}");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

/// GitHub issue #186's real coverage for Surface C's Hover popup: that it is triggered by the real
/// pointer resting on a real token (not by a click, which is what it used to be), and that it
/// genuinely goes away again - by moving the pointer off the token, by clicking elsewhere in the
/// editor, and by `Escape`. Before this issue there was no dismissal path of any kind: an opened
/// card only ever went away by switching tab/file/worktree.
///
/// The dismissal tests seed [`AdeApp::hover`] directly (the same established pattern
/// [`hover_popover_position_tests`] already uses) against a plain `.txt` file, because dismissal
/// is genuinely independent of the `textDocument/hover` round trip. The *trigger* test uses a real
/// `.rs` file, because resolving a pointer position to a token is exactly what it is proving.
#[cfg(test)]
mod hover_pointer_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    fn open_file<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
        source: &str,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        Entity<AdeApp>,
        &'a mut VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join(name);
        std::fs::write(&file_path, source).expect("write fixture");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The load-bearing proof for bug 1's first half: a real mouse *move* onto a real token arms a
    /// real hover for exactly that token, and a real *click* on the very same pixel does not.
    /// Before this issue the two were the other way round - hover was wired to `.on_click` on
    /// every syntax run, with no mouse-position tracking anywhere in the app.
    #[gpui::test]
    fn a_real_mouse_move_onto_a_real_token_arms_a_real_hover_and_a_real_click_does_not(
        cx: &mut TestAppContext,
    ) {
        // `fn alpha() {}` - `alpha` spans bytes 3..8.
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

        // The real debounce genuinely elapses and genuinely consumes the anchor.
        cx.background_executor
            .advance_clock(HOVER_TRIGGER_DELAY + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.hover_pending.is_none()),
            "once the real trigger delay elapses the armed anchor must be consumed by a real \
             `request_hover`, not left armed forever"
        );

        // A real click on the same pixel arms nothing: hover is a pointer-rest gesture now.
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

    /// Bug 1's second half, dismissal route 1: moving the pointer off the anchor token closes the
    /// real painted card.
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

        // A real point well outside every painted code row - the pointer has left the editor.
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

    /// Direct regression coverage for the debounce itself: a pointer sweep that merely passes
    /// through a plain whitespace gap on its way to a different real token must not flash the
    /// still-visible card off in between - the real, reported bug this fix addresses. Before this,
    /// `Self::track_hover_pointer`'s "anything else" branch called `dismiss_hover_and_notify`
    /// synchronously, so even a single frame spent over the space between "alpha" and "bravo"
    /// cleared the card.
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

    /// The other side of the same rule: moving *onto* the card must not dismiss it, or its own
    /// `F12 definition` footer would be impossible to press.
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

    /// Dismissal route 2: a real click elsewhere in the editor.
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

    /// Dismissal route 3: a real `Escape` keystroke, through the real bound action rather than a
    /// direct handler call.
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
///
/// Driven through a **real** diagnostic: a genuinely spawned minimal LSP server
/// (`crate::lsp::client::lsp_connection_facade_tests`'s own `spawn_fake_server`, the same real
/// `LspClient::spawn` handshake every production server goes through) pushing a real
/// `textDocument/publishDiagnostics`. Seeding `AdeApp::file_view_diagnostics` directly would prove
/// nothing here - `render_file_view` genuinely recomputes that map from the live connection on
/// every repaint, so a seeded value never survives to the frame the card would paint in.
#[cfg(test)]
mod diagnostic_popover_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::{
        publish_and_wait, publish_at_and_wait, spawn_fake_server,
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

    /// The real regression proof for bug 2: with a real diagnostic present, the code view's own
    /// painted box is byte-for-byte the box it had without one, and the real card paints *over*
    /// that box rather than below it. The old in-flow card could not do either - being an ordinary
    /// `flex_none` child of the File view's flex column, it genuinely shortened the code list.
    #[gpui::test]
    fn the_real_diagnostic_card_floats_over_the_code_without_reflowing_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        // A real, minimal LSP server installed *before* the first render, so `ensure_lsp_client`
        // finds a Ready entry for this key and never spawns a real rust-analyzer for the fixture.
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // A real `publishDiagnostics` from the real server process, on line 0 (1-based line 1).
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

        // ...and it is genuinely anchored under the offending row, not painted at a fixed spot.
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

    /// Direct regression coverage for the card's real horizontal position: it must be anchored
    /// under the real, offending span's own start column, not flush under the row's bare left
    /// edge (i.e. under the line-number gutter). An earlier version of `AdeApp::
    /// render_diagnostic_card` discarded the row's own `ShapedLine` entirely and anchored at
    /// `row_bounds.left()` alone - a bug the sibling test above could never catch, since every
    /// diagnostic it publishes starts at column 0, where the two anchors are numerically identical
    /// by coincidence. This one publishes a diagnostic on `"alpha"` (columns 3..8 of `"fn alpha()
    /// {}"`), where they are not.
    #[gpui::test]
    fn the_real_diagnostic_card_is_anchored_under_the_offending_span_not_the_row_edge(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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
        // `"fn alpha() {}"` - `alpha` starts at byte/column 3, not 0.
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

    /// The other half of bug 2: the card follows the caret and shows *that* line's diagnostic
    /// only. The old card listed every diagnostic anywhere in the open file, permanently.
    #[gpui::test]
    fn the_real_card_shows_the_caret_line_only_and_yields_to_the_other_lsp_popups(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // Back on the offending line, a real Hover popup takes the one-at-a-time slot.
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

    /// Direct regression coverage for the real, live-reported bug: hovering *directly over the
    /// offending span itself* used to always show Hover's own card, even when that card had
    /// genuinely nothing to say (`HoverStatus::Ready(None)`, "no symbol information here") - a
    /// real, useless popup shown right where the user was almost certainly looking for the real
    /// diagnostic instead. The real diagnostic card must win this one specific case now.
    #[gpui::test]
    fn hovering_directly_over_the_diagnostic_span_shows_the_diagnostic_not_an_empty_hover(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The other half: even a real, *non-empty* hover result must still yield to the diagnostic
    /// card when it's genuinely describing the same span a diagnostic covers - real servers
    /// sometimes do have something to say about an erroring expression (e.g. its inferred type),
    /// and showing that instead of the diagnostic reads as the diagnostic's own message being
    /// silently replaced by unrelated-looking text, which is exactly the second real complaint
    /// this fix addresses.
    #[gpui::test]
    fn hovering_directly_over_the_diagnostic_span_shows_the_diagnostic_over_real_hover_content_too(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// Follow-up to GitHub issue #186: the Diagnostic card is no longer caret-only - resting the
    /// real pointer directly on a diagnostic's own span shows it too, even while the caret is
    /// sitting on a completely different, clean line. "Keep the click thing but just add the
    /// hover" - the caret/click trigger from the tests above stays exactly as it was; this is
    /// strictly a second, independent trigger.
    #[gpui::test]
    fn hovering_a_diagnostic_span_shows_the_card_even_while_the_caret_is_elsewhere(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, SOURCE).expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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
}

/// GitHub issue #186, bug 3: the dim end-of-line diagnostic message must not collide with the code
/// text it annotates. It used to live inside the row's `flex_none` code-run box - which, per that
/// box's own comment, "never shrinks" - with no wrap, truncation or ellipsis of its own, so on a
/// narrow pane a real `rustc` message simply overflowed and painted over the glyphs.
///
/// Driven through a real published diagnostic and a genuinely resized window, measuring the real
/// painted boxes of the code text and the message against each other - not by inspecting styles.
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
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn alpha() {}\nfn beta() {}\n").expect("write sample.rs");
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // A genuinely narrow window, so the real message genuinely cannot fit beside the code.
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

    /// A real, multi-token Rust signature must come back as more than one run, each with a real,
    /// non-`Text` kind for its keyword/type tokens - the pre-fix bug painted the whole signature
    /// as one flat-colored string, which this would see as a single run.
    #[test]
    fn a_real_rust_signature_is_split_into_real_distinctly_kinded_runs() {
        let runs = highlighted_signature_runs("pub fn where_eq(col: &str) -> Self", Some("rs"));
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

    /// No extension resolved (shouldn't happen in practice, but must degrade honestly rather than
    /// silently dropping the signature) still returns the full text, as a single unhighlighted run.
    #[test]
    fn no_extension_still_returns_the_real_full_text_as_one_unhighlighted_run() {
        let runs = highlighted_signature_runs("let x: i32", None);
        assert_eq!(runs.len(), 1, "got: {runs:?}");
        assert_eq!(runs[0].0.as_ref(), "let x: i32");
        assert_eq!(runs[0].1, code_view::HighlightKind::Text);
    }

    /// The Diagnostic popover's own chrome must be the design's real red-tinted card, not a copy
    /// of the neutral Hover/Completions popover chrome - the regression this guards against is
    /// exactly the one flagged in review: the card compiled and painted, but with the wrong
    /// colors, which a bounds-only test (`diagnostic_popover_tests`) can't catch.
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

    /// [`theme::syntax::DIAGNOSTIC_ROW_BG`] already carried the design's exact red-tinted
    /// background hex before this fix - this pins that the Diagnostic card actually *uses* it
    /// (see `render_diagnostic_card_content`), rather than the coincidentally-similar-looking
    /// [`theme::surface::POPOVER`] it used to paint with.
    #[test]
    fn the_diagnostic_row_bg_token_matches_the_real_design_card_background() {
        assert_eq!(
            theme::syntax::DIAGNOSTIC_ROW_BG.default,
            theme::hex_rgba(0x191416),
            "must match the mockup's own `background:#191416` exactly"
        );
    }

    /// Regression coverage for the second design-review pass: the mockup's hover/diagnostic
    /// footer bands paint their own real `#141719` background, distinct from both cards' own
    /// `#181c20`/`#191416` body backgrounds and from [`theme::surface::CARD_SUNK`] (`#131619`,
    /// the *different* footer tone every other card footer in the app uses).
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

    /// The Diagnostic card's own footer border (`#2b2224`) is a real, different shade from the
    /// card's outer border (`#3a2224`, [`theme::border::DIAGNOSTIC_CARD`]) - the mockup uses two
    /// distinct red tones on the same card, not one border colour reused for both seams.
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

    /// Regression coverage for a real typo caught in design review: this token's own doc comment
    /// already cited the mockup's real `#e3908b` headline colour, but the literal value assigned
    /// was `0xf07f77` - a different colour nobody had cross-checked against the doc comment right
    /// above it, let alone the real mockup file.
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

    /// The mockup's hover footer puts `module::path` at the far left and `F12 definition` at the
    /// far right, with a real `flex:1` spacer between them - not bunched together with a plain
    /// gap, which is what the pre-review layout did. Proven the same way this file's other
    /// popover-position tests prove real layout: real painted bounds, not a description of intent.
    #[gpui::test]
    fn the_module_path_and_definition_chip_sit_at_opposite_ends_of_the_real_footer(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add_one(x: i32) -> i32 { x + 1 }\n").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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
}
