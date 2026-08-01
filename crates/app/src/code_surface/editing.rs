//! Real text editing for Surface C's File view (Revision R8.5a) - the GPUI-facing half of
//! `crate::code_surface::edit_buffer::EditBuffer`'s pure logic. Three real pieces live here:
//!
//! 1. `impl EntityInputHandler for AdeApp` - the trait GPUI's real keyboard/IME platform layer
//!    calls into (see `vendor/zed/crates/gpui/src/input.rs`'s own docs, and this module's own
//!    `render_editable_file_view_line`, which is the one real call site that registers it via
//!    `Window::handle_input`). Every method resolves its target buffer through
//!    [`AdeApp::active_editable_path`]/`AdeApp::edit_buffers` and returns an honest empty/`None`
//!    result when there's no active File-view tab or buffer for it - an input-handler call
//!    arriving while, say, a terminal agent or the read-only Diff view has focus is a safe
//!    no-op, never a panic or a wrong-buffer edit (the Diff view's own dispatch path never even
//!    gets the `"file-editor"` key context these actions are scoped to - see
//!    `crate::code_surface::render::AdeApp::render_code_surface`'s own docs for exactly where that
//!    context lives and why - but the input-handler trait methods have no such context gate of
//!    their own, so this module's own checks are the real, structural guard).
//! 2. The `Editor*` keyboard action handlers (arrows, Backspace/Delete/Enter, selection,
//!    save) - bound with a `"file-editor"` `key_context` in `crate::default_key_bindings`,
//!    registered on `render_code_surface`'s outer, focused container (see that method's docs).
//! 3. Real per-row painting: [`render_editable_file_view_line`] renders each visible row's real
//!    visible glyphs from real, content-sized `div`s (matching the read-only File view's own
//!    proven text rendering - see that function's own docs for why, not a bare `gpui::canvas`),
//!    while a sibling `Window::text_system().shape_line` call (via an absolutely-positioned
//!    `gpui::canvas` overlay) provides the real pixel-accurate selection/cursor math (on every
//!    row the selection touches, not only the caret's own row) and registers the real
//!    `EntityInputHandler` wiring from the one row that actually contains the caret.
//!
//! ## A documented, honest gap: typing while the caret's own row is scrolled out of view
//!
//! `window.handle_input` is only ever registered from the caret's own row's paint (see point 3
//! above) - if that row isn't in the currently-*painted* range at all (the user scrolled away
//! without moving the caret - `Self::sync_cursor_and_scroll` keeps it in view after every real
//! cursor-moving action/edit, so this is a narrow window: a manual scroll with no intervening
//! action, then typing immediately), the platform window has no real input handler registered
//! that frame at all, and a literal character keystroke has nowhere real to go - not silently
//! misapplied to the wrong buffer/position, just genuinely dropped, with no visual feedback.
//! `Editor*` action-based input (arrows, Backspace/Delete/Enter, Save) is unaffected - those
//! dispatch as ordinary GPUI actions independent of the input-handler registration, and moving
//! the caret via any of them immediately re-scrolls it into view. A real, deliberate scope cut
//! for this phase rather than a fabricated fallback (e.g. guessing which row "should" get focus)
//! - flagged here rather than silently left undocumented.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    fill, point, prelude::*, size, Bounds, ClipboardItem, Context, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, PaintQuad, Pixels, Point, TextRun, UTF16Selection,
    UnderlineStyle, Window,
};

use crate::code_surface::blame;
use crate::code_surface::blame_view::render_inline_blame_span;
use crate::code_surface::code_view;
use crate::code_surface::edit_buffer::EditBuffer;
use crate::code_surface::indent;
use crate::code_surface::lsp_ui::{
    diagnostic_inline_message_color, diagnostic_row_bg, diagnostic_underline_color,
};
use crate::lsp::diagnostics as diagnostics_view;
use crate::lsp::hover as hover_view;
use crate::root::{
    AdeApp, EditorBackspace, EditorCollapseCursors, EditorCopy, EditorCut, EditorDedent,
    EditorDelete, EditorDown, EditorEnd, EditorEnter, EditorEscape, EditorHome, EditorIndent,
    EditorLeft, EditorPaste, EditorRight, EditorSave, EditorSaveAnyway, EditorSelectAll,
    EditorSelectAllOccurrences, EditorSelectDown, EditorSelectLeft, EditorSelectNextOccurrence,
    EditorSelectRight, EditorSelectUp, EditorSelectWordLeft, EditorSelectWordRight,
    EditorSkipOccurrence, EditorUp, EditorWordLeft, EditorWordRight, TextRedo, TextUndo,
};
use crate::settings::store as settings_store;
use crate::theme;

/// How long after the last keystroke [`AdeApp::schedule_rehighlight`] waits before running a real
/// `tree-sitter` re-highlight - see `crate::code_surface::edit_buffer`'s own "Re-highlighting cost" docs for the
/// real ~75ms measurement that rules out running this inline on every keystroke. Short enough
/// that a genuine pause in typing (not just the gap between two ordinary keystrokes) still feels
/// responsive; long enough that a fast typist's whole word lands before the first re-highlight for
/// it even starts.
const REHIGHLIGHT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Which real, live buffer is the current `EntityInputHandler`/`Editor*` action target - the File
/// view's [`AdeApp::edit_buffers`] entry, or Surface D's merge hand-edit
/// [`AdeApp::merge_edit`] - generalized (Revision R8.5c) from the File-view-only routing
/// [`AdeApp::active_editable_path`] originally provided. Never both at once:
/// `crate::work_surface::render::AdeApp::render_center_pane`'s own real visibility rule is
/// `open_change.is_some() && (open_diff_file_cache.is_some() || code_view ==
/// CodeView::File)` - Surface C (File/Diff) only actually renders when *that* holds. A real,
/// reachable state this must account for (documented at
/// `crate::code_surface::tabs::AdeApp::activate_file_tab`'s own doc comment): `open_change` can
/// be `Some` while Surface C is *not* shown at all - a tab can be "active" (its path still in
/// `open_change`) without a diff to show it (`open_diff_file_cache` is `None`) and `code_view`
/// left on `Diff`, in which case `render_center_pane` falls through to the agent/merge surface
/// with `open_change` still `Some` the whole time. [`AdeApp::active_edit_target`] mirrors this
/// exact predicate (not the weaker "`open_change.is_some()`" a first version of this method used,
/// which incorrectly treated that real, reachable state as "Surface C is showing" and silently
/// swallowed every keystroke meant for a genuinely on-screen merge hand-edit buffer) so it never
/// has to arbitrate between the two surfaces - at most one is ever genuinely on screen, and this
/// always agrees with `render_center_pane` about which one that is.
enum EditTarget {
    File(PathBuf),
    Merge,
}

impl AdeApp {
    /// The worktree-relative path an editing action should target - `Some` only while the File
    /// view (not the Diff view) is showing for an open tab. Deliberately File-view-only (unlike
    /// [`Self::active_edit_target`]): every call site of this specific method is a File-view-only
    /// concern that must never apply to the merge hand-edit surface at all - LSP diagnostics/
    /// completions/hover (`crate::lsp::completion_popup`, `crate::lsp::client`) - see
    /// `crate::merge::editing`'s own top docs for why no language-server relationship is
    /// ever established for a merge conflict buffer.
    pub(crate) fn active_editable_path(&self) -> Option<PathBuf> {
        if self.code_view == code_view::CodeView::File {
            self.open_change.clone()
        } else {
            None
        }
    }

    /// The generalized real edit target (Revision R8.5c) - see [`EditTarget`]'s own docs for the
    /// mutual-exclusivity guarantee this relies on. `Merge` only while the merge hand-edit slot
    /// exists *and* actually belongs to the currently active agent tab (a merge for a
    /// background agent tab the user has since switched away from is real, live state, but
    /// genuinely not on screen right now, so it must not receive keystrokes meant for whatever
    /// *is* focused).
    fn active_edit_target(&self) -> Option<EditTarget> {
        if let Some(path) = self.active_editable_path() {
            return Some(EditTarget::File(path));
        }
        if self.open_change.is_some()
            && (self.open_diff_file_cache.is_some() || self.code_view == code_view::CodeView::File)
        {
            // Surface C (File/Diff) is genuinely showing - see this method's own docs (and
            // `crate::work_surface::render::AdeApp::render_center_pane`'s real predicate,
            // which this mirrors exactly) for why `open_change.is_some()` alone is not enough to
            // conclude that.
            return None;
        }
        let edit = self.merge_edit.as_ref()?;
        let active_agent_id = self.agents.active().map(|agent| agent.id)?;
        if edit.agent_id != active_agent_id {
            return None;
        }
        Some(EditTarget::Merge)
    }

    pub(crate) fn active_edit_buffer(&self) -> Option<&EditBuffer> {
        match self.active_edit_target()? {
            EditTarget::File(path) => self.edit_buffers.get(&path),
            EditTarget::Merge => self.merge_edit.as_ref().map(|edit| &edit.buffer),
        }
    }

    fn active_edit_buffer_mut(&mut self) -> Option<&mut EditBuffer> {
        match self.active_edit_target()? {
            EditTarget::File(path) => self.edit_buffers.get_mut(&path),
            EditTarget::Merge => self.merge_edit.as_mut().map(|edit| &mut edit.buffer),
        }
    }

    /// Syncs the caret-line indicator and scrolls the owning list so the real caret stays in
    /// view - [`AdeApp::code_cursor`]/[`AdeApp::file_view_scroll_handle`] for the File view,
    /// [`AdeApp::merge_edit_scroll_handle`] (no cursor-line indicator - not read by anything) for
    /// the merge hand-edit view (Revision R8.5c generalization). Called after every real
    /// cursor-moving action and every real edit - both change where the caret is.
    pub(crate) fn sync_cursor_and_scroll(&mut self) {
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get(&path) else {
                    return;
                };
                let (line, _) = buffer.line_col_for_offset(buffer.cursor_offset());
                self.code_cursor = Some(line + 1);
                self.file_view_scroll_handle
                    .scroll_to_item(line, gpui::ScrollStrategy::Nearest);
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_ref() else {
                    return;
                };
                let (line, _) = edit.buffer.line_col_for_offset(edit.buffer.cursor_offset());
                self.merge_edit_scroll_handle
                    .scroll_to_item(line, gpui::ScrollStrategy::Nearest);
            }
            None => {}
        }
    }

    /// Debounces a real `tree-sitter` re-highlight for `path`'s buffer - see this module's own
    /// `REHIGHLIGHT_DEBOUNCE` docs and `crate::code_surface::edit_buffer`'s "Re-highlighting cost" docs for why.
    /// A single slot per path in [`AdeApp::_rehighlight_tasks`]: assigning a fresh task here drops
    /// (cancels) whatever earlier debounce timer for the same path was still waiting, so only the
    /// most recent keystroke's timer ever actually fires - real debounce, not a queue.
    pub(crate) fn schedule_rehighlight(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(buffer) = self.edit_buffers.get(&path) else {
            return;
        };
        if !buffer.highlight_dirty {
            return;
        }
        let Some(highlighter) = buffer.highlighter() else {
            // No real highlighter for this extension (matches the read-only File view's own
            // "no highlighter -> plain text" behavior) - the plain rebuild already done by
            // `EditBuffer::rebuild_plain` is the final, correct rendering; just clear the flag
            // rather than debouncing work that would never run.
            if let Some(buffer) = self.edit_buffers.get_mut(&path) {
                buffer.highlight_dirty = false;
            }
            return;
        };
        let content_snapshot = buffer.content.clone();
        let task = cx.spawn({
            let path = path.clone();
            let content_snapshot = content_snapshot.clone();
            async move |this, cx| {
                cx.background_executor().timer(REHIGHLIGHT_DEBOUNCE).await;
                let lines = cx
                    .background_executor()
                    .spawn({
                        let content_snapshot = content_snapshot.clone();
                        async move {
                            let spans = highlighter(&content_snapshot);
                            code_view::build_lines(&content_snapshot, &spans)
                        }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(buffer) = this.edit_buffers.get_mut(&path) {
                        if buffer.apply_highlight(&content_snapshot, lines) {
                            cx.notify();
                        }
                    }
                });
            }
        });
        self._rehighlight_tasks.insert(path, task);
    }

    pub(crate) fn handle_editor_backspace_action(
        &mut self,
        _: &EditorBackspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Delegates to `EditBuffer::backspace` itself (the exact same real no-op-at-start-of-
        // buffer/select-then-replace logic this handler used to reimplement inline) rather than
        // duplicating it - a real fix: the two copies had already drifted apart from
        // `EditBuffer::backspace`'s own dedicated unit tests, which is exactly the kind of
        // silent duplication this project's history (Revision R5.5) has already flagged once
        // for a different bug class.
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                buffer.backspace();
                self.schedule_rehighlight(path.clone(), cx);
                self.schedule_lsp_sync(path, cx);
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                edit.buffer.backspace();
            }
            None => return,
        }
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
        cx.notify();
    }

    pub(crate) fn handle_editor_delete_action(
        &mut self,
        _: &EditorDelete,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // See `Self::handle_editor_backspace_action`'s own docs - delegates to
        // `EditBuffer::delete_forward` for the same real reason.
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                buffer.delete_forward();
                self.schedule_rehighlight(path.clone(), cx);
                self.schedule_lsp_sync(path, cx);
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                edit.buffer.delete_forward();
            }
            None => return,
        }
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
        cx.notify();
    }

    pub(crate) fn handle_editor_enter_action(
        &mut self,
        _: &EditorEnter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Defense in depth, not the only guard: `crate::default_key_bindings` already scopes
        // this action's own binding to `"file-editor && !completions"` so a real `Enter`
        // keystroke never reaches this handler while the popup is *genuinely, actionably* open
        // (`CompletionsAccept`'s own `"file-editor && completions"` binding claims it instead) -
        // but this handler is also reachable by a direct call (as `crate::code_surface::editing::
        // editing_tests` already does for the analogous Diff-view guard), so it must
        // independently refuse to insert a newline in that same case, matching this project's
        // own established "guard the handler, not just the binding" discipline.
        //
        // `Self::completions_open_for_active_path` only returns `true` for a genuine
        // `CompletionsStatus::Ready` popup (see that method's own docs) - a real, live-reproduced
        // bug this project's audit caught (Revision R8.5b finding 1): while a completion request
        // is merely `Loading` (seeded on *every* completion-worthy keystroke, before the real
        // request even completes) or `Failed`, there is nothing real to accept/navigate, so
        // `Enter` must still reach here and insert a real newline, not be silently swallowed for
        // the whole real round-trip a completion request can take.
        if self.completions_open_for_active_path() {
            return;
        }
        self.replace_text_in_range(None, "\n", window, cx);
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
    }

    pub(crate) fn handle_editor_left_action(
        &mut self,
        _: &EditorLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::move_left);
    }

    pub(crate) fn handle_editor_right_action(
        &mut self,
        _: &EditorRight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::move_right);
    }

    pub(crate) fn handle_editor_up_action(
        &mut self,
        _: &EditorUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // See `Self::handle_editor_enter_action`'s own docs - the same real, independent guard,
        // since `CompletionsUp`'s binding is meant to move the popup's own selection instead
        // while it's open, not the real caret.
        if self.completions_open_for_active_path() {
            return;
        }
        self.move_active_buffer(cx, EditBuffer::move_up);
    }

    pub(crate) fn handle_editor_down_action(
        &mut self,
        _: &EditorDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // See `Self::handle_editor_enter_action`'s own docs.
        if self.completions_open_for_active_path() {
            return;
        }
        self.move_active_buffer(cx, EditBuffer::move_down);
    }

    pub(crate) fn handle_editor_select_left_action(
        &mut self,
        _: &EditorSelectLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_left);
    }

    pub(crate) fn handle_editor_select_right_action(
        &mut self,
        _: &EditorSelectRight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_right);
    }

    pub(crate) fn handle_editor_select_up_action(
        &mut self,
        _: &EditorSelectUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_up);
    }

    pub(crate) fn handle_editor_select_down_action(
        &mut self,
        _: &EditorSelectDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_down);
    }

    pub(crate) fn handle_editor_word_left_action(
        &mut self,
        _: &EditorWordLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::move_word_left);
    }

    pub(crate) fn handle_editor_word_right_action(
        &mut self,
        _: &EditorWordRight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::move_word_right);
    }

    pub(crate) fn handle_editor_select_word_left_action(
        &mut self,
        _: &EditorSelectWordLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_word_left);
    }

    pub(crate) fn handle_editor_select_word_right_action(
        &mut self,
        _: &EditorSelectWordRight,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_word_right);
    }

    pub(crate) fn handle_editor_home_action(
        &mut self,
        _: &EditorHome,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::move_home);
    }

    pub(crate) fn handle_editor_end_action(
        &mut self,
        _: &EditorEnd,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::move_end);
    }

    pub(crate) fn handle_editor_select_all_action(
        &mut self,
        _: &EditorSelectAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_buffer(cx, EditBuffer::select_all);
    }

    /// `Ctrl+D` (Revision R13, issue #28): `EditBuffer::select_word_or_add_next_occurrence`'s own
    /// docs for the real two-step VS Code behavior ("select word under caret" the first time,
    /// "add the next occurrence as a new cursor" every time after that) this single binding drives.
    pub(crate) fn handle_editor_select_next_occurrence_action(
        &mut self,
        _: &EditorSelectNextOccurrence,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_multi_cursor_action(cx, EditBuffer::select_word_or_add_next_occurrence);
    }

    /// `Ctrl+Shift+L` (Revision R13, issue #28) - `EditBuffer::select_all_occurrences`'s own docs.
    pub(crate) fn handle_editor_select_all_occurrences_action(
        &mut self,
        _: &EditorSelectAllOccurrences,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_multi_cursor_action(cx, EditBuffer::select_all_occurrences);
    }

    /// `Ctrl+K Ctrl+D` (Revision R13, issue #28) - `EditBuffer::skip_current_occurrence`'s own
    /// docs.
    pub(crate) fn handle_editor_skip_occurrence_action(
        &mut self,
        _: &EditorSkipOccurrence,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_multi_cursor_action(cx, EditBuffer::skip_current_occurrence);
    }

    /// `Esc` in the File view (Revision R13, issue #28; falls through to GitHub issue #26's
    /// accessibility escape hatch): tries `EditBuffer::collapse_to_single_cursor` first via
    /// `Self::apply_multi_cursor_action`'s own real change-reporting contract. Only one binding
    /// can genuinely own the File view's plain `Escape` at equal context depth (`crate::
    /// default_key_bindings`'s own docs on GPUI's real "later registration wins" precedence for
    /// same-depth contexts - confirmed against the pinned `gpui` dependency's own
    /// `key_dispatch.rs` test suite, not guessed), so rather than silently shadowing one of these
    /// two real, independently-designed behaviors, this handler composes both: a real multi-
    /// cursor collapse when one is active, or - the exact same no-op case `EditorCollapseCursors`
    /// was already documented as deliberately doing nothing for - [`Self::
    /// escape_focus_off_editor`]'s real accessibility fallback when there's nothing multi-cursor-
    /// related to do. `EditorEscape` stays a real, separately-bound action in its own right for
    /// `"merge-editor"` (see that binding's own docs), which never gets multi-cursor actions at
    /// all and so never faces this same collision.
    pub(crate) fn handle_editor_collapse_cursors_action(
        &mut self,
        _: &EditorCollapseCursors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.apply_multi_cursor_action(cx, EditBuffer::collapse_to_single_cursor) {
            return;
        }
        self.escape_focus_off_editor(window, cx);
    }

    /// Shared plumbing for every multi-cursor-only action above: applies `f` to the active buffer
    /// and, only if it reports a real change (`true`), dismisses completions (the caret may have
    /// moved/multiplied somewhere the popup's own anchor no longer describes - same real reasoning
    /// as `Self::move_active_buffer`'s own dismissal), notifies, and scrolls the (possibly new)
    /// primary caret into view. Returns that same `bool` so [`Self::
    /// handle_editor_collapse_cursors_action`] can tell a genuine no-op (e.g. `Ctrl+D` with no
    /// word under an empty caret, or `Esc` with only one cursor already active) apart from real
    /// work - callers that don't need to distinguish (every other action above) simply ignore it.
    fn apply_multi_cursor_action(
        &mut self,
        cx: &mut Context<Self>,
        f: fn(&mut EditBuffer) -> bool,
    ) -> bool {
        let Some(buffer) = self.active_edit_buffer_mut() else {
            return false;
        };
        if !f(buffer) {
            return false;
        }
        self.dismiss_completions();
        cx.notify();
        self.sync_cursor_and_scroll();
        true
    }

    /// `Tab` (GitHub issue #26) - real indentation, not a raw `\t` character: resolves the real
    /// indent unit (tabs vs. spaces, width) from [`Self::resolved_indent_settings_for_target`],
    /// then delegates to [`EditBuffer::indent_lines`] (see that method's own docs for the real
    /// no-selection-vs-selection behavior). File-target edits schedule a re-highlight/LSP-sync the
    /// same way every other real text-changing `Editor*` action does; the merge hand-edit target
    /// has neither (see `crate::merge::editing`'s own top docs for why).
    pub(crate) fn handle_editor_indent_action(
        &mut self,
        _: &EditorIndent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.resolved_indent_settings_for_target();
        let unit = indent::indent_unit(settings);
        let changed = match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                let changed = buffer.indent_lines(&unit);
                if changed {
                    self.schedule_rehighlight(path.clone(), cx);
                    self.schedule_lsp_sync(path, cx);
                }
                changed
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                edit.buffer.indent_lines(&unit)
            }
            None => return,
        };
        if !changed {
            return;
        }
        self.dismiss_completions();
        self.sync_cursor_and_scroll();
        cx.notify();
    }

    /// `Shift+Tab` (GitHub issue #26) - the mirror of [`Self::handle_editor_indent_action`], via
    /// [`EditBuffer::dedent_lines`]. A genuine no-op (every touched line already at column 0, or
    /// no real edit target at all) skips the re-highlight/sync/notify, matching every other
    /// `Editor*` handler's own "don't do real work for nothing" discipline.
    pub(crate) fn handle_editor_dedent_action(
        &mut self,
        _: &EditorDedent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.resolved_indent_settings_for_target();
        let changed = match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                let changed = buffer.dedent_lines(settings.tab_width);
                if changed {
                    self.schedule_rehighlight(path.clone(), cx);
                    self.schedule_lsp_sync(path, cx);
                }
                changed
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                edit.buffer.dedent_lines(settings.tab_width)
            }
            None => return,
        };
        if !changed {
            return;
        }
        self.dismiss_completions();
        self.sync_cursor_and_scroll();
        cx.notify();
    }

    /// The real [`indent::IndentSettings`] `Tab`/`Shift+Tab` should use right now - resolved from
    /// a real `.editorconfig` (via [`indent::indent_settings_for_path`]) for the File-view target,
    /// since only that target has a real on-disk path/worktree root to resolve one against; the
    /// merge hand-edit target (no real file path of its own - see `crate::merge::editing`'s own
    /// top docs) always falls back straight to the user's own [`crate::settings::store::
    /// EditorSettings`] default. `None`/no edit target also falls back to the same user default -
    /// harmless, since every real caller already returns before using it in that case.
    fn resolved_indent_settings_for_target(&self) -> indent::IndentSettings {
        let user_default = indent::IndentSettings {
            insert_spaces: self.settings.editor.insert_spaces,
            tab_width: self.settings.editor.tab_width,
        };
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => match self.edit_buffers.get(&path) {
                Some(buffer) => indent::indent_settings_for_path(
                    &buffer.path,
                    &self.file_tree_root,
                    user_default,
                ),
                None => user_default,
            },
            _ => user_default,
        }
    }

    /// `Escape` in the merge hand-edit view (GitHub issue #26's accessibility requirement) -
    /// `"merge-editor"` never gets multi-cursor actions bound (see `crate::code_surface::
    /// edit_buffer`'s own "Multi-cursor" docs for why), so this is a plain, standalone binding
    /// with no collision to resolve, unlike the File view's own `Escape` (see [`Self::
    /// handle_editor_collapse_cursors_action`]'s own docs for why *that* one has to compose two
    /// behaviors instead of just calling this directly).
    pub(crate) fn handle_editor_escape_action(
        &mut self,
        _: &EditorEscape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.escape_focus_off_editor(window, cx);
    }

    /// Moves keyboard focus off the editor entirely, onto [`AdeApp::filter_focus_handle`] (the
    /// rail's own filter field) - the same real fallback target [`crate::work_surface::render::
    /// AdeApp::close_agent`] already uses for "nothing agent/file-related is left to focus".
    /// Since `Tab`/`Shift+Tab` are now real indent/dedent actions while the editor has focus
    /// (rather than falling through to GPUI's ordinary focus-cycling), a keyboard-only user needs
    /// some other way to leave the editor and keep tabbing through the rest of the UI - this is
    /// that "escape hatch": once focus has genuinely moved elsewhere, an ordinary (now-unbound-
    /// here) `Tab` press resumes GPUI's normal focus-cycling immediately, no special two-key state
    /// machine needed. Shared by both real `Escape` paths that can reach it - [`Self::
    /// handle_editor_escape_action`] (`"merge-editor"`) directly, and [`Self::
    /// handle_editor_collapse_cursors_action`] (`"file-editor && !completions"`) as its own
    /// fallback once a real multi-cursor collapse has nothing to do.
    fn escape_focus_off_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.filter_focus_handle, cx);
        cx.notify();
    }

    /// Shared plumbing for every cursor-movement-only action (no text change, so no re-highlight
    /// to schedule): applies `f` to the active buffer, notifies, and scrolls the new caret
    /// position into view.
    fn move_active_buffer(&mut self, cx: &mut Context<Self>, f: fn(&mut EditBuffer)) {
        let Some(buffer) = self.active_edit_buffer_mut() else {
            return;
        };
        f(buffer);
        // A real caret move away from wherever the popup was anchored invalidates it - real
        // editors close completions the moment the caret leaves the word being completed, rather
        // than leaving a popup up that no longer describes the real insertion point Tab/Enter
        // would act on.
        self.dismiss_completions();
        cx.notify();
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
    }

    pub(crate) fn handle_editor_copy_action(
        &mut self,
        _: &EditorCopy,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer) = self.active_edit_buffer() else {
            return;
        };
        if buffer.selected_range.is_empty() {
            return;
        }
        if let Some(text) = buffer.content.get(buffer.selected_range.clone()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    pub(crate) fn handle_editor_cut_action(
        &mut self,
        _: &EditorCut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer) = self.active_edit_buffer() else {
            return;
        };
        if buffer.selected_range.is_empty() {
            return;
        }
        let Some(text) = buffer
            .content
            .get(buffer.selected_range.clone())
            .map(|text| text.to_string())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        // Same real group-boundary reasoning as `Self::handle_editor_paste_action` - a cut is a
        // discrete, deliberate action, not part of a backspace run on either side of it.
        self.seal_active_edit_history();
        self.replace_text_in_range(None, "", window, cx);
        self.seal_active_edit_history();
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
    }

    pub(crate) fn handle_editor_paste_action(
        &mut self,
        _: &EditorPaste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        // A paste is one of GitHub issue #17's four named undo-group boundaries, and it can't be
        // inferred from the splice itself (an ordinary typed character reaches the same
        // `EditBuffer::replace_range`). Sealing on both sides makes it its own step in both
        // directions: whatever was being typed before doesn't absorb it, and whatever is typed
        // after doesn't either.
        self.seal_active_edit_history();
        self.replace_text_in_range(None, &text, window, cx);
        self.seal_active_edit_history();
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
    }

    /// Closes the active buffer's current undo group - the caller-driven half of
    /// `crate::text_history`'s coalescing policy. See
    /// `crate::code_surface::edit_buffer::EditBuffer::seal_history`'s own docs.
    pub(crate) fn seal_active_edit_history(&mut self) {
        if let Some(buffer) = self.active_edit_buffer_mut() {
            buffer.seal_history();
        }
    }

    /// `TextUndo` for the code surface and the merge hand-edit surface (GitHub issue #17). Bound
    /// to `secondary-z` scoped `Some("text-input")`, registered on the exact focused node that
    /// carries that tag - see `crate::default_key_bindings`' own docs for why the routing is
    /// structural (per-node `on_action`) rather than a state lookup.
    pub(crate) fn handle_text_undo_action(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_text_undo(cx);
    }

    pub(crate) fn handle_text_redo_action(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_text_redo(cx);
    }

    /// The keystroke-independent entry points, so the title bar's Edit menu drives the exact same
    /// real code path the `secondary-z` binding does rather than a second implementation - the
    /// `perform_*`/`handle_*_action` split lets both call sites share one implementation.
    pub(crate) fn perform_text_undo(&mut self, cx: &mut Context<Self>) {
        self.step_edit_history(cx, true);
    }

    pub(crate) fn perform_text_redo(&mut self, cx: &mut Context<Self>) {
        self.step_edit_history(cx, false);
    }

    /// Shared plumbing for [`Self::handle_text_undo_action`]/[`Self::handle_text_redo_action`]:
    /// steps the active buffer's own history and then runs the exact same post-edit bookkeeping
    /// every ordinary keystroke already runs (re-highlight debounce, language-server sync, caret
    /// scroll-into-view). An undo genuinely changes the buffer's text, so skipping any of those
    /// would leave real, visible staleness - stale syntax colors, a language server answering
    /// about content that no longer exists.
    fn step_edit_history(&mut self, cx: &mut Context<Self>, undo: bool) {
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                let changed = if undo { buffer.undo() } else { buffer.redo() };
                if !changed {
                    return;
                }
                // A caret that jumped somewhere else makes whatever the popup was anchored to
                // meaningless - the same reasoning `Self::move_active_buffer` already applies.
                self.dismiss_completions();
                self.schedule_rehighlight(path.clone(), cx);
                self.schedule_lsp_sync(path, cx);
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                let changed = if undo {
                    edit.buffer.undo()
                } else {
                    edit.buffer.redo()
                };
                if !changed {
                    return;
                }
            }
            None => return,
        }
        self.sync_cursor_and_scroll();
        cx.notify();
    }

    /// Generalized (Revision R8.5c): routes to [`Self::save_active_file`] (the File view's own
    /// freshness-checked save) or [`Self::save_merge_edit`] (the merge hand-edit editor's own
    /// pipeline - `crate::merge::flow`'s own docs), whichever [`Self::active_edit_target`]
    /// names. `"merge-editor"`'s own key bindings (`crate::default_key_bindings`) never bind
    /// [`EditorSaveAnyway`] at all - there is no external-change-conflict concept for a merge
    /// hand-edit buffer (see [`Self::save_merge_edit`]'s own docs), so
    /// [`Self::handle_editor_save_anyway_action`] deliberately stays File-view-only, unchanged.
    pub(crate) fn handle_editor_save_action(
        &mut self,
        _: &EditorSave,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_edit_target() {
            Some(EditTarget::File(_)) => self.save_active_file(cx),
            Some(EditTarget::Merge) => self.save_merge_edit(cx),
            None => {}
        }
    }

    pub(in crate::code_surface) fn handle_editor_save_anyway_action(
        &mut self,
        _: &EditorSaveAnyway,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.force_save_active_file(cx);
    }

    /// The real explicit save (`secondary-s`, scoped to `"file-editor"`) - see this module's own
    /// docs and `AdeApp::file_external_conflict`'s own docs for the real conflict this refuses to
    /// silently paper over.
    ///
    /// The freshness check here is authoritative and synchronous: a single, cheap
    /// `std::fs::metadata` stat call (mirroring `crate::code_surface::file_view::AdeApp::
    /// render_file_view`'s own established precedent for doing this inline rather than off the
    /// foreground thread) compared against [`EditBuffer::saved_mtime`]/[`EditBuffer::saved_len`],
    /// stronger than relying only on the render-time, throttled [`AdeApp::file_external_conflict`]
    /// flag (which can be up to `FILE_FRESHNESS_CHECK_INTERVAL` stale), though that flag is also
    /// updated here for the File view's own banner to read.
    pub(crate) fn save_active_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_editable_path() else {
            return;
        };
        let Some(buffer) = self.edit_buffers.get(&path) else {
            return;
        };

        let metadata = std::fs::metadata(&buffer.path).ok();
        // A genuinely brand-new, never-loaded-or-saved buffer (`crate::root::AdeApp::
        // create_new_file`) has no real on-disk metadata on *either* side: `saved_mtime` was
        // never seeded from a real load (there was nothing to load yet), and the path itself
        // doesn't exist on disk yet either. That's the ordinary, expected shape of a file's
        // first-ever save, not an external-change conflict - a naive version of the checks below
        // would misread it as one (`saved_mtime: None` can never equal a real `Some(mtime)`) and
        // permanently refuse to create the file at all, without a real `EditorSaveAnyway` making
        // sense for it either (there's nothing "external" to override - see that action's own
        // docs). It also needs to skip the plain dirty-buffer check just below: an empty new
        // file's `content`/`saved_content` are both `""`, so `is_dirty()` alone would say "no
        // edits to save" even though the file genuinely doesn't exist on disk yet.
        let is_new_never_saved = buffer.saved_mtime.is_none() && metadata.is_none();

        if !is_new_never_saved && !buffer.is_dirty() {
            return;
        }

        let unchanged_since_load = is_new_never_saved
            || match &metadata {
                Some(metadata) => {
                    metadata.modified().ok() == buffer.saved_mtime
                        && metadata.len() == buffer.saved_len
                }
                // The file having vanished entirely (deleted externally) is itself a real
                // external change - refuse the same way a real mtime/len mismatch would, rather
                // than silently recreating it as if nothing happened.
                None => false,
            };
        if !unchanged_since_load {
            self.file_external_conflict.insert(path.clone());
            self.file_save_error = Some((
                path,
                "not saved: the file changed on disk since it was opened - press \
                 secondary-shift-s to overwrite the external change with your edits anyway"
                    .to_string(),
            ));
            cx.notify();
            return;
        }

        self.enqueue_save(path, cx);
    }

    /// The real, explicit, opt-in escape hatch for a real [`AdeApp::file_external_conflict`]
    /// (`secondary-shift-s`) - see that field's own docs for the real, permanent-deadlock bug
    /// this fixes: once a conflict is flagged, nothing but a *successful* save ever clears it,
    /// and [`Self::save_active_file`]'s own freshness gate can never pass again after any real
    /// external touch to the file (even reverting it back to byte-identical content still
    /// changes its real mtime) - so without a real, deliberate way to override it, the file
    /// would stay unsavable for the rest of the agent. This skips the *freshness* gate
    /// entirely and unconditionally overwrites the file with the buffer's current content - a
    /// real, user-initiated action, never automatic (matches this phase's own "no auto-merge"
    /// scope: this is a real, explicit "keep mine" choice, not a silent one) - reusing the exact
    /// same [`Self::enqueue_save`]/[`Self::spawn_file_save_loop`] dispatch [`Self::save_active_file`]
    /// itself uses once its own gate passes, so there is still only ever one real write path.
    ///
    /// Still real-guarded by [`crate::code_surface::edit_buffer::EditBuffer::is_dirty`], though (finding 7's own
    /// low-priority fix): without it, triggering this real "save anyway" override on an already-
    /// clean buffer (e.g. a stale keybinding fired after an unrelated save already succeeded)
    /// would perform a real, genuinely unnecessary disk write and bump the file's real mtime for
    /// no reason - a needless write, not a needless *safety check* (the freshness gate itself is
    /// still deliberately skipped, per this method's whole point).
    pub(in crate::code_surface) fn force_save_active_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_editable_path() else {
            return;
        };
        let Some(buffer) = self.edit_buffers.get(&path) else {
            return;
        };
        if !buffer.is_dirty() {
            return;
        }
        self.enqueue_save(path, cx);
    }

    /// Shared dispatch for both [`Self::save_active_file`] (after its own gate passes) and
    /// [`Self::force_save_active_file`] (which skips that gate) - the real serial-writer-loop
    /// spawn-or-join logic, factored out so there's exactly one real place a save gets enqueued.
    fn enqueue_save(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file_save_pending.insert(path.clone());
        if self.file_save_running.contains(&path) {
            // The loop below is already alive and always re-checks `file_save_pending` before
            // writing or stopping - it will pick this edit up on its own next iteration. This is
            // the exact same serial-writer-loop discipline `AdeApp::persist_settings` already
            // established for settings (see that method's own docs) - a second, independent write
            // task for the same path here would risk the same "an older write lands after a
            // newer one" race Revision R5.5 fixed once already.
            return;
        }
        self.file_save_running.insert(path.clone());
        self.spawn_file_save_loop(path, cx);
    }

    /// The real serial writer loop for one path - see [`AdeApp::save_active_file`]'s docs. Reads
    /// the buffer's *current* content fresh at each iteration (not a value captured once at
    /// dispatch time), so a keystroke landing while an earlier write is still in flight is picked
    /// up by this same loop's next pass rather than needing a second, racing task.
    ///
    /// [`AdeApp::file_save_running`] must be cleared on *every* real exit path from the loop
    /// below, not just one of them - a real, if previously only latent, bug an audit caught: an
    /// earlier version only cleared it in the "no pending save left" branch, so a pending save
    /// whose [`AdeApp::edit_buffers`] entry vanished before this loop got to check it (currently
    /// only reachable via `state::reset_per_worktree_ui_state`, which happens to clear
    /// `file_save_running` too today - so this was latent, not live, but one future refactor away
    /// from becoming real) left `file_save_running` stuck containing that path forever, since the
    /// closure below returned `None` without clearing it and the loop broke on exactly that
    /// `None`. [`Self::enqueue_save`] then treats any path still in `file_save_running` as "a
    /// writer loop is already alive for it" and silently no-ops every future save for that path -
    /// a real, silent, permanent, data-loss-adjacent failure the user would have no way to notice
    /// (Ctrl+S would appear to do nothing, forever, for that one file). Restructured so there is
    /// exactly one real place this flag is set ([`Self::enqueue_save`]) and every `None`-producing
    /// branch here clears it before returning `None`, impossible to desync.
    fn spawn_file_save_loop(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let task = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                loop {
                    let step = this.update(cx, |this, _cx| {
                        if !this.file_save_pending.remove(&path) {
                            this.file_save_running.remove(&path);
                            return None;
                        }
                        match this.edit_buffers.get(&path) {
                            Some(buffer) => Some((buffer.path.clone(), buffer.content.clone())),
                            None => {
                                // The buffer vanished while a save was still pending for it -
                                // nothing left to write, but `file_save_running` must still be
                                // cleared here too (see this method's own docs) or this path
                                // becomes permanently unsavable.
                                this.file_save_running.remove(&path);
                                None
                            }
                        }
                    });
                    let Ok(Some((real_path, content))) = step else {
                        break;
                    };
                    // Cloned before `real_path` moves into the background write task below - the
                    // real blame refresh this save should trigger (`force_refresh_blame_for_save`,
                    // see this file's own docs above) needs the file's absolute path *after* the
                    // write settles, once `real_path` itself is no longer available here.
                    let blame_refresh_path = real_path.clone();
                    let write_result = cx
                        .background_executor()
                        .spawn(async move {
                            std::fs::write(&real_path, content.as_bytes())?;
                            let metadata = std::fs::metadata(&real_path)?;
                            let mtime = metadata.modified().ok();
                            let len = metadata.len();
                            Ok::<
                                (std::option::Option<std::time::SystemTime>, u64, String),
                                std::io::Error,
                            >((mtime, len, content))
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        match write_result {
                            Ok((mtime, len, written_content)) => {
                                if let Some(buffer) = this.edit_buffers.get_mut(&path) {
                                    buffer.mark_saved(written_content, mtime, len);
                                }
                                this.file_save_error = None;
                                this.file_external_conflict.remove(&path);
                                // Force the next render's freshness check to re-read metadata
                                // immediately rather than trusting the throttle window - a
                                // `cache_is_fresh` mismatch right after our own save is expected
                                // (we just changed the file's real mtime/len ourselves) and must
                                // be resolved by an immediate reload, not misread as an external
                                // change on the next throttled tick.
                                this.file_view_last_freshness_check = None;
                                // GitHub issue #29: a save is one of the three real triggers
                                // inline blame must recompute on - force it now rather than
                                // waiting up to `BLAME_FRESHNESS_CHECK_INTERVAL` for the generic
                                // poll to notice the new mtime (see `force_refresh_blame_for_
                                // save`'s own docs).
                                this.force_refresh_blame_for_save(&blame_refresh_path, cx);
                            }
                            Err(err) => {
                                this.file_save_error = Some((path.clone(), err.to_string()));
                            }
                        }
                        cx.notify();
                    });
                }
            }
        });
        self._file_save_tasks.insert(path, task);
    }
}

impl EntityInputHandler for AdeApp {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let buffer = self.active_edit_buffer()?;
        let range = buffer.range_from_utf16(&range_utf16);
        actual_range.replace(buffer.range_to_utf16(&range));
        buffer.content.get(range).map(|text| text.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let buffer = self.active_edit_buffer()?;
        Some(UTF16Selection {
            range: buffer.range_to_utf16(&buffer.selected_range),
            reversed: buffer.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let buffer = self.active_edit_buffer()?;
        buffer
            .marked_range
            .as_ref()
            .map(|range| buffer.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(buffer) = self.active_edit_buffer_mut() {
            buffer.unmark();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                let range = range_utf16.map(|range_utf16| buffer.range_from_utf16(&range_utf16));
                buffer.replace_range(range, text);
                self.schedule_rehighlight(path.clone(), cx);
                self.schedule_lsp_sync(path, cx);
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                let range =
                    range_utf16.map(|range_utf16| edit.buffer.range_from_utf16(&range_utf16));
                edit.buffer.replace_range(range, text);
            }
            None => return,
        }
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_edit_target() {
            Some(EditTarget::File(path)) => {
                let Some(buffer) = self.edit_buffers.get_mut(&path) else {
                    return;
                };
                let range = range_utf16.map(|range_utf16| buffer.range_from_utf16(&range_utf16));
                buffer.replace_and_mark_range(range, new_text, new_selected_range_utf16);
                self.schedule_rehighlight(path.clone(), cx);
                self.schedule_lsp_sync(path, cx);
            }
            Some(EditTarget::Merge) => {
                let Some(edit) = self.merge_edit.as_mut() else {
                    return;
                };
                let range =
                    range_utf16.map(|range_utf16| edit.buffer.range_from_utf16(&range_utf16));
                edit.buffer
                    .replace_and_mark_range(range, new_text, new_selected_range_utf16);
            }
            None => return,
        }
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        match self.active_edit_target()? {
            EditTarget::File(path) => {
                let buffer = self.edit_buffers.get(&path)?;
                let (last_path, last_line) = self.file_view_last_layout_for.clone()?;
                if last_path != path {
                    return None;
                }
                let last_layout = self.file_view_last_layout.as_ref()?;
                bounds_for_line_range(buffer, last_layout, last_line, element_bounds, range_utf16)
            }
            EditTarget::Merge => {
                let edit = self.merge_edit.as_ref()?;
                let (last_path, last_line) = self.merge_edit_last_layout_for.clone()?;
                if last_path != edit.relative_path {
                    return None;
                }
                let last_layout = self.merge_edit_last_layout.as_ref()?;
                bounds_for_line_range(
                    &edit.buffer,
                    last_layout,
                    last_line,
                    element_bounds,
                    range_utf16,
                )
            }
        }
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        match self.active_edit_target()? {
            EditTarget::File(path) => {
                let buffer = self.edit_buffers.get(&path)?;
                let (last_path, last_line) = self.file_view_last_layout_for.clone()?;
                if last_path != path {
                    return None;
                }
                let last_bounds = self.file_view_last_bounds?;
                let last_layout = self.file_view_last_layout.as_ref()?;
                character_index_for_line_point(buffer, last_bounds, last_layout, last_line, point)
            }
            EditTarget::Merge => {
                let edit = self.merge_edit.as_ref()?;
                let (last_path, last_line) = self.merge_edit_last_layout_for.clone()?;
                if last_path != edit.relative_path {
                    return None;
                }
                let last_bounds = self.merge_edit_last_bounds?;
                let last_layout = self.merge_edit_last_layout.as_ref()?;
                character_index_for_line_point(
                    &edit.buffer,
                    last_bounds,
                    last_layout,
                    last_line,
                    point,
                )
            }
        }
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.active_edit_buffer().is_some()
    }
}

/// The shared real bounds-for-range math [`EntityInputHandler::bounds_for_range`] needs for
/// whichever buffer/last-painted-layout pair is the current edit target - factored out (Revision
/// R8.5c) so the File view and merge hand-edit view (each with their own dedicated "last painted
/// caret row" cache fields) can't drift into two independently-maintained copies of this real
/// pixel math.
fn bounds_for_line_range(
    buffer: &EditBuffer,
    last_layout: &gpui::ShapedLine,
    last_line: usize,
    element_bounds: Bounds<Pixels>,
    range_utf16: Range<usize>,
) -> Option<Bounds<Pixels>> {
    let line_range = buffer.line_ranges.get(last_line)?;
    let range = buffer.range_from_utf16(&range_utf16);
    let local_start = range.start.checked_sub(line_range.start)?;
    let local_end = range.end.checked_sub(line_range.start)?;
    Some(Bounds::from_corners(
        point(
            element_bounds.left() + last_layout.x_for_index(local_start),
            element_bounds.top(),
        ),
        point(
            element_bounds.left() + last_layout.x_for_index(local_end),
            element_bounds.bottom(),
        ),
    ))
}

/// The mirror of [`bounds_for_line_range`] for
/// [`EntityInputHandler::character_index_for_point`] - see that function's own docs for the real
/// "honest degrade if the painted row no longer matches the buffer's current text" check this
/// preserves.
fn character_index_for_line_point(
    buffer: &EditBuffer,
    last_bounds: Bounds<Pixels>,
    last_layout: &gpui::ShapedLine,
    last_line: usize,
    point_on_screen: Point<Pixels>,
) -> Option<usize> {
    let line_text = buffer.lines.get(last_line)?.text.as_str();
    if last_layout.text.as_ref() != line_text {
        return None;
    }
    let line_point = last_bounds.localize(&point_on_screen)?;
    let local_index = last_layout.index_for_x(line_point.x)?;
    let line_range = buffer.line_ranges.get(last_line)?;
    Some(buffer.offset_to_utf16(line_range.start + local_index))
}

/// Every visible row's real per-row painting context the File view's `uniform_list` row builder
/// (`crate::code_surface::file_view::AdeApp::render_file_view`) hands to
/// [`render_editable_file_view_line`] - see that function's own docs.
pub(in crate::code_surface) struct EditableLineContext<'a> {
    pub entity: Entity<AdeApp>,
    pub focus_handle: FocusHandle,
    pub path: PathBuf,
    pub line_index: usize,
    pub line_number: usize,
    pub line: &'a code_view::RenderedLine,
    pub is_current: bool,
    pub is_changed: bool,
    pub is_cursor_line: bool,
    pub selection_local: Option<Range<usize>>,
    pub cursor_local: Option<usize>,
    pub marked_local: Option<Range<usize>>,
    /// Every *secondary* real cursor's own selection, local to this row - the multi-cursor
    /// mirror of `selection_local` above (which only ever carries the primary's own). See
    /// `crate::code_surface::edit_buffer::EditBuffer::secondary_selections_within_line`'s own
    /// docs; empty in ordinary single-cursor use, so this changes nothing about how an
    /// unaffected row paints.
    pub secondary_selections_local: Vec<Range<usize>>,
    /// Every *secondary* real cursor's own empty-caret position, local to this row - the
    /// multi-cursor mirror of `cursor_local` above. See `crate::code_surface::edit_buffer::
    /// EditBuffer::secondary_cursors_within_line`'s own docs.
    pub secondary_cursors_local: Vec<usize>,
    pub diagnostics: &'a [diagnostics_view::LineDiagnostic],
    pub hovered_byte_range: Option<Range<usize>>,
    pub hover_target: Option<&'a Path>,
    /// GitHub issue #29's real, already-computed inline blame label for *this* line - `Some`
    /// only when `is_current` (only the current line ever shows it); see
    /// `crate::code_surface::blame_view::AdeApp::inline_blame_render_model`'s own docs for how
    /// it's built.
    pub inline_blame: Option<&'a blame::InlineBlameLabel>,
    /// The live, persisted caret shape (GitHub issue #27) - read once per row from
    /// `AdeApp::settings.appearance.caret_style` rather than threaded through some separate
    /// theme mechanism, matching every other persisted-and-applied `Settings` field's own
    /// pattern.
    pub caret_style: settings_store::CaretStyle,
    /// The live shared blink phase (GitHub issue #27) - `AdeApp::caret_blink_visible`, read once
    /// per row rather than re-derived; see `crate::root::caret_blink`'s module docs for the
    /// whole mechanism this feeds.
    pub caret_blink_visible: bool,
}

/// The real quad(s) to paint for a caret at pixel range `[start_x, end_x)` on a row spanning
/// `[top, bottom)` (GitHub issue #27) - shared by
/// [`render_editable_file_view_line`]/`crate::merge::editing`'s own row painter (the merge
/// hand-edit view's deliberately-separate mirror of this same paint approach - see
/// `crate::merge::editing::MergeEditLineContext`'s own docs for why it stays a separate `struct`)
/// so the app's two caret-bearing surfaces can never visually drift apart, satisfying issue #27's
/// own "consistent caret style ... across the code editor and all app text inputs" ask for at
/// least these two. `end_x` is only read for [`settings_store::CaretStyle::Block`]/
/// [`settings_store::CaretStyle::Underline`] (the width of the character at the caret); pass
/// `start_x` again for [`settings_store::CaretStyle::Line`] callers with nothing convenient to
/// measure it from.
///
/// Returns `None` exactly when the caret should be invisible this frame: mid-blink "off" phase
/// while genuinely focused (`is_focused && !blink_visible`) - never while unfocused, which always
/// paints a real, dimmed, non-blinking caret instead (issue #27's own explicit ask), never
/// nothing at all.
pub(crate) fn caret_paint_quad(
    start_x: Pixels,
    end_x: Pixels,
    top: Pixels,
    bottom: Pixels,
    style: settings_store::CaretStyle,
    is_focused: bool,
    blink_visible: bool,
) -> Option<PaintQuad> {
    if is_focused && !blink_visible {
        return None;
    }
    let color = if is_focused {
        theme::syntax::CARET.resolve()
    } else {
        theme::syntax::CARET
            .resolve()
            .opacity(theme::syntax::CARET_UNFOCUSED_OPACITY)
    };
    // At most one real char's width (never negative - `end_x` can equal `start_x` for a
    // `Line`-style caller, or a real end-of-line caret with nothing after it to measure).
    let char_width = (end_x - start_x).max(gpui::px(1.0));
    match style {
        settings_store::CaretStyle::Line => Some(fill(
            Bounds::new(point(start_x, top), size(gpui::px(2.0), bottom - top)),
            color,
        )),
        settings_store::CaretStyle::Block => Some(fill(
            Bounds::new(point(start_x, top), size(char_width, bottom - top)),
            color,
        )),
        settings_store::CaretStyle::Underline => {
            let thickness = gpui::px(2.0);
            Some(fill(
                Bounds::new(
                    point(start_x, bottom - thickness),
                    size(char_width, thickness),
                ),
                color,
            ))
        }
    }
}

/// The real, editable File view's per-row renderer - the `"real cursor/selection needs real
/// per-row `ShapedLine` shaping"` piece this phase's design calls for (see this module's own top
/// docs). Structurally mirrors `crate::code_surface::file_view::render_file_view_line`'s gutter/git-
/// gutter/diagnostics chrome (kept as ordinary `div`s - no reason to reinvent that) **and** its
/// real per-run `div`-per-token text rendering (see [`build_text_run_divs`]) - a real, live-
/// measured bug an audit caught in an earlier version of this function ruled out rendering the
/// visible glyphs from a bare `gpui::canvas` instead: a `canvas` contributes *no* intrinsic
/// content size to GPUI's own layout pass (it has no text for the ordinary text-measurement path
/// to see), so a canvas-only row collapsed to a near-fixed handful of pixels regardless of the
/// real line's length, confirmed via `cx.debug_bounds` - exactly the "looks right in the one
/// case tried, silently wrong otherwise" bug class this project's history keeps finding. Reusing
/// the read-only path's own proven `div`-per-run text rendering for the *visible* glyphs (real,
/// already-correct content-based sizing) fixes that; a `gpui::canvas` is still used, but only as
/// an `.absolute().size_full()` overlay on top of that now-correctly-sized row (GPUI's own "low
/// level paint API without defining a whole custom element", `vendor/zed/crates/gpui/src/
/// elements/canvas.rs`), purely to shape the line once (`Window::text_system().shape_line`, for
/// real pixel-accurate `x_for_index`/`closest_index_for_x` cursor/selection/click math - the
/// actual glyphs it shapes are never painted, since the sibling `div`s already show them) and to
/// paint the real cursor bar/selection fill and (only for the caret's own row) register the real
/// `EntityInputHandler` wiring - not fabricated pixel math, and no risk of the overlay's shaped
/// text visually disagreeing with the `div`s' own real glyphs, since both are built from the
/// exact same `line`/`diagnostics`/`hovered_byte_range`/`marked_local` inputs under the same
/// ambient font/size.
pub(in crate::code_surface) fn render_editable_file_view_line(
    context: EditableLineContext<'_>,
    row_line_height: Pixels,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    let EditableLineContext {
        entity,
        focus_handle,
        path,
        line_index,
        line_number,
        line,
        is_current,
        is_changed,
        is_cursor_line,
        selection_local,
        cursor_local,
        marked_local,
        secondary_selections_local,
        secondary_cursors_local,
        diagnostics,
        hovered_byte_range,
        hover_target,
        inline_blame,
        caret_style,
        caret_blink_visible,
    } = context;

    let gutter_color = if is_current {
        theme::editor::GUTTER_TEXT_ACTIVE
    } else {
        theme::editor::GUTTER_TEXT
    };
    let worst_severity = diagnostics_view::Severity::worst(diagnostics);

    let runs = build_text_runs(
        line,
        diagnostics,
        worst_severity,
        &hovered_byte_range,
        &marked_local,
    );
    let line_text: gpui::SharedString = line.text.clone().into();
    let visible_runs = build_text_run_divs(
        line,
        diagnostics,
        worst_severity,
        &hovered_byte_range,
        &marked_local,
    );

    let row_path = path.clone();
    // A second, independent clone for the `.on_mouse_move` drag-extend handler below - it needs
    // its own owned `PathBuf` exactly like the `.on_mouse_down` handler's own `row_path` does
    // (both are separate `move` closures), not a second reference to the same one `on_mouse_down`
    // already moved into itself.
    let drag_row_path = row_path.clone();
    let click_line_index = line_index;
    let click_line_number = line_number;
    let click_hover_target = hover_target.map(|target| target.to_path_buf());
    let click_line_runs = line.runs.clone();
    let click_line_text = line.text.clone();

    let paint_entity = entity;
    let paint_path = path;

    // Overlay-only: paints the real cursor bar/selection fill and registers the real
    // `EntityInputHandler` wiring, never the visible glyphs themselves - see this function's own
    // docs for why. `.absolute().size_full()` inside the `.relative()` `text_row` below, matching
    // the same proven idiom `terminal_pane.rs`'s own `measure_bounds` canvas and
    // `work_surface_render.rs`'s `plus_button_bounds` canvas already use in this crate: an
    // absolutely-positioned child doesn't affect `text_row`'s own real width/height (which now
    // comes from `visible_runs`' own real, in-flow text content below), it just fills whatever
    // box that real content already resolved to.
    // A separate clone from `focus_handle` below - the measurement closure and the paint closure
    // are two independent `move` closures (`gpui::canvas`'s own real two-callback shape), each
    // needing its own owned handle, not a second reference to the one the paint closure moves
    // into itself for `window.handle_input`.
    let focus_handle_for_measure = focus_handle.clone();
    let cursor_overlay = gpui::canvas(
        move |bounds, window, _cx| {
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let shaped = window
                .text_system()
                .shape_line(line_text.clone(), font_size, &runs, None);

            // GitHub issue #27: "selection remains visible (dimmed) when the editor loses
            // focus" / "unfocused editors show a dimmed, non-blinking caret" - both the
            // selection fill and the caret itself read this same real, live focus check, not two
            // independently-derived ones that could disagree.
            let is_focused = focus_handle_for_measure.is_focused(window);
            let selection_opacity = if is_focused {
                theme::editor::SELECTION_OPACITY
            } else {
                theme::editor::SELECTION_INACTIVE_OPACITY
            };
            let selection_quad = selection_local.as_ref().map(|range| {
                fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + shaped.x_for_index(range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + shaped.x_for_index(range.end),
                            bounds.bottom(),
                        ),
                    ),
                    theme::editor::SELECTION
                        .resolve()
                        .opacity(selection_opacity),
                )
            });
            let cursor_quad = cursor_local.and_then(|offset| {
                let start_x = bounds.left() + shaped.x_for_index(offset);
                // The real next char boundary after `offset` (for `Block`/`Underline` styles'
                // own real character-width measurement) - `offset` itself for a caret at the
                // real end of the line, which `caret_paint_quad` falls back to a minimal width
                // for rather than measuring a character that isn't there.
                let next_offset = line_text
                    .as_ref()
                    .get(offset..)
                    .and_then(|rest| rest.chars().next())
                    .map(|ch| offset + ch.len_utf8())
                    .unwrap_or(offset);
                let end_x = bounds.left() + shaped.x_for_index(next_offset);
                caret_paint_quad(
                    start_x,
                    end_x,
                    bounds.top(),
                    bounds.bottom(),
                    caret_style,
                    is_focused,
                    caret_blink_visible,
                )
            });
            // Multi-cursor (Revision R13, issue #28): every *secondary* real cursor's own
            // selection fill/caret bar on this row, painted with the exact same real tokens as
            // the primary's own above - `theme.rs` has no separate "secondary cursor" color, and
            // inventing one with no `design_handoff_jerry_ade` spec to back it would be an
            // unjustified guess (`CONTRIBUTING.md`'s own "exact values" discipline) - so a real
            // multi-cursor agent simply shows several real, identically-styled carets/
            // selections rather than a fabricated visual distinction between them. Empty in
            // ordinary single-cursor use, so this is real, additional work only when it's real,
            // additional cursors.
            let secondary_selection_quads: Vec<_> = secondary_selections_local
                .iter()
                .map(|range| {
                    fill(
                        Bounds::from_corners(
                            point(
                                bounds.left() + shaped.x_for_index(range.start),
                                bounds.top(),
                            ),
                            point(
                                bounds.left() + shaped.x_for_index(range.end),
                                bounds.bottom(),
                            ),
                        ),
                        theme::editor::SELECTION
                            .resolve()
                            .opacity(theme::editor::SELECTION_OPACITY),
                    )
                })
                .collect();
            let secondary_cursor_quads: Vec<_> = secondary_cursors_local
                .iter()
                .map(|offset| {
                    fill(
                        Bounds::new(
                            point(bounds.left() + shaped.x_for_index(*offset), bounds.top()),
                            size(gpui::px(2.0), bounds.bottom() - bounds.top()),
                        ),
                        theme::editor::CARET,
                    )
                })
                .collect();
            (
                shaped,
                selection_quad,
                cursor_quad,
                secondary_selection_quads,
                secondary_cursor_quads,
            )
        },
        move |bounds,
              (
            shaped,
            selection_quad,
            cursor_quad,
            secondary_selection_quads,
            secondary_cursor_quads,
        ),
              window,
              cx| {
            if is_cursor_line {
                window.handle_input(
                    &focus_handle,
                    ElementInputHandler::new(bounds, paint_entity.clone()),
                    cx,
                );
            }
            if let Some(selection_quad) = selection_quad {
                window.paint_quad(selection_quad);
            }
            for quad in secondary_selection_quads {
                window.paint_quad(quad);
            }
            // The primary caret's own visibility (blink phase, unfocused dimming) is already
            // fully decided by `caret_paint_quad` above - `cursor_quad` is `None` exactly when
            // it shouldn't paint this frame, so no extra `is_focused` gate belongs here (GitHub
            // issue #27's own docs on `caret_paint_quad` are explicit about this). Secondary
            // cursors don't have their own blink/dim treatment yet (see this row's own docs on
            // why multi-cursor deliberately keeps every cursor identically styled), so they paint
            // unconditionally too, matching the primary selection's own always-visible-when-
            // present treatment above - the alternative (secondary cursors vanishing on focus
            // loss while the primary caret stays dimly visible) would be a real, visible
            // inconsistency between cursors in the same buffer.
            if let Some(cursor_quad) = cursor_quad {
                window.paint_quad(cursor_quad);
            }
            for quad in secondary_cursor_quads {
                window.paint_quad(quad);
            }
            let row_layout_entry = (bounds, shaped.clone());
            paint_entity.update(cx, |this, _cx| {
                this.file_view_row_layout
                    .insert(line_number, row_layout_entry);
                if is_cursor_line {
                    this.file_view_last_layout = Some(shaped);
                    this.file_view_last_bounds = Some(bounds);
                    this.file_view_last_layout_for = Some((paint_path.clone(), line_index));
                }
            });
        },
    )
    .absolute()
    .size_full();

    let mut text_row = gpui::div()
        .id(("file-view-editable-text", line_number))
        .relative()
        .flex_1()
        .min_w_0()
        .h(row_line_height)
        .flex()
        // The code runs keep their natural width in their own `flex_none` box, so they never
        // shrink; only the blame span placed beside them below yields and truncates.
        .child(gpui::div().flex_none().flex().children(visible_runs));
    // GitHub issue #29: the current line's dimmed inline git blame, placed *in-flow* immediately
    // after the code text so it begins right at the end of the line and is truncated at the
    // pane's right edge - rather than pinned to the far right of the row (a flex sibling of the
    // `flex_1` text wrapper), where it used to be painted on top of a long line's own overflowing
    // glyphs. `inline_blame` is only ever `Some` on the current line (see
    // `EditableLineContext::inline_blame`'s own docs), so this never appears on any other row.
    if let Some(label) = inline_blame {
        text_row = text_row.child(render_inline_blame_span(label, line_number));
    }
    let text_row = text_row
        .child(cursor_overlay)
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                let Some((bounds, shaped)) =
                    this.file_view_row_layout.get(&click_line_number).cloned()
                else {
                    return;
                };
                let Some(local_point) = bounds.localize(&event.position) else {
                    return;
                };
                let local_offset = shaped.closest_index_for_x(local_point.x);
                // `this` (not a separately-captured `Entity<AdeApp>::read(cx)`) - `this` is
                // already the real, live-leased `&mut AdeApp` `cx.listener` hands this closure;
                // a real, live-reproduced bug this fixes: reading a *second*, independent handle
                // to the same entity while `cx.listener`'s own update lease is still active is a
                // real double-lease, and GPUI's `EntityMap::read` panics on exactly that
                // (confirmed live: "cannot read app::root::AdeApp while it is already being
                // updated") - every real left-click on an editable row hit this, unconditionally.
                let Some(buffer) = this.edit_buffers.get(&row_path) else {
                    return;
                };
                let Some(line_range) = buffer.line_ranges.get(click_line_index).cloned() else {
                    return;
                };
                // Revision R8.5b: hover is no longer suppressed on a dirty buffer - `hover_view::
                // position_for_line_byte_offset` below is computed from `click_line_text`, the
                // *live* buffer's own line text (see `EditableLineContext::line`'s docs), and the
                // language server now genuinely tracks that same live content via
                // `Self::schedule_lsp_sync`'s real `didChange` sync, not just the last-saved
                // snapshot (a real, honest latency window still exists between an edit landing
                // and the server's diagnostics answering for it - see `Self::schedule_lsp_sync`'s
                // own docs).
                let absolute_offset = line_range.start + local_offset;

                window.focus(&this.code_focus_handle, cx);
                // A real click moves the caret somewhere the popup's own anchor almost certainly
                // no longer describes - see `Self::move_active_buffer`'s own docs for the same
                // real dismiss-on-caret-move reasoning.
                this.dismiss_completions();
                if let Some(buffer) = this.edit_buffers.get_mut(&row_path) {
                    // Alt+click (Revision R13, issue #28): adds a brand-new cursor at the click
                    // point, keeping every existing real cursor - `EditBuffer::add_cursor_at`'s
                    // own docs. Checked first, before the click-count/shift chain below, so an
                    // Alt-modified click always means "add a cursor" regardless of click count -
                    // this editor has no mouse-drag-to-select of any kind yet (only click/
                    // shift-click), so a real Alt+Shift+*drag* column selection is a separate,
                    // currently undone piece of work (see `crate::code_surface::edit_buffer`'s
                    // own "Multi-cursor" docs for why) - a plain Alt+Shift+click still does
                    // something real and useful in the meantime rather than silently falling
                    // through to a plain click.
                    //
                    // GitHub issue #27: "double-click selects a word, triple-click selects a
                    // line, drag extends, Shift+click extends from the caret." GPUI's real
                    // `MouseDownEvent::click_count` (`vendor` GPUI's own `interactive.rs`,
                    // verified via the finder subagent before writing this) already counts
                    // consecutive same-position clicks, so this app doesn't need its own
                    // double/triple-click timing - it just reads the count GPUI already
                    // computed. `>= 3` (not `== 3`) so a fourth/fifth rapid click keeps
                    // re-selecting the line rather than falling back to a plain caret placement.
                    if event.modifiers.alt {
                        buffer.add_cursor_at(absolute_offset);
                    } else if event.click_count >= 3 {
                        buffer.select_line_at(click_line_index);
                    } else if event.click_count == 2 {
                        buffer.select_word_at(absolute_offset);
                    } else if event.modifiers.shift {
                        buffer.select_to(absolute_offset);
                    } else {
                        buffer.move_to(absolute_offset);
                    }
                }
                this.code_cursor = Some(click_line_number);
                this.reset_caret_blink(cx);

                if let Some(hover_target) = &click_hover_target {
                    if let Some(token_range) = token_at_offset(&click_line_runs, local_offset) {
                        let token_text =
                            click_line_text.get(token_range.clone()).unwrap_or_default();
                        if !token_text.trim().is_empty() {
                            let position = hover_view::position_for_line_byte_offset(
                                click_line_number as u32 - 1,
                                &click_line_text,
                                token_range.start,
                            );
                            this.request_hover(
                                hover_target.clone(),
                                click_line_number,
                                token_range,
                                position,
                                cx,
                            );
                        }
                    }
                }
                cx.stop_propagation();
                cx.notify();
            }),
        )
        // GitHub issue #27's "drag extends" - real-drag detection via `MouseMoveEvent::
        // dragging()` (`vendor` GPUI's own `interactive.rs`: `self.pressed_button ==
        // Some(MouseButton::Left)`, verified via the finder subagent, real usage confirmed at
        // `data_table.rs:350`'s own `if !ev.dragging() { return; }`), the same real idiom this
        // whole file's own per-row hit-testing already uses for clicks - registered per-row
        // (matching `crate::code_surface::lsp_ui`'s own per-row `.on_mouse_move` hover-debounce
        // precedent in this same crate) rather than a window-level capture, so this naturally
        // only extends the selection while the pointer is actually over *some* row - dragging
        // past the very top/bottom of the visible rows (auto-scroll) is a real, documented gap,
        // not built this phase - see `BUILD-LOG.md`.
        .on_mouse_move(
            cx.listener(move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                if !event.dragging() {
                    return;
                }
                let Some((bounds, shaped)) =
                    this.file_view_row_layout.get(&click_line_number).cloned()
                else {
                    return;
                };
                let Some(local_point) = bounds.localize(&event.position) else {
                    return;
                };
                let local_offset = shaped.closest_index_for_x(local_point.x);
                let Some(buffer) = this.edit_buffers.get(&drag_row_path) else {
                    return;
                };
                let Some(line_range) = buffer.line_ranges.get(click_line_index).cloned() else {
                    return;
                };
                let absolute_offset = line_range.start + local_offset;
                let Some(buffer) = this.edit_buffers.get_mut(&drag_row_path) else {
                    return;
                };
                buffer.select_to(absolute_offset);
                this.code_cursor = Some(click_line_number);
                this.reset_caret_blink(cx);
                cx.notify();
            }),
        );

    // `.w_full()` (real fix, this revision): without it, a real GPUI row painted at the root of
    // `uniform_list`'s per-item layout sizes itself to its own content (shrink-to-fit), not to
    // the list's full available width - confirmed by measuring `file-view-text-row-N`'s own real
    // painted bounds, which used to differ per line length. That made a real click land nowhere
    // (no element to hit-test against) anywhere past a short line's own last glyph: not in the
    // blank space to the right of short text, and not on a blank line at all past column zero -
    // exactly the real "click only works on existing text" bug this revision fixes. `.w_full()`
    // makes every row (and, since `text_row`/its wrapper are already `.flex_1()`, the click
    // target nested inside it) span the row's real full available width regardless of content,
    // so `text_row`'s own `on_mouse_down` below - which already correctly clamps via
    // `gpui::LineLayout::closest_index_for_x` returning the shaped line's real length for any `x`
    // past its last glyph - now actually receives the click in the first place. A same-width
    // real side effect: the current-line highlight below now also spans the row's full width,
    // matching every real code editor's own current-line highlight instead of stopping at the
    // last character.
    let mut row = gpui::div()
        .id(("file-view-line", line_number))
        .w_full()
        .flex_none()
        .flex()
        .items_center();
    if is_current {
        row = row.bg(theme::editor::CURRENT_LINE);
    } else if let Some(bg) = worst_severity.and_then(diagnostic_row_bg) {
        row = row.bg(bg);
    }

    row = row
        .child(
            gpui::div()
                .flex_none()
                .w(gpui::px(52.0))
                .pr(gpui::px(12.0))
                .text_right()
                .text_color(gutter_color)
                .text_size(gpui::px(11.0))
                // Matches the read-only File view's own `render_file_view_line` selector - see
                // that function's docs (`code_zoom_tests::zoom_scales_text_but_not_the_gutter_
                // width` measures this exact id against both rendering paths).
                .debug_selector(move || format!("file-view-gutter-{line_number}"))
                .child(line_number.to_string()),
        )
        .child(
            gpui::div()
                .flex_none()
                .w(gpui::px(3.0))
                .self_stretch()
                .bg(if is_changed {
                    theme::editor::DIFF_ADDED
                } else {
                    theme::ColorToken(crate::work_surface::state::TRANSPARENT)
                }),
        )
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .pl(gpui::px(12.0))
                .flex()
                .items_center()
                .debug_selector(move || format!("file-view-text-row-{line_number}"))
                .child(text_row),
        );

    if let Some(first) = diagnostics.first() {
        let first_line = first.message.lines().next().unwrap_or_default();
        row = row.child(
            gpui::div()
                .pl(gpui::px(10.0))
                .text_color(diagnostic_inline_message_color(first.severity))
                .child(first_line.to_string()),
        );
    }

    // NB: the current line's inline git blame is rendered *inside* `text_row` above (right after
    // the code runs), not appended here at the end of the row - see that construction's own docs.

    row.into_any_element()
}

/// Builds real, syntax-highlight-colored `TextRun`s for one row, reusing
/// `diagnostics_view::overlay_diagnostic_runs` (the same real function the read-only File view's
/// own `render_file_view_line` already uses) so diagnostic/hover underlines are expressed as real
/// `TextRun::underline` decoration rather than a second, div-based implementation - see this
/// module's own top docs.
fn build_text_runs(
    line: &code_view::RenderedLine,
    diagnostics: &[diagnostics_view::LineDiagnostic],
    worst_severity: Option<diagnostics_view::Severity>,
    hovered_byte_range: &Option<Range<usize>>,
    marked_local: &Option<Range<usize>>,
) -> Vec<TextRun> {
    let font = crate::theme::font::MONO;
    let mut cursor = 0usize;
    let mut runs = Vec::new();
    for (text, kind, is_diagnostic) in
        diagnostics_view::overlay_diagnostic_runs(&line.runs, diagnostics)
    {
        let start = cursor;
        let end = start + text.len();
        cursor = end;
        let underline = if is_diagnostic {
            let color = worst_severity
                .map(diagnostic_underline_color)
                .unwrap_or(theme::syntax::ERROR_UNDERLINE.into());
            Some(UnderlineStyle {
                color: Some(color.into()),
                thickness: gpui::px(1.0),
                wavy: false,
            })
        } else if hovered_byte_range.as_ref() == Some(&(start..end)) {
            Some(UnderlineStyle {
                color: Some(theme::syntax::HOVER_UNDERLINE.into()),
                thickness: gpui::px(1.0),
                wavy: false,
            })
        } else {
            None
        };
        runs.push(TextRun {
            len: text.len(),
            font: gpui::font(font),
            color: code_view::color_for_kind(kind).into(),
            background_color: None,
            underline,
            strikethrough: None,
        });
    }

    if let Some(marked) = marked_local {
        runs = split_runs_for_marked_range(runs, marked);
    }
    runs
}

/// Builds the real, visible per-run `div`s for one editable row's text - the same real
/// `diagnostics_view::overlay_diagnostic_runs`-driven classification/diagnostic/hover treatment
/// `crate::code_surface::file_view::render_file_view_line` (the read-only path) already uses, so the
/// two rendering paths can never visually disagree on what a given run should look like. See
/// [`render_editable_file_view_line`]'s own docs for why these `div`s - not the sibling
/// `gpui::canvas` overlay - are what's actually responsible for both the visible glyphs and the
/// row's own real, content-based width/height.
fn build_text_run_divs(
    line: &code_view::RenderedLine,
    diagnostics: &[diagnostics_view::LineDiagnostic],
    worst_severity: Option<diagnostics_view::Severity>,
    hovered_byte_range: &Option<Range<usize>>,
    marked_local: &Option<Range<usize>>,
) -> Vec<gpui::AnyElement> {
    let mut cursor = 0usize;
    let mut elements = Vec::new();
    for (text, kind, is_diagnostic) in
        diagnostics_view::overlay_diagnostic_runs(&line.runs, diagnostics)
    {
        let start = cursor;
        let end = start + text.len();
        cursor = end;
        let is_hovered = hovered_byte_range.as_ref() == Some(&(start..end));

        // Splits this run further at the real marked (IME composition) range's own byte
        // boundaries, if it overlaps at all - the same real byte-accurate splitting
        // `split_runs_for_marked_range` applies to `TextRun`s, applied here to display `div`s
        // instead, so a composition that starts or ends mid-run still gets a precisely-bounded
        // underline rather than one rounded out to the whole run (which - since a fresh edit
        // always resets a line to one big plain-`Text` run via `EditBuffer::rebuild_plain` until
        // the next debounced re-highlight - would otherwise routinely underline far more than
        // the real composing text, most severely misleadingly the *entire line*).
        let marked_overlap = marked_local.as_ref().and_then(|marked| {
            let overlap_start = marked.start.max(start);
            let overlap_end = marked.end.min(end);
            (overlap_start < overlap_end).then_some((overlap_start, overlap_end))
        });

        let Some((marked_start, marked_end)) = marked_overlap else {
            elements.push(text_run_div(
                kind,
                text.as_ref(),
                is_diagnostic,
                is_hovered,
                worst_severity,
                false,
            ));
            continue;
        };
        if marked_start > start {
            elements.push(text_run_div(
                kind,
                &text[0..marked_start - start],
                is_diagnostic,
                is_hovered,
                worst_severity,
                false,
            ));
        }
        elements.push(text_run_div(
            kind,
            &text[marked_start - start..marked_end - start],
            is_diagnostic,
            is_hovered,
            worst_severity,
            true,
        ));
        if end > marked_end {
            elements.push(text_run_div(
                kind,
                &text[marked_end - start..],
                is_diagnostic,
                is_hovered,
                worst_severity,
                false,
            ));
        }
    }
    elements
}

/// Builds one real display `div` for a single (possibly marked-range-split) text piece - shared
/// by every branch of [`build_text_run_divs`]'s own real splitting logic.
fn text_run_div(
    kind: code_view::HighlightKind,
    text: &str,
    is_diagnostic: bool,
    is_hovered: bool,
    worst_severity: Option<diagnostics_view::Severity>,
    is_marked: bool,
) -> gpui::AnyElement {
    let mut run = gpui::div()
        .text_color(code_view::color_for_kind(kind))
        .child(text.to_string());
    if is_diagnostic {
        // A diagnostic underline always wins over a hover/composition underline on the same
        // piece, matching the read-only path's own precedence.
        let underline_color = worst_severity
            .map(diagnostic_underline_color)
            .unwrap_or(theme::syntax::ERROR_UNDERLINE.into());
        run = run
            .border_b_2()
            .border_color(underline_color)
            .border_dashed();
    } else if is_marked {
        run = run
            .border_b_1()
            .border_color(code_view::color_for_kind(kind));
    } else if is_hovered {
        run = run
            .border_b_1()
            .border_color(theme::syntax::HOVER_UNDERLINE);
    }
    run.into_any_element()
}

/// Overlays a real IME-composition underline onto `runs`, splitting any run(s) `marked` crosses -
/// composition must stay visible regardless of whatever syntax/diagnostic coloring sits under it.
/// `pub(crate)` (Revision R8.5c) - `crate::merge::editing`'s own simplified row painter
/// reuses this for the merge hand-edit view's real IME-composition-range underline, rather than a
/// second, duplicate implementation.
pub(crate) fn split_runs_for_marked_range(
    runs: Vec<TextRun>,
    marked: &Range<usize>,
) -> Vec<TextRun> {
    if marked.start >= marked.end {
        return runs;
    }
    let mut result = Vec::with_capacity(runs.len() + 2);
    let mut cursor = 0usize;
    for run in runs {
        let start = cursor;
        let end = start + run.len;
        cursor = end;
        let overlap_start = marked.start.max(start);
        let overlap_end = marked.end.min(end);
        if overlap_start >= overlap_end {
            result.push(run);
            continue;
        }
        if overlap_start > start {
            result.push(TextRun {
                len: overlap_start - start,
                ..run.clone()
            });
        }
        result.push(TextRun {
            len: overlap_end - overlap_start,
            underline: Some(UnderlineStyle {
                color: Some(run.color),
                thickness: gpui::px(1.0),
                wavy: false,
            }),
            ..run.clone()
        });
        if end > overlap_end {
            result.push(TextRun {
                len: end - overlap_end,
                ..run
            });
        }
    }
    result
}

/// Finds the real syntax-highlight run in `runs` (a `RenderedLine::runs` slice) containing byte
/// `offset`, for click-to-hover token detection - mirrors the read-only File view's own per-run
/// click boundaries (`crate::code_surface::file_view::render_file_view_line`), computed here from a
/// hit-tested offset instead of from per-run `div` boundaries.
fn token_at_offset(
    runs: &[(gpui::SharedString, code_view::HighlightKind)],
    offset: usize,
) -> Option<Range<usize>> {
    let mut cursor = 0usize;
    for (text, _) in runs {
        let start = cursor;
        let end = start + text.len();
        cursor = end;
        if offset >= start && offset <= end {
            return Some(start..end);
        }
    }
    None
}

#[cfg(test)]
mod editing_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn write_file(repo: &std::path::Path, name: &str, content: &str) -> PathBuf {
        let path = repo.join(name);
        std::fs::write(&path, content).expect("write file");
        path
    }

    fn bind_real_keys(cx: &mut gpui::VisualTestContext) {
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
    }

    /// Opens `path` in the File view and forces two real, synchronous render passes around a
    /// `run_until_parked` - mirrors `code_view_cache_tests`' own established precedent
    /// (`crates/app/src/code_surface/tabs.rs`) for making sure the background load has actually
    /// dispatched *and* resolved (which is also the real point `AdeApp::edit_buffers` gets seeded)
    /// before a test starts asserting against it.
    fn open_file_for_editing(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        path: PathBuf,
    ) {
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(path, window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
    }

    /// Real test (a): typing changes the real buffer content immediately, and the real
    /// `tree-sitter` highlight (deliberately debounced - see this module's own "Re-highlighting
    /// cost" docs) lands once the debounce elapses, changing a token's real classification from
    /// plain `Text` to `Keyword`/`Function`.
    ///
    /// Also GitHub issue #48's own real regression coverage at the `AdeApp`/`replace_text_in_range`
    /// level (`crate::code_surface::edit_buffer`'s own test module covers the same fix directly
    /// against `EditBuffer::splice_lines`): this file already carried real, live `tree-sitter`
    /// highlighting for "foo"/the punctuation before the edit (`open_file_for_editing`'s
    /// background load ran a real highlight), so before the fix this same edit reset the *whole*
    /// line back to plain `Text` for the ~150ms until the debounce fired - the real flicker the
    /// issue reports. It must not do that anymore.
    #[gpui::test]
    fn typing_changes_real_content_and_updates_syntax_highlighting_after_the_debounce(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.rs", "foo(x: i32) {}\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.rs");

        let initial_kinds = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().lines[0]
                .runs
                .iter()
                .map(|(_, kind)| *kind)
                .collect::<Vec<_>>()
        });
        assert!(
            initial_kinds.contains(&code_view::HighlightKind::Function),
            "the file must already carry a real highlight before the edit, or this test cannot \
             tell a real fix from a vacuous one: {initial_kinds:?}"
        );

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "fn ", window, cx);
        });

        let content = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .expect("buffer should exist")
                .content
                .clone()
        });
        assert_eq!(content, "fn foo(x: i32) {}\n");

        let (dirty_immediately, kinds_immediately) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            (
                buffer.highlight_dirty,
                buffer.lines[0]
                    .runs
                    .iter()
                    .map(|(_, kind)| *kind)
                    .collect::<Vec<_>>(),
            )
        });
        assert!(
            dirty_immediately,
            "the real tree-sitter highlight hasn't run yet - only the cheap incremental splice has"
        );
        assert!(
            !kinds_immediately
                .iter()
                .all(|kind| *kind == code_view::HighlightKind::Text),
            "GitHub issue #48: the whole line must not flash back to plain text while the real \
             re-highlight is still pending - only the actually-new/changed bytes may; runs: \
             {kinds_immediately:?}"
        );
        assert!(
            kinds_immediately.contains(&code_view::HighlightKind::Function),
            "\"foo\"'s own already-known real highlighting must survive this edit untouched, not \
             just get lucky in the eventual re-highlight: {kinds_immediately:?}"
        );
        assert!(
            kinds_immediately.contains(&code_view::HighlightKind::PunctuationBracket),
            "the untouched brackets' own already-known real highlighting must survive too: \
             {kinds_immediately:?}"
        );

        cx.background_executor
            .advance_clock(REHIGHLIGHT_DEBOUNCE + Duration::from_millis(50));
        cx.run_until_parked();

        let (dirty_after, kinds_after) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            (
                buffer.highlight_dirty,
                buffer.lines[0]
                    .runs
                    .iter()
                    .map(|(_, kind)| *kind)
                    .collect::<Vec<_>>(),
            )
        });
        assert!(
            !dirty_after,
            "the debounced real highlight should have landed"
        );
        assert!(
            kinds_after.contains(&code_view::HighlightKind::Keyword),
            "\"fn\" should now be a real keyword: {kinds_after:?}"
        );
        assert!(
            kinds_after.contains(&code_view::HighlightKind::Function),
            "\"foo\" should now be a real function name: {kinds_after:?}"
        );
    }

    /// Real test (b): real arrow-key cursor movement, including crossing a real line boundary -
    /// driven through the real, bound `EditorRight` keystroke, not a direct method call.
    #[gpui::test]
    fn arrow_keys_move_the_real_caret_and_cross_a_real_line_boundary(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "ab\ncd\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .selected_range
                .clone()),
            0..0
        );

        cx.simulate_keystrokes("right right right");

        let selected = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .unwrap()
                .selected_range
                .clone()
        });
        assert_eq!(
            selected,
            3..3,
            "three real right-arrow keystrokes from offset 0 in \"ab\\ncd\\n\" should cross the \
             real line boundary and land at offset 3, the start of line 2 (\"c\")"
        );

        cx.simulate_keystrokes("left");
        let selected = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .unwrap()
                .selected_range
                .clone()
        });
        assert_eq!(
            selected,
            2..2,
            "a real left-arrow keystroke should cross back to the end of line 1"
        );
    }

    /// Real test (c): real shift+arrow selection, driven through the real, bound
    /// `EditorSelectRight` keystroke.
    #[gpui::test]
    fn shift_arrow_extends_a_real_selection_through_the_real_key_bindings(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        cx.simulate_keystrokes("shift-right shift-right shift-right");

        let selected = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .unwrap()
                .selected_range
                .clone()
        });
        assert_eq!(
            selected,
            0..3,
            "three real shift-right keystrokes should select \"hel\""
        );
    }

    /// GitHub issue #27: "Ctrl+Shift+arrows (word-wise)" - driven through the real, bound
    /// `EditorSelectWordRight`/`EditorWordLeft` keystrokes, matching this module's own
    /// established "through the real key bindings, not a direct method call" discipline for
    /// every other `Editor*` action test in this file.
    #[gpui::test]
    fn ctrl_shift_arrow_extends_a_real_selection_word_wise_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello world\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        let select_word_right = if cfg!(target_os = "macos") {
            "cmd-shift-right"
        } else {
            "ctrl-shift-right"
        };
        cx.simulate_keystrokes(select_word_right);

        let selected = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .unwrap()
                .selected_range
                .clone()
        });
        assert_eq!(
            selected,
            0..5,
            "one real Ctrl+Shift+Right from offset 0 in \"hello world\" should select exactly \
             \"hello\", the whole first real word - not one grapheme, matching plain \
             `shift-right`'s own behavior"
        );

        let word_left = if cfg!(target_os = "macos") {
            "cmd-left"
        } else {
            "ctrl-left"
        };
        cx.simulate_keystrokes(word_left);
        let cursor = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().cursor_offset()
        });
        assert_eq!(
            cursor, 0,
            "a real Ctrl+Left should collapse the selection to its start (real `move_word_left` \
             semantics - a real selection collapses rather than jumping a further word)"
        );
    }

    /// GitHub issue #27: "double-click selects a word, triple-click selects a line" - driven
    /// through real `MouseDownEvent`s with a real, non-1 `click_count`
    /// (`vendor` GPUI's own real click-count field, not a hand-rolled double-click timer this
    /// app would otherwise need), matching `clicking_a_real_editable_row_places_the_real_cursor_
    /// without_panicking`'s own established real-click-simulation precedent.
    #[gpui::test]
    fn double_click_selects_the_real_word_and_triple_click_selects_the_real_line(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello world\nsecond line\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        let row_bounds = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's real text row should have painted real bounds");
        // Land inside "world" (not "hello") so a real double-click must select the *whole* real
        // word, not just extend from wherever the click's own x lands.
        let click_point = gpui::point(row_bounds.right() - gpui::px(10.0), row_bounds.center().y);

        cx.simulate_event(gpui::MouseDownEvent {
            position: click_point,
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });

        let selected_text = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).unwrap();
            buffer.content[buffer.selected_range.clone()].to_string()
        });
        assert_eq!(
            selected_text, "world",
            "a real double-click (click_count == 2) must select the whole real word under the \
             click, not just place a caret or select one character"
        );

        cx.simulate_event(gpui::MouseDownEvent {
            position: click_point,
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 3,
            first_mouse: false,
        });

        let selected_text = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).unwrap();
            buffer.content[buffer.selected_range.clone()].to_string()
        });
        assert_eq!(
            selected_text, "hello world",
            "a real triple-click (click_count == 3) must select the whole real line, not just \
             the one word a double-click would"
        );
    }

    /// GitHub issue #27: "selection survives scrolling with virtualized/windowed rendering - no
    /// dropped highlight on rows recycled out of view." A real regression risk in this app's own
    /// architecture: [`AdeApp::file_view_row_layout`] is pruned to only the currently *painted*
    /// range on every render (`crate::code_surface::file_view::AdeApp::render_file_view`'s own
    /// `.retain(...)`, matching `uniform_list`'s real virtualization), so a selection that lived
    /// only in some per-row cache keyed by that map could plausibly vanish once its row scrolls
    /// out and back in. It doesn't: [`EditBuffer::selected_range`] is the one real source of
    /// truth every row's [`EditBuffer::selection_within_line`] is derived from fresh on every
    /// single render, not cached per-row at all - this test proves that structurally, by
    /// selecting text, forcing far-scroll-away-and-back (two real render passes with a distant
    /// [`Self::code_cursor`] each time, exactly what a real scroll would do to which rows get
    /// painted), and confirming the real selection is still exactly what it was.
    #[gpui::test]
    fn selection_survives_a_row_scrolling_out_of_the_virtualized_range_and_back(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let many_lines: String = (0..500).map(|n| format!("line {n}\n")).collect();
        let file_path = write_file(repo.path(), "sample.txt", &many_lines);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        // A real selection on line 1 (offsets 0..4, "line").
        app.update(cx, |app, cx| {
            let buffer = app.edit_buffers.get_mut(&relative).unwrap();
            buffer.move_to(0);
            buffer.select_to(4);
            cx.notify();
        });
        app.update(cx, |app, cx| app.render_center_pane(cx));

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .selection_within_line(0)),
            Some(0..4),
            "sanity check: the real selection should be visible on line 1 before any scrolling"
        );

        // Force line 1's own row out of the painted range by scrolling
        // `file_view_scroll_handle` directly - the real, underlying mechanism
        // `Self::sync_cursor_and_scroll` itself drives (`UniformListScrollHandle::
        // scroll_to_item`), used directly here rather than through a cursor move: a real mouse-
        // wheel scroll doesn't touch the caret/selection at all, and going through `EditBuffer::
        // move_to` here would collapse the very selection this test exists to prove survives -
        // `move_to`'s own docs are explicit that it clears the selection, which is real,
        // correct behavior for a cursor move, just not what this test is about.
        app.update(cx, |app, cx| {
            app.file_view_scroll_handle
                .scroll_to_item(400, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        app.update(cx, |app, cx| app.render_center_pane(cx));
        cx.run_until_parked();
        app.update(cx, |app, cx| app.render_center_pane(cx));

        assert!(
            cx.debug_bounds("file-view-text-row-1").is_none(),
            "sanity check: line 1's own row must genuinely not be in the painted range this far \
             from the real scroll position - otherwise this test isn't proving anything"
        );

        // Scroll back - line 1 is repainted (a real, freshly-built row, not reused state).
        app.update(cx, |app, cx| {
            app.file_view_scroll_handle
                .scroll_to_item(0, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        app.update(cx, |app, cx| app.render_center_pane(cx));
        cx.run_until_parked();
        app.update(cx, |app, cx| app.render_center_pane(cx));

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .selection_within_line(0)),
            Some(0..4),
            "the real selection must survive a row being scrolled out of the virtualized range \
             and back - it must not have been silently dropped by whatever pruned \
             `file_view_row_layout` while the row was out of view"
        );
    }

    /// Multi-cursor (Revision R13, issue #28): `Ctrl+D` through the real, bound
    /// `EditorSelectNextOccurrence` keystroke - first press selects the real word under the
    /// caret, second press adds the next real occurrence as a new cursor, and typing afterward
    /// (through the real `EntityInputHandler::replace_text_in_range` path, not a direct
    /// `EditBuffer` call) lands at *both* cursors at once.
    #[gpui::test]
    fn ctrl_d_through_real_key_bindings_adds_a_cursor_and_typing_fans_out_to_both(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "value + value\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
            cx.notify();
        });

        cx.simulate_keystrokes("ctrl-d");
        let after_first = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .unwrap()
                .selected_range
                .clone()
        });
        assert_eq!(
            after_first,
            0..5,
            "the first Ctrl+D should select the real word (\"value\") under the caret"
        );

        cx.simulate_keystrokes("ctrl-d");
        let cursor_count = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().cursor_count()
        });
        assert_eq!(
            cursor_count, 2,
            "the second Ctrl+D should add the next real occurrence as a new cursor"
        );

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "x", window, cx);
        });
        let content = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().content.clone()
        });
        assert_eq!(
            content, "x + x\n",
            "typing after Ctrl+D must land at every real cursor at once, through the real \
             EntityInputHandler path"
        );
    }

    /// Multi-cursor (Revision R13, issue #28): `Ctrl+Shift+L` through the real, bound
    /// `EditorSelectAllOccurrences` keystroke selects every real occurrence at once.
    #[gpui::test]
    fn ctrl_shift_l_through_real_key_bindings_selects_every_occurrence(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "value + value + value\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
            cx.notify();
        });

        cx.simulate_keystrokes("ctrl-shift-l");

        let cursor_count = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().cursor_count()
        });
        assert_eq!(
            cursor_count, 3,
            "every real occurrence of \"value\" should get a cursor"
        );
    }

    /// Multi-cursor (Revision R13, issue #28): `Ctrl+K Ctrl+D` (a real, space-separated chord
    /// binding) through the real, bound `EditorSkipOccurrence` keystroke skips the current
    /// occurrence rather than keeping it selected.
    #[gpui::test]
    fn ctrl_k_ctrl_d_through_real_key_bindings_skips_without_adding_a_cursor(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "value + value\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
            cx.notify();
        });
        cx.simulate_keystrokes("ctrl-d"); // selects the first "value"

        cx.simulate_keystrokes("ctrl-k ctrl-d");

        let (cursor_count, selected) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).unwrap();
            (buffer.cursor_count(), buffer.selected_range.clone())
        });
        assert_eq!(cursor_count, 1, "skip must not add a cursor");
        assert_eq!(selected, 8..13, "skip should move to the second \"value\"");
    }

    /// Multi-cursor (Revision R13, issue #28): `Esc` through the real, bound
    /// `EditorCollapseCursors` keystroke collapses back to a single cursor.
    #[gpui::test]
    fn escape_through_real_key_bindings_collapses_multi_cursor_state(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "value + value\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
            cx.notify();
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.simulate_keystrokes("ctrl-d");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .cursor_count()),
            2
        );

        cx.simulate_keystrokes("escape");

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .cursor_count()),
            1,
            "Escape should collapse back to a single real cursor"
        );
    }

    /// Real test (d): real Backspace/Delete through the real, bound key bindings.
    #[gpui::test]
    fn backspace_and_delete_remove_real_text_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "abc\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(2);
            cx.notify();
        });

        cx.simulate_keystrokes("backspace");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .content
                .clone()),
            "ac\n"
        );

        cx.simulate_keystrokes("delete");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .content
                .clone()),
            "a\n"
        );
    }

    /// Real test (e): explicit save actually writes real content to a real temp file and clears
    /// the real dirty flag - driven through the real, bound `EditorSave` keystroke.
    #[gpui::test]
    fn editor_save_writes_real_content_to_disk_and_clears_the_dirty_flag(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "well ", window, cx);
        });
        assert!(app.read_with(cx, |app, _| app
            .edit_buffers
            .get(&relative)
            .unwrap()
            .is_dirty()));

        let secondary_s = if cfg!(target_os = "macos") {
            "cmd-s"
        } else {
            "ctrl-s"
        };
        cx.simulate_keystrokes(secondary_s);
        cx.run_until_parked();

        let on_disk = std::fs::read_to_string(&file_path).expect("read back");
        assert_eq!(
            on_disk, "well hello\n",
            "the real file on disk should hold the real edit"
        );
        assert!(
            !app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .is_dirty()),
            "the dirty flag should be cleared by a successful real save"
        );
        assert!(app.read_with(cx, |app, _| app.file_save_error.is_none()));
    }

    /// Real test (f): the external-change-vs-unsaved-edit conflict from point 8 - a real,
    /// constructed scenario (dirty buffer, then a real out-of-band rewrite of the same file on
    /// disk, matching `code_view_cache_tests::a_real_on_disk_change_to_the_open_file_invalidates_
    /// the_cache`'s own precedent for simulating an external change). Confirms the app neither
    /// silently reloads over the unsaved edit nor lets a save silently overwrite the external
    /// change.
    #[gpui::test]
    fn external_change_while_dirty_is_detected_and_blocks_save_without_overwriting_either_version(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "original\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "edited ", window, cx);
        });
        assert!(app.read_with(cx, |app, _| app
            .edit_buffers
            .get(&relative)
            .unwrap()
            .is_dirty()));

        // A real external change, bypassing this app entirely.
        std::fs::write(&file_path, "changed on disk, a completely different size\n")
            .expect("external rewrite");
        std::thread::sleep(crate::root::FILE_FRESHNESS_CHECK_INTERVAL + Duration::from_millis(50));

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        assert!(
            app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)),
            "a real external change to a dirty file's real bytes must be detected"
        );

        app.update(cx, |app, cx| {
            app.save_active_file(cx);
        });
        cx.run_until_parked();

        let on_disk = std::fs::read_to_string(&file_path).expect("read back");
        assert_eq!(
            on_disk, "changed on disk, a completely different size\n",
            "save must refuse rather than silently overwrite the real external change"
        );
        assert!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .is_dirty()),
            "the user's own real unsaved edit must not have been silently discarded either"
        );
        assert!(app.read_with(cx, |app, _| app.file_save_error.is_some()));
    }

    /// The other half of test (f): a save that follows a load with no external interference must
    /// not be falsely flagged as a conflict - a real, non-vacuous negative case.
    #[gpui::test]
    fn a_save_with_no_external_interference_is_not_falsely_flagged_as_a_conflict(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "well ", window, cx);
        });

        app.update(cx, |app, cx| {
            app.save_active_file(cx);
        });
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.file_save_error.is_none()));
        assert!(!app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)));
        assert!(!app.read_with(cx, |app, _| app
            .edit_buffers
            .get(&relative)
            .unwrap()
            .is_dirty()));
        let on_disk = std::fs::read_to_string(&file_path).expect("read back");
        assert_eq!(on_disk, "well hello\n");
    }

    /// Regression coverage for a real bug caught while writing test (f) above: an earlier version
    /// of the conflict check was based on `AdeApp::file_view_cache`'s own freshness, which
    /// `spawn_file_load` always catches up to match disk regardless of whether the edit buffer is
    /// dirty - so the conflict banner would silently clear itself a `FILE_FRESHNESS_CHECK_INTERVAL`
    /// or two after the external change, even though the buffer was still genuinely dirty and
    /// still didn't match the new disk content. This drives *two* full freshness-check intervals
    /// (enough for `file_view_cache` to have already caught up after the first) and asserts the
    /// conflict is still reported on the second.
    #[gpui::test]
    fn the_conflict_flag_does_not_self_clear_once_file_view_cache_catches_up_while_still_dirty(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "original\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "edited ", window, cx);
        });

        std::fs::write(&file_path, "changed on disk\n").expect("external rewrite");
        std::thread::sleep(crate::root::FILE_FRESHNESS_CHECK_INTERVAL + Duration::from_millis(50));

        // First real freshness check: detects the conflict, and dispatches (then resolves) a
        // real background reload that catches `file_view_cache` up to the new disk content.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)));

        // A second full throttle interval, with the buffer still dirty and disk still not
        // matching the buffer's own load-time snapshot - `file_view_cache` is now fresh (it
        // caught up above), but the real conflict has not actually been resolved.
        std::thread::sleep(crate::root::FILE_FRESHNESS_CHECK_INTERVAL + Duration::from_millis(50));
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        assert!(
            app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)),
            "the conflict must still be reported - the buffer is still dirty and still doesn't \
             match the real, current on-disk content, even though file_view_cache has since \
             caught up for diagnostics purposes"
        );
    }

    /// Real IME coverage: GPUI's test harness (`vendor/zed/crates/gpui/src/platform/test/
    /// window.rs`) has no simulated OS-level IME composition event to drive - only a real
    /// `PlatformInputHandler`/`take_input_handler` plumbing point real platform backends use.
    /// This calls `EntityInputHandler::replace_and_mark_text_in_range`/`unmark_text` directly,
    /// exactly the way GPUI's real platform layer would (per that trait's own documented
    /// contract), rather than claiming an end-to-end simulated IME keystroke that doesn't
    /// actually exist in this GPUI version's test support.
    #[gpui::test]
    fn replace_and_mark_text_in_range_records_a_real_composition_and_unmark_clears_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "ab\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
            cx.notify();
        });

        app.update_in(cx, |app, window, cx| {
            app.replace_and_mark_text_in_range(None, "n", None, window, cx);
        });

        let (content, marked) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).unwrap();
            (buffer.content.clone(), buffer.marked_range.clone())
        });
        assert_eq!(content, "anb\n");
        assert_eq!(
            marked,
            Some(1..2),
            "the composing \"n\" should be recorded as marked text"
        );

        app.update_in(cx, |app, window, cx| {
            app.unmark_text(window, cx);
        });
        let (content_after, marked_after) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).unwrap();
            (buffer.content.clone(), buffer.marked_range.clone())
        });
        assert_eq!(
            content_after, "anb\n",
            "unmarking alone must not remove the composed text"
        );
        assert_eq!(marked_after, None);
    }

    /// Correct interaction with the Diff view: `EditorLeft` etc. must not fire while the Diff
    /// view (not File view) has focus for the same open tab - the whole point of scoping these
    /// actions to the `"file-editor"` key context rather than binding them globally.
    #[gpui::test]
    fn editor_actions_are_a_safe_no_op_while_the_diff_view_is_active(cx: &mut TestAppContext) {
        fn git(dir: &std::path::Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("failed to spawn git");
            assert!(
                output.status.success(),
                "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
                args,
                dir,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        write_file(repo.path(), "sample.rs", "fn add() -> i32 {\n    1\n}\n");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        write_file(repo.path(), "sample.rs", "fn add() -> i32 {\n    2\n}\n");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("sample.rs"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff
        );

        // A direct dispatch of the real action (not a keystroke, which the Diff view's own
        // container never even binds - see `render_file_view`'s `"file-editor"` key_context) -
        // proves the handler itself is a safe, honest no-op rather than relying only on the
        // binding never firing.
        app.update_in(cx, |app, window, cx| {
            app.handle_editor_left_action(&EditorLeft, window, cx);
        });

        // Nothing should have panicked, and - the real, non-vacuous assertion this test's own
        // comment always intended (an earlier version of this test asserted on
        // `file_save_error`, a field `EditorLeft`'s handler never touches either way, so it
        // would have passed identically whether the Diff-view guard worked or not) - no stray
        // edit buffer state should exist for this file: the Diff view path never seeds one
        // (`render_diff_file_detail` is entirely separate from `render_file_view`), and a
        // buggy handler that *did* fire against the wrong file would show up here as a real,
        // unexpected `EditBuffer` entry.
        assert!(app.read_with(cx, |app, _| !app
            .edit_buffers
            .contains_key(&PathBuf::from("sample.rs"))));
        assert!(app.read_with(cx, |app, _| app.file_save_error.is_none()));
    }

    /// Regression coverage for two related real, live-reproduced bugs an audit caught in an
    /// earlier version of this row: (a) every real left-click on an editable row panicked
    /// (`row_entity.read(cx)` inside a `cx.listener` closure, where `AdeApp` is already leased by
    /// that same `cx.listener`'s own `entity.update()` - GPUI's `EntityMap::read` double-lease
    /// panic); (b) the editable row's real text used to be painted from a bare `gpui::canvas`
    /// alone, which contributes no intrinsic content size to GPUI's layout, so the row collapsed
    /// to a near-fixed handful of pixels regardless of the real line's length - real content
    /// past roughly the first character or two was never actually clickable. Fixed by (a)
    /// reading `this.edit_buffers` directly instead of a second entity handle, and (b) rendering
    /// the visible glyphs from real, content-sized `div`s (matching the read-only path) with the
    /// `canvas` demoted to an absolutely-positioned overlay - see `render_editable_file_view_line`'s
    /// own docs for both fixes in full.
    ///
    /// A later revision (this one) made every row `.w_full()` (see that call's own docs) to fix a
    /// *third* bug in the same family: a row that stayed content-sized could never be clicked
    /// anywhere past its own last glyph. That real behavior change is exactly what this test now
    /// measures instead of the old "longer content paints a wider row" assertion, which the fix
    /// makes structurally false (every row now paints the list's own full available width,
    /// regardless of content) - `clicking_past_the_end_of_a_short_line_places_the_cursor_at_the_
    /// end_of_that_line`, below, is what now covers the "real content past the first glyph is
    /// genuinely clickable" property this test used to be the one proving.
    #[gpui::test]
    fn clicking_a_real_editable_row_places_the_real_cursor_without_panicking(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(
            repo.path(),
            "sample.txt",
            "a short line\nworld, a longer real line of text\n",
        );
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        let short_row_bounds = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's real text row should have painted real bounds");
        let long_row_bounds = cx
            .debug_bounds("file-view-text-row-2")
            .expect("line 2's real text row should have painted real bounds");
        assert_eq!(
            short_row_bounds.size.width, long_row_bounds.size.width,
            "every row now spans the list's real full available width regardless of its own \
             content - a short line's row and a long line's row must paint the exact same real \
             width (short: {:?}, long: {:?})",
            short_row_bounds.size.width, long_row_bounds.size.width
        );

        let row_bounds = long_row_bounds;
        assert!(
            row_bounds.size.width > gpui::px(100.0),
            "the real text row must not be collapsed to a near-zero width - measured {:?}",
            row_bounds.size.width
        );

        cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let (cursor_line, selected_range) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            (
                buffer.line_col_for_offset(buffer.cursor_offset()).0,
                buffer.selected_range.clone(),
            )
        });
        assert_eq!(
            cursor_line, 1,
            "clicking inside line 2's own real row should place the real caret on that line, \
             not panic or leave it on line 1"
        );
        assert!(
            selected_range.is_empty(),
            "an ordinary click (no shift) should place a caret, not a selection"
        );
        assert_eq!(app.read_with(cx, |app, _| app.code_cursor), Some(2));
    }

    /// Real regression coverage for real bug 1 in this revision's brief: "click-to-place-cursor
    /// only works on existing text, not anywhere in the editor". Before the `.w_full()` fix (see
    /// that call's own docs, in `render_editable_file_view_line`), a short line's row painted
    /// only as wide as its own glyphs, so a click landing in the real, visible blank space to the
    /// right of a short line's text had no element under it at all - the click was silently
    /// swallowed, nothing moved. `gpui::LineLayout::closest_index_for_x` (which the row's own
    /// `on_mouse_down` already calls) has always correctly clamped an out-of-range `x` to the
    /// shaped line's own real length; the missing piece was purely that the row never extended
    /// far enough to receive that click in the first place.
    #[gpui::test]
    fn clicking_past_the_end_of_a_short_line_places_the_cursor_at_the_end_of_that_line(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(
            repo.path(),
            "sample.txt",
            "a short line\nworld, a longer real line of text that runs on for a while\n",
        );
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        let short_row_bounds = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's real text row should have painted real bounds");

        // Clearly past "a short line"'s own 12 real glyphs (the row is now real full width, per
        // the fix above) - a point an old, content-sized row would never have painted an element
        // under at all.
        let click_point = gpui::point(
            short_row_bounds.right() - gpui::px(4.0),
            short_row_bounds.center().y,
        );
        cx.simulate_click(click_point, gpui::Modifiers::none());
        cx.run_until_parked();

        let (cursor_line, cursor_col) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            buffer.line_col_for_offset(buffer.cursor_offset())
        });
        assert_eq!(
            cursor_line, 0,
            "the click was on line 1's own row - the real caret must land on line 1, not be \
             silently swallowed or misplaced onto another line"
        );
        assert_eq!(
            cursor_col,
            "a short line".len(),
            "clicking past a short line's own last glyph must clamp the real caret to that \
             line's real end, not leave it at column 0 (which a swallowed/no-op click would look \
             like) or panic"
        );
        assert_eq!(app.read_with(cx, |app, _| app.code_cursor), Some(1));
    }

    /// Real regression coverage for the other half of real bug 1: clicking on a real blank line
    /// (no glyphs at all) must still place the real caret there, not silently do nothing. Before
    /// the `.w_full()` fix, an empty line's row had essentially zero content-derived width, so -
    /// like a short line's trailing blank space - there was no real element for a click landing
    /// anywhere but the very first pixel column to hit-test against.
    #[gpui::test]
    fn clicking_on_a_real_blank_line_places_the_cursor_there(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "first\n\nthird\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        let blank_row_bounds = cx
            .debug_bounds("file-view-text-row-2")
            .expect("the real blank line 2 must still paint a real, clickable row");
        assert!(
            blank_row_bounds.size.width > gpui::px(100.0),
            "a blank line's row must still span the row's real full available width, not \
             collapse to near-zero - measured {:?}",
            blank_row_bounds.size.width
        );

        cx.simulate_click(blank_row_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let (cursor_line, cursor_col) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            buffer.line_col_for_offset(buffer.cursor_offset())
        });
        assert_eq!(
            cursor_line, 1,
            "clicking the real blank second line must place the real caret on that line"
        );
        assert_eq!(cursor_col, 0, "a blank line's only real valid column is 0");
        assert_eq!(app.read_with(cx, |app, _| app.code_cursor), Some(2));
    }

    /// Real regression coverage for the other real symptom bug 1's brief calls out: clicking in
    /// the real blank area *below* the last line of content (still inside the File view's own
    /// viewport, just past every real rendered row) must place the real caret at the real end of
    /// the buffer, matching every real code editor, instead of being silently swallowed because
    /// `uniform_list` only ever paints rows for real line indices.
    #[gpui::test]
    fn clicking_below_the_last_line_places_the_cursor_at_the_end_of_the_buffer(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "one\ntwo\nthree");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        let last_row_bounds = cx
            .debug_bounds("file-view-text-row-3")
            .expect("the real last line's row should have painted real bounds");
        let list_bounds = cx
            .debug_bounds("file-view-code-list")
            .expect("the real code list container should have painted real bounds");

        // Real, genuinely below every rendered row, but still real, live-clickable space inside
        // the code list's own real viewport - a real editor's own tail-of-buffer clickable area.
        let click_point = gpui::point(
            last_row_bounds.center().x,
            (last_row_bounds.bottom() + gpui::px(20.0)).min(list_bounds.bottom() - gpui::px(1.0)),
        );
        cx.simulate_click(click_point, gpui::Modifiers::none());
        cx.run_until_parked();

        let expected_end = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            buffer.content.len()
        });
        let (cursor_offset, selected_range) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).expect("buffer");
            (buffer.cursor_offset(), buffer.selected_range.clone())
        });
        assert_eq!(
            cursor_offset, expected_end,
            "clicking below the last line must place the real caret at the real end of the \
             buffer, not leave it wherever it was (a swallowed click) or panic"
        );
        assert!(
            selected_range.is_empty(),
            "an ordinary click below the content (no shift) must place a caret, not a selection"
        );
        assert_eq!(app.read_with(cx, |app, _| app.code_cursor), Some(3));
    }

    /// Regression coverage for a real bug an audit caught: seeding an editable buffer from a
    /// file whose real bytes aren't valid UTF-8 (`code_view::load_file_with_source`'s `source` is
    /// already a lossy decode with every invalid byte replaced by `U+FFFD` at that point) and
    /// later saving it would silently rewrite the file's real original bytes with replacement
    /// characters. Such a file must stay read-only, the same real reasoning already applied to a
    /// truncated file.
    #[gpui::test]
    fn a_file_with_invalid_utf8_bytes_does_not_get_a_real_edit_buffer(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("binary.txt");
        // A lone UTF-8 continuation byte (0x80) is never valid on its own - real, genuinely
        // invalid UTF-8, not a contrived edge case.
        std::fs::write(&file_path, [b'h', b'i', 0x80, b'\n']).expect("write invalid-utf8 file");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("binary.txt");

        assert!(
            app.read_with(cx, |app, _| app
                .file_view_cache
                .as_ref()
                .is_some_and(|parsed| !parsed.is_valid_utf8)),
            "the file should have really loaded (read-only) with its real invalid-UTF-8 flag set"
        );
        assert!(
            app.read_with(cx, |app, _| !app.edit_buffers.contains_key(&relative)),
            "a file whose real bytes aren't valid UTF-8 must not get a real edit buffer - saving \
             one would silently corrupt the file with U+FFFD replacement characters"
        );
    }

    /// Regression coverage for a real, live-reproduced bug an audit caught: once
    /// `AdeApp::file_external_conflict` was set for a path, nothing but a *successful* save ever
    /// cleared it, and `AdeApp::save_active_file`'s own freshness gate could never pass again
    /// after any real external touch (even reverting the file back to byte-identical content
    /// still changes its real mtime) - so the file became permanently unsavable for the rest of
    /// the agent, with no real way out. `EditorSaveAnyway`/`AdeApp::force_save_active_file` is
    /// the real, explicit, opt-in escape hatch this fixes it with.
    #[gpui::test]
    fn editor_save_anyway_resolves_a_real_permanently_stuck_conflict(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "original\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "edited ", window, cx);
        });

        std::fs::write(&file_path, "changed on disk\n").expect("external rewrite");
        std::thread::sleep(crate::root::FILE_FRESHNESS_CHECK_INTERVAL + Duration::from_millis(50));
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)));

        // An ordinary save must still refuse - confirms the conflict is genuinely still active,
        // not something a prior fix round already cleared by accident.
        app.update(cx, |app, cx| {
            app.save_active_file(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read back"),
            "changed on disk\n",
            "an ordinary save must still refuse to overwrite the real external change"
        );

        // The real, explicit override - driven through the real, bound `secondary-shift-s`
        // keystroke, not a direct method call, so this also proves the real keybinding wiring.
        let secondary_shift_s = if cfg!(target_os = "macos") {
            "cmd-shift-s"
        } else {
            "ctrl-shift-s"
        };
        cx.simulate_keystrokes(secondary_shift_s);
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read back"),
            "edited original\n",
            "the real, explicit override should overwrite the real external change with the \
             user's own edits"
        );
        assert!(
            !app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .is_dirty()),
            "a successful force-save should clear the real dirty flag"
        );
        assert!(
            !app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)),
            "a successful force-save should clear the real conflict flag - the file is no \
             longer stuck"
        );
        assert!(app.read_with(cx, |app, _| app.file_save_error.is_none()));

        // Confirms the conflict is *genuinely* resolved, not just cleared once: a further
        // ordinary save (no new external interference) must now succeed normally.
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "more ", window, cx);
        });
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-s"
        } else {
            "ctrl-s"
        });
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read back"),
            // The caret sits right after "edited " (position 7) from the earlier edit - the
            // force-save above only wrote to disk, it didn't move the real caret.
            "edited more original\n"
        );
        assert!(app.read_with(cx, |app, _| app.file_save_error.is_none()));
    }

    /// Regression coverage (finding 7, low priority): `EditorSaveAnyway`/
    /// `AdeApp::force_save_active_file` must be a genuine no-op on an already-clean buffer -
    /// without a real dirty-check guard, triggering it there would perform a needless real disk
    /// write and bump the file's real mtime for no reason.
    #[gpui::test]
    fn force_save_active_file_is_a_real_no_op_on_an_already_clean_buffer(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        assert!(
            !app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .is_dirty()),
            "a freshly-opened buffer should start clean"
        );
        let mtime_before = std::fs::metadata(&file_path)
            .expect("stat")
            .modified()
            .expect("real mtime");

        app.update(cx, |app, cx| {
            app.force_save_active_file(cx);
        });
        cx.run_until_parked();

        let mtime_after = std::fs::metadata(&file_path)
            .expect("stat")
            .modified()
            .expect("real mtime");
        assert_eq!(
            mtime_before, mtime_after,
            "force-saving an already-clean buffer must not perform a real, needless disk write"
        );
    }

    /// Regression coverage (finding 6): the real, if previously only latent, bug where
    /// `file_save_running` could leak forever for a path whose buffer vanished after a save was
    /// enqueued but before the writer loop got to check it - see `AdeApp::spawn_file_save_loop`'s
    /// own docs for the full real, silent-permanent-failure reasoning this fixes. Removes the
    /// `edit_buffers` entry directly to simulate the buffer vanishing mid-flight (the loop task is
    /// spawned, but hasn't run yet - `cx.spawn` doesn't poll until the executor is next run) - a
    /// real, if synthetic, reproduction of the exact race the audit described, since this app has
    /// no other real way to force that interleaving deterministically.
    #[gpui::test]
    fn a_pending_save_whose_buffer_vanished_before_the_writer_loop_checked_it_does_not_leak_file_save_running(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "hello\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "well ", window, cx);
        });

        app.update(cx, |app, cx| {
            app.save_active_file(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.file_save_running.contains(&relative)),
            "the real writer loop should be marked alive immediately"
        );

        // The buffer vanishes before the (already-spawned, not-yet-polled) writer loop task gets
        // to check it.
        app.update(cx, |app, _cx| {
            app.edit_buffers.remove(&relative);
        });
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.file_save_running.contains(&relative)),
            "the real writer loop must clear file_save_running on every real exit path, not \
             just the one branch that used to - otherwise this path becomes permanently unsavable"
        );

        // Confirms it's genuinely fixed, not just cleared once: force a real fresh reload (the
        // cached `file_view_cache` is otherwise still fresh - nothing ever actually hit disk -
        // so `render_file_view` would not re-seed `edit_buffers` on its own) and prove an
        // ordinary save on the same path now works normally.
        app.update(cx, |app, _cx| {
            app.file_view_cache = None;
        });
        open_file_for_editing(&app, cx, file_path.clone());
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "second ", window, cx);
        });
        app.update(cx, |app, cx| {
            app.save_active_file(cx);
        });
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read back"),
            "second hello\n",
            "a fresh save for the same real path must actually succeed, proving \
             file_save_running wasn't left permanently stuck for it"
        );
        assert!(app.read_with(cx, |app, _| app.file_save_error.is_none()));
    }

    /// CRITICAL regression coverage (finding 1): the real, live-reproduced panic an audit caught
    /// with real Japanese IME composition input, driven through the real `EntityInputHandler`
    /// trait methods (not just `EditBuffer` directly - see `edit_buffer::tests::
    /// replace_and_mark_range_computes_the_composition_selection_relative_to_new_text_not_the_whole_buffer`
    /// for that lower-level regression). `new_selected_range_utf16` is a real, non-`None` value
    /// here (unlike `replace_and_mark_text_in_range_records_a_real_composition_and_unmark_clears_it`
    /// above, whose `None` argument can never exercise this bug) - a real platform IME reporting a
    /// composing caret at a UTF-16 offset *within the composing text itself*, not the whole buffer.
    #[gpui::test]
    fn real_cjk_ime_composition_with_a_non_default_caret_does_not_corrupt_the_selection_or_panic_on_the_next_keystroke(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "prefix ok\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        let relative = PathBuf::from("sample.txt");

        app.update(cx, |app, cx| {
            app.edit_buffers
                .get_mut(&relative)
                .unwrap()
                .move_to("prefix ".len());
            cx.notify();
        });

        // Composing "\u{65e5}\u{672c}\u{8a9e}" with the real IME reporting a caret 2 UTF-16 units
        // into the composing text itself (after "\u{65e5}\u{672c}").
        app.update_in(cx, |app, window, cx| {
            app.replace_and_mark_text_in_range(
                None,
                "\u{65e5}\u{672c}\u{8a9e}",
                Some(2..2),
                window,
                cx,
            );
        });

        let (content, selected_range) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffers.get(&relative).unwrap();
            (buffer.content.clone(), buffer.selected_range.clone())
        });
        assert_eq!(content, "prefix \u{65e5}\u{672c}\u{8a9e}ok\n");
        let expected_offset = "prefix ".len() + "\u{65e5}\u{672c}".len();
        assert_eq!(
            selected_range,
            expected_offset..expected_offset,
            "the composing caret must land 2 real chars into the composing text itself, not be \
             misread as an offset into the whole buffer"
        );

        // The real crash this fix addresses: a further real keystroke right after this
        // composition update must not panic - the old formula could leave `selected_range` on a
        // non-UTF-8-char-boundary byte offset, which panicked the very next real edit.
        app.update_in(cx, |app, window, cx| {
            app.unmark_text(window, cx);
            app.replace_text_in_range(None, "!", window, cx);
        });
        let content_after = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().content.clone()
        });
        assert_eq!(content_after, "prefix \u{65e5}\u{672c}!\u{8a9e}ok\n");
    }

    /// CRITICAL regression coverage (finding 2): the real, live-reproduced bug where a literal
    /// `]` keystroke typed while the File view is actively being edited was silently swallowed
    /// by the pre-existing, unrelated `]` -> `NextChangedFile` binding instead of reaching the
    /// real edit buffer - the fourth time this project has shipped this exact "silently swallowed
    /// keystroke" bug class (Revisions R2, R4a, R4b, and this one). Fixed by changing that
    /// binding's own registered predicate from `Some("diff")` to `Some("diff && !file-editor")`
    /// (`crate::default_key_bindings`) - driven here through a real, simulated keystroke (not a
    /// direct method call, and not `dispatch_action`, which would validate the handler but not
    /// prove the *binding* itself no longer intercepts the keystroke first), matching this
    /// project's own established lesson that only a real keystroke-simulation test actually
    /// catches this bug class.
    #[gpui::test]
    fn a_literal_right_bracket_keystroke_reaches_the_real_edit_buffer_while_the_file_view_is_editing(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.rs", "abc\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.rs");

        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(3);
            cx.notify();
        });

        cx.simulate_keystrokes("]");

        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .content
                .clone()),
            "abc]\n",
            "a literal ] keystroke must reach and be inserted into the real edit buffer while \
             the File view is in editing mode, not be swallowed by the unrelated \
             NextChangedFile binding"
        );
    }

    /// Regression coverage for the keybinding-collision bug class this project has now shipped
    /// four times (R2, R4a, R4b, R8.5a) - Revision R8.5b's own instance, for the real Completions
    /// popup's `Up`/`Down`/`Enter`/`Escape` bindings. Two real, keystroke-simulated states, not
    /// just one: with the popup closed, `up`/`down`/`enter` must still behave exactly as the
    /// plain `Editor*` actions always have (the real regression risk from narrowing their own
    /// predicate to `!completions`); with the popup genuinely open, the same keystrokes must
    /// route to the popup instead, never touching the real caret/buffer, and `Escape` must
    /// dismiss it. The popup state itself is seeded directly (a real `Ready` `CompletionsEntry`,
    /// not a real LSP round trip) - matching `crate::lsp::client::lsp_client_eviction_tests`' own
    /// established precedent for isolating real routing/bookkeeping proofs from a real process
    /// spawn; the real end-to-end proof (a genuine server's completions actually opening this
    /// same popup) lives in `crate::lsp::client::lsp_diagnostics_wiring_tests`.
    #[gpui::test]
    fn completions_keybindings_are_correctly_scoped_in_both_the_open_and_closed_state(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.rs", "ab\ncd\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.rs");

        // State 1: no popup open - `up`/`down`/`enter` must behave exactly like the plain
        // editor actions always have.
        app.update(cx, |app, cx| {
            app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
            cx.notify();
        });
        cx.simulate_keystrokes("down");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .cursor_offset()),
            4,
            "with no popup open, `down` must still move the real caret to line 2 exactly as \
             before"
        );
        cx.simulate_keystrokes("enter");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .content
                .clone()),
            "ab\nc\nd\n",
            "with no popup open, `enter` must still insert a real newline at the real caret \
             exactly as before"
        );

        // State 2: a real, seeded `Ready` popup.
        let fake_item = |label: &str| lsp_core::lsp_types::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        };
        app.update(cx, |app, cx| {
            app.completions = Some(crate::lsp::completion_popup::CompletionsEntry {
                path: relative.clone(),
                status: crate::lsp::completion_popup::CompletionsStatus::Ready {
                    items: vec![fake_item("alpha"), fake_item("beta")],
                    selected: 0,
                },
            });
            cx.notify();
        });

        let cursor_before = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().cursor_offset()
        });
        let content_before = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().content.clone()
        });
        cx.simulate_keystrokes("down");
        assert_eq!(
            app.read_with(cx, |app, _| {
                let entry = app
                    .completions
                    .as_ref()
                    .expect("popup should still be open");
                match &entry.status {
                    crate::lsp::completion_popup::CompletionsStatus::Ready { selected, .. } => {
                        *selected
                    }
                    _ => panic!("expected Ready"),
                }
            }),
            1,
            "with the popup open, `down` must move its own real selection, not the real caret"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .cursor_offset()),
            cursor_before,
            "the real caret must not have moved while the popup owns `down`"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .content
                .clone()),
            content_before,
            "the real buffer content must be untouched while the popup owns `down`"
        );

        cx.simulate_keystrokes("enter");
        let content_after_enter = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().content.clone()
        });
        assert!(
            content_after_enter.contains("beta"),
            "with the popup open, `enter` must accept the real selected item (\"beta\", index \
             1 after the real `down` above), not insert a real newline - got: \
             {content_after_enter:?}"
        );
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "accepting must close the real popup"
        );

        // A fresh popup, dismissed by a real `Escape` keystroke without touching the buffer.
        app.update(cx, |app, cx| {
            app.completions = Some(crate::lsp::completion_popup::CompletionsEntry {
                path: relative.clone(),
                status: crate::lsp::completion_popup::CompletionsStatus::Ready {
                    items: vec![fake_item("gamma")],
                    selected: 0,
                },
            });
            cx.notify();
        });
        let content_before_escape = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().content.clone()
        });
        cx.simulate_keystrokes("escape");
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "a real Escape keystroke must dismiss the real popup"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .content
                .clone()),
            content_before_escape,
            "dismissing via Escape must not touch the real buffer content"
        );
    }

    /// Revision R8.5b audit finding 1's direct regression test: the sixth instance of this
    /// project's recurring "a keystroke gets swallowed" bug class. Before this fix, `Self::
    /// completions_open_for_active_path` returned `true` for *any* real [`CompletionsEntry`],
    /// including a merely `Loading`/`Failed` one - which `AdeApp::prepare_lsp_sync` seeds on
    /// *every* completion-worthy keystroke, before the real request even completes - so the real
    /// `"completions"` key context stayed active (claiming `Enter`/`Down`) for the *entire* real
    /// round trip a completion request takes, live-reproduced against a real rust-analyzer as:
    /// pressing Enter while a request was merely loading inserted no newline at all, and Down did
    /// nothing either. Verified here by simulating real keystrokes (not calling handlers
    /// directly) against a real, seeded `Loading` entry, then a real `Failed` one - both must
    /// fall all the way through to the plain `Editor*` behavior, exactly as if no popup existed.
    #[gpui::test]
    fn enter_and_down_are_not_swallowed_while_completions_are_merely_loading_or_failed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.rs", "ab\ncd\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.rs");

        for (label, status) in [
            (
                "Loading",
                crate::lsp::completion_popup::CompletionsStatus::Loading,
            ),
            (
                "Failed",
                crate::lsp::completion_popup::CompletionsStatus::Failed("timed out".to_string()),
            ),
        ] {
            app.update(cx, |app, cx| {
                app.edit_buffers.get_mut(&relative).unwrap().move_to(1);
                app.completions = Some(crate::lsp::completion_popup::CompletionsEntry {
                    path: relative.clone(),
                    status,
                });
                cx.notify();
            });

            // The real "completions" key context must not even be present while merely
            // `Loading`/`Failed` - `Self::completions_open_for_active_path` is the single real
            // predicate both the key context and the handlers' own defense-in-depth guards share.
            assert!(
                !app.read_with(cx, |app, _| app.completions_open_for_active_path()),
                "[{label}] a merely {label} entry must not report the popup as actionably open"
            );

            let content_before = app.read_with(cx, |app, _| {
                app.edit_buffers.get(&relative).unwrap().content.clone()
            });
            cx.simulate_keystrokes("enter");
            let content_after_enter = app.read_with(cx, |app, _| {
                app.edit_buffers.get(&relative).unwrap().content.clone()
            });
            assert_ne!(
                content_after_enter, content_before,
                "[{label}] a real Enter keystroke while completions are merely {label} must \
                 still insert a real newline - it must not be silently swallowed"
            );
            assert!(
                content_after_enter.len() == content_before.len() + 1
                    && content_after_enter.contains('\n'),
                "[{label}] expected exactly one real newline to have been inserted, got: \
                 {content_before:?} -> {content_after_enter:?}"
            );

            // A real Down keystroke must genuinely move the real caret, not silently no-op -
            // re-seed a fresh `Loading`/`Failed` entry (the Enter above already dismissed the
            // previous one via `Self::move_active_buffer`'s own caret-move dismissal).
            app.update(cx, |app, cx| {
                let buffer = app.edit_buffers.get_mut(&relative).unwrap();
                buffer.move_to(1);
                let status = match label {
                    "Loading" => crate::lsp::completion_popup::CompletionsStatus::Loading,
                    _ => crate::lsp::completion_popup::CompletionsStatus::Failed(
                        "timed out".to_string(),
                    ),
                };
                app.completions = Some(crate::lsp::completion_popup::CompletionsEntry {
                    path: relative.clone(),
                    status,
                });
                cx.notify();
            });
            let cursor_before_down = app.read_with(cx, |app, _| {
                app.edit_buffers.get(&relative).unwrap().cursor_offset()
            });
            cx.simulate_keystrokes("down");
            let cursor_after_down = app.read_with(cx, |app, _| {
                app.edit_buffers.get(&relative).unwrap().cursor_offset()
            });
            assert_ne!(
                cursor_after_down, cursor_before_down,
                "[{label}] a real Down keystroke while completions are merely {label} must \
                 still move the real caret, not silently do nothing"
            );
        }
    }

    // ------------------------------------------------------------------------------------------
    // GitHub issue #17 - real, dispatched undo/redo. These deliberately drive `simulate_keystrokes`
    // rather than calling the handlers directly: this project's own history is that
    // state-assertion-only tests miss real dispatch-routing bugs, and routing is exactly what is
    // at risk here (two distinct undo systems on one physical key).
    // ------------------------------------------------------------------------------------------

    /// `secondary-z`, resolved for the real build target - `crate::default_key_bindings`' own
    /// convention.
    const SECONDARY_Z: &str = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    const SECONDARY_SHIFT_Z: &str = if cfg!(target_os = "macos") {
        "cmd-shift-z"
    } else {
        "ctrl-shift-z"
    };

    fn buffer_content(
        app: &Entity<AdeApp>,
        cx: &gpui::VisualTestContext,
        relative: &Path,
    ) -> String {
        app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(relative)
                .expect("buffer should exist")
                .content
                .clone()
        })
    }

    /// The headline behaviour of GitHub issue #17 §2, end to end through the real key bindings:
    /// a real typing burst is one undo step, undo puts the real caret back, and redo replays both.
    #[gpui::test]
    fn a_real_typing_burst_undoes_and_redoes_as_one_step_through_the_real_key_bindings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "ab\ncd\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        cx.simulate_input("hello");
        assert_eq!(buffer_content(&app, cx, &relative), "helloab\ncd\n");
        let caret_after_typing = app.read_with(cx, |app, _| {
            app.edit_buffers.get(&relative).unwrap().cursor_offset()
        });
        assert_eq!(caret_after_typing, 5);

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            buffer_content(&app, cx, &relative),
            "ab\ncd\n",
            "a real secondary-z keystroke must undo the whole burst in one step"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .cursor_offset()),
            0,
            "and restore the real caret from before the burst"
        );

        cx.simulate_keystrokes(SECONDARY_SHIFT_Z);
        assert_eq!(buffer_content(&app, cx, &relative), "helloab\ncd\n");
        assert_eq!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .cursor_offset()),
            5,
            "redo must replay the real post-edit caret too"
        );
    }

    /// `ctrl-y` as a real, alternative redo key - GitHub issue #17's checklist asks for it
    /// explicitly, and it is a literal `Ctrl` on every OS (see `crate::default_key_bindings`).
    #[gpui::test]
    fn ctrl_y_really_redoes_in_the_code_editor(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "ab\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        cx.simulate_input("xy");
        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(buffer_content(&app, cx, &relative), "ab\n");
        cx.simulate_keystrokes("ctrl-y");
        assert_eq!(buffer_content(&app, cx, &relative), "xyab\n");
    }

    /// The central scoping guarantee of GitHub issue #17 §3, proven by dispatch rather than by
    /// reading the predicate: with a real file editor focused, `secondary-z` must reach **text**
    /// undo.
    #[gpui::test]
    fn secondary_z_in_the_code_editor_reaches_text_undo(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "ab\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        cx.simulate_input("z");
        cx.simulate_keystrokes(SECONDARY_Z);

        assert_eq!(
            buffer_content(&app, cx, &relative),
            "ab\n",
            "the text undo must genuinely have run"
        );

        // Same again for redo, in both of its real spellings.
        cx.simulate_keystrokes(SECONDARY_SHIFT_Z);
        cx.simulate_keystrokes("ctrl-y");
        assert_eq!(buffer_content(&app, cx, &relative), "zab\n");
    }

    /// The same routing guarantee with the real Completions popup **also** open - a real
    /// overlapping-scope case (`"file-editor text-input completions"` all live on one node at
    /// once) that the narrower `"file-editor && completions"` bindings share a dispatch path
    /// with.
    #[gpui::test]
    fn secondary_z_still_reaches_text_undo_while_the_completions_popup_is_open(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.rs", "ab\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.rs");

        cx.simulate_input("zz");
        assert_eq!(buffer_content(&app, cx, &relative), "zzab\n");

        // A real, seeded `Ready` popup - the same construction
        // `completions_keybindings_are_correctly_scoped_in_both_the_open_and_closed_state` uses.
        let fake_item = |label: &str| lsp_core::lsp_types::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        };
        app.update(cx, |app, cx| {
            app.completions = Some(crate::lsp::completion_popup::CompletionsEntry {
                path: relative.clone(),
                status: crate::lsp::completion_popup::CompletionsStatus::Ready {
                    items: vec![fake_item("alpha")],
                    selected: 0,
                },
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.completions_open_for_active_path()),
            "sanity check: the popup must genuinely be open, or this test proves nothing"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            buffer_content(&app, cx, &relative),
            "ab\n",
            "an open completions popup must not divert secondary-z away from text undo"
        );
    }

    /// GitHub issue #17 §2's "history survives switching tabs" requirement, driven through the
    /// real tab-activation path rather than by poking `edit_buffers` directly.
    #[gpui::test]
    fn a_files_undo_history_survives_switching_to_another_tab_and_back(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let first = write_file(repo.path(), "first.txt", "one\n");
        let second = write_file(repo.path(), "second.txt", "two\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, first.clone());
        bind_real_keys(cx);
        let first_relative = PathBuf::from("first.txt");

        cx.simulate_input("AAA");
        assert_eq!(buffer_content(&app, cx, &first_relative), "AAAone\n");

        // Switch to a genuinely different file, edit it too, then come back.
        open_file_for_editing(&app, cx, second.clone());
        cx.simulate_input("B");
        assert_eq!(
            buffer_content(&app, cx, &PathBuf::from("second.txt")),
            "Btwo\n"
        );
        open_file_for_editing(&app, cx, first.clone());
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(first_relative.clone()),
            "sanity check: the first file must really be the active tab again"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            buffer_content(&app, cx, &first_relative),
            "one\n",
            "the first file's own undo history must have survived the round trip through \
             another tab"
        );
        assert_eq!(
            buffer_content(&app, cx, &PathBuf::from("second.txt")),
            "Btwo\n",
            "and the other file's buffer must be untouched - undo is strictly per buffer"
        );
    }

    /// A real external rewrite of a **clean** buffer, landing through the real file-load path the
    /// freshness check dispatches - not a direct `reload_from_disk` call. The reload must be one
    /// real undo step with the pre-reload history still behind it.
    #[gpui::test]
    fn an_external_rewrite_of_a_clean_buffer_reloads_as_one_undoable_step(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "original\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        cx.simulate_input("X");
        assert_eq!(buffer_content(&app, cx, &relative), "Xoriginal\n");
        // Save, so the buffer is genuinely clean against disk before the external write.
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-s"
        } else {
            "ctrl-s"
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| !app
                .edit_buffers
                .get(&relative)
                .unwrap()
                .is_dirty()),
            "sanity check: the buffer must really be clean before the external rewrite"
        );

        // A real external writer rewrites the file, with a genuinely newer mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file_path, "rewritten by an agent\n").expect("external rewrite");
        // Force the throttled freshness check to run, then let the load it dispatches resolve.
        app.update(cx, |app, _| {
            app.file_view_last_freshness_check = None;
        });
        for _ in 0..3 {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
        }

        assert_eq!(
            buffer_content(&app, cx, &relative),
            "rewritten by an agent\n",
            "a clean buffer whose file was rewritten externally must adopt the new content - \
             showing bytes that no longer exist anywhere would be silently stale"
        );

        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            buffer_content(&app, cx, &relative),
            "Xoriginal\n",
            "the reload must be one real undoable step"
        );
        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            buffer_content(&app, cx, &relative),
            "original\n",
            "and the history recorded before the reload must still be there - never a silent \
             wipe mid-stack"
        );
    }

    /// The dirty half of the same case: an external rewrite while the user has real unsaved edits
    /// must leave the buffer *and* its history completely alone, and surface the real conflict
    /// instead.
    #[gpui::test]
    fn an_external_rewrite_of_a_dirty_buffer_leaves_the_buffer_and_its_history_untouched(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_file(repo.path(), "sample.txt", "original\n");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        open_file_for_editing(&app, cx, file_path.clone());
        bind_real_keys(cx);
        let relative = PathBuf::from("sample.txt");

        cx.simulate_input("MINE");
        assert!(app.read_with(cx, |app, _| app
            .edit_buffers
            .get(&relative)
            .unwrap()
            .is_dirty()));

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file_path, "theirs\n").expect("external rewrite");
        app.update(cx, |app, _| {
            app.file_view_last_freshness_check = None;
        });
        for _ in 0..3 {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
        }

        assert_eq!(
            buffer_content(&app, cx, &relative),
            "MINEoriginal\n",
            "a dirty buffer's unsaved content is the user's - an external rewrite must never \
             silently replace it"
        );
        assert!(
            app.read_with(cx, |app, _| app.file_external_conflict.contains(&relative)),
            "the real divergence must be surfaced as a conflict instead"
        );
        cx.simulate_keystrokes(SECONDARY_Z);
        assert_eq!(
            buffer_content(&app, cx, &relative),
            "original\n",
            "and the user's own undo history must be exactly as it was"
        );
    }
}
