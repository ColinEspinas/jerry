//! Surface C's real Completions popup (Revision R8.5b) - the GPUI-facing counterpart of
//! `crate::lsp::completion`'s pure trigger/insert-text logic, mirroring `crate::code_surface`'s
//! `HoverEntry`/`render_hover_card` split for the same reason: request dispatch/version/generation
//! bookkeeping lives in `crate::lsp::client` (alongside `AdeApp::schedule_lsp_sync`, which is what
//! actually decides when to open/refresh/close this popup), and this module owns the resulting
//! `AdeApp::completions` state, its keyboard-driven navigation/accept/dismiss actions, and its
//! real, cursor-anchored popover paint.
//!
//! ## Positioning: reusing Revision R8.5a's own real cursor pixel math
//!
//! The popover is anchored to the real caret's own painted position - [`AdeApp::file_view_last_bounds`]/
//! [`AdeApp::file_view_last_layout`]/[`AdeApp::file_view_last_layout_for`], the exact same real,
//! already-computed values `crate::code_surface::editing`'s `EntityInputHandler::bounds_for_range`/
//! `character_index_for_point` read (see that module's own docs) - never a second, independently
//! computed position. Painted as a top-level sibling in [`AdeApp::render`] (the same real
//! `.absolute()`-positioned-off-a-captured-`Bounds` idiom `crate::work_surface::render::
//! AdeApp::render_plus_menu` already establishes for this app's other floating popover, off
//! `AdeApp::plus_button_bounds`), not nested inside the File view's own `uniform_list` - a popup
//! anchored to one row must not be clipped by that row's own virtualized scroll container.
//!
//! ## Deliberately not on the shared menu chrome (GitHub issue #129)
//!
//! An audit for that issue flagged this popover's `theme::surface::POPOVER` background,
//! `theme::radius::CARD_SM` (5px) corner radius, and `theme::shadow::POPOVER` shadow as
//! inconsistent with the `theme::surface::PALETTE`/`theme::radius::CARD`/`theme::shadow::MENU`
//! recipe every dropdown/context-menu in the app now shares. They're not drift: each value is
//! pinned to `design_handoff_jerry_ade/Jerry.dc.html`'s own completions-popup markup (line ~406 -
//! `border-radius:5px;background:#181c20;box-shadow:0 8px 20px rgba(0,0,0,.5)`), which specifies
//! a genuinely different recipe from the app-level menus for this specific surface. Left
//! unchanged, along with `crate::code_surface::lsp_ui`'s hover card, which intentionally matches
//! it (same `theme::surface::POPOVER`/`theme::border::POPOVER` pair, plus every plain tooltip via
//! `crate::root::widgets::TextTooltip`) - all three are the "info popover" family, not the "menu"
//! family, and have no per-row hover at all (keyboard/passive, not mouse-driven).

use super::*;
use crate::code_surface::code_view;
use crate::code_surface::edit_buffer::EditBuffer;
use crate::keymap;
use crate::lsp::completion as completion_view;
use crate::root::widgets::{render_hint_pair, render_hint_row};
use crate::theme;
use gpui::{BoxShadow, EntityInputHandler};

/// The state of one in-flight or completed, keystroke-triggered `textDocument/completion`
/// request; see [`AdeApp::completions`]'s own docs for the caching/staleness discipline this
/// backs. Mirrors `crate::code_surface::state::HoverEntry`'s own shape.
#[derive(Debug, Clone)]
pub(crate) struct CompletionsEntry {
    /// Worktree-relative, matching [`AdeApp::edit_buffers`]'s own key convention -
    /// `crate::code_surface::render::AdeApp::render_code_surface`/[`AdeApp::render_completions_popover`]
    /// only show this popup while it matches the file actually on screen.
    pub path: PathBuf,
    pub status: CompletionsStatus,
}

/// The outcomes of one [`CompletionsEntry`]'s request - mirrors
/// `crate::code_surface::state::HoverStatus`'s own three-state shape.
#[derive(Debug, Clone)]
pub(crate) enum CompletionsStatus {
    Loading,
    /// A real, non-empty response arrived (an empty one is normalized to no popup at all at the
    /// point [`AdeApp::completions`] is written - see `crate::lsp::client::AdeApp::
    /// apply_completion_result`'s own docs - so this variant is never empty).
    Ready {
        items: Vec<lsp_core::lsp_types::CompletionItem>,
        selected: usize,
    },
    Failed(String),
}

/// A soft cap on how many real completion items this popup renders at once - this app has no
/// virtualized/scrollable popover widget (unlike the File view's own `uniform_list`), and a real
/// completion response can legitimately carry hundreds of items (e.g. an empty-prefix "complete
/// everything in scope" request) - rendering all of them would paint an enormous, unusable
/// column. The real, live-returned list is truncated for *display* only; `Self::move_completions_selection`/
/// `Self::accept_active_completion` still only ever act on what's actually shown, so a truncated
/// tail is never silently selectable via keyboard nav either.
const MAX_RENDERED_COMPLETION_ITEMS: usize = 12;

/// 290px - the design mockup's own real completions-list column width
/// (`design_handoff_jerry_ade/revision 3/Jerry.dc.html`: `width:290px` on the list column), plus
/// its own `1px` right divider. Design-review follow-up: this used to be the popup's *entire*
/// width, with the detail pane below left out of scope entirely (see [`DETAIL_WIDTH`]'s own docs
/// for why that was a real, addressable gap rather than a permanent one).
const LIST_WIDTH: gpui::Pixels = gpui::px(290.0);
/// 300px - the design mockup's own real signature/doc/module-path detail pane width
/// (`Jerry.dc.html`: `width:300px` on the right column), shown alongside [`LIST_WIDTH`] only
/// while [`CompletionsStatus::Ready`] has a real selected item to describe - `Loading`/`Failed`
/// show the list column alone, since neither has anything real for a detail pane to describe.
/// `README.md`'s own summary confirms this is core to "the differentiator" three-popup language
/// server UI ("Right 300: signature in mono, doc in 11px Plex Sans #7d848b, module path
/// footer"), not optional polish - this was a real, previously undocumented-to-the-user scope
/// cut, not a deliberate permanent simplification.
const DETAIL_WIDTH: gpui::Pixels = gpui::px(300.0);
/// The design mockup's own real completion-item row height (`Jerry.dc.html`: `height:22px`).
const POPOVER_ROW_HEIGHT: gpui::Pixels = gpui::px(22.0);
/// 8 real rows' worth of [`POPOVER_ROW_HEIGHT`] plus the popover's own real `py(3.0)` top/bottom
/// padding (see [`AdeApp::render_completions_popover`]'s own padding) - not derived arithmetically
/// from [`POPOVER_ROW_HEIGHT`] itself, since `gpui::Pixels`' inner `f32` is only `pub(crate)`
/// inside `gpui`, not reachable for a real `const`-context multiply from this crate.
const POPOVER_MAX_HEIGHT: gpui::Pixels = gpui::px(182.0);

impl AdeApp {
    /// Whether [`Self::completions`] is genuinely, *actionably* open *for the currently active
    /// editable file* - the real guard `crate::code_surface::editing`'s `EditorUp`/`EditorDown`/
    /// `EditorEnter` handlers use to refuse firing while the popup that should own that keystroke
    /// instead is open (see those handlers' own docs), and the same real condition
    /// `crate::code_surface::render::AdeApp::render_code_surface` uses to decide whether to add the
    /// `"completions"` key context.
    ///
    /// Deliberately requires [`CompletionsStatus::Ready`], not just *any* [`CompletionsEntry`]
    /// (Revision R8.5b audit finding 1's fix for a real, live-reproduced keystroke-swallowing
    /// bug): [`crate::lsp::client::AdeApp::prepare_lsp_sync`] seeds a real `Loading` entry on
    /// *every* completion-worthy keystroke, before the real `textDocument/completion` request
    /// even goes out - an earlier version of this method returned `true` for that `Loading` state
    /// too, which meant the `"completions"` key context (and thus the `CompletionsAccept`/`Up`/
    /// `Down` bindings) claimed `Enter`/`Up`/`Down` for the *entire* real round-trip a completion
    /// request takes, even though there was nothing yet to navigate or accept - live-reproduced
    /// against a real rust-analyzer: pressing Enter while a request was merely in flight inserted
    /// no newline at all, and Down did nothing either. A `Failed` entry gets the same honest
    /// treatment: nothing real to navigate/accept there either, just an error message to read.
    /// Only a genuine [`CompletionsStatus::Ready`] popup - something the user can actually
    /// navigate/accept right now - is allowed to claim these keystrokes.
    pub(crate) fn completions_open_for_active_path(&self) -> bool {
        let Some(path) = self.active_editable_path() else {
            return false;
        };
        self.completions.as_ref().is_some_and(|entry| {
            entry.path == path && matches!(entry.status, CompletionsStatus::Ready { .. })
        })
    }

    /// Dismisses [`Self::completions`] unconditionally (a real caret move, a click, a worktree
    /// switch, an explicit `Escape`, ...) and bumps [`Self::completions_generation`] - see that
    /// field's own docs for the real stale-response race this closes. Does **not** call
    /// `cx.notify()` itself, matching `crate::root::OverlayFocus::clear`'s own established
    /// convention: every real caller already has its own surrounding state change and issues one
    /// `cx.notify()` once everything, this included, is done.
    pub(crate) fn dismiss_completions(&mut self) {
        self.completions = None;
        self.completions_generation = self.completions_generation.wrapping_add(1);
    }

    pub(crate) fn handle_completions_up_action(
        &mut self,
        _: &CompletionsUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_completions_selection(-1, cx);
    }

    pub(crate) fn handle_completions_down_action(
        &mut self,
        _: &CompletionsDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_completions_selection(1, cx);
    }

    fn move_completions_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(entry) = self.completions.as_mut() else {
            return;
        };
        let CompletionsStatus::Ready { items, selected } = &mut entry.status else {
            return;
        };
        let shown = items.len().min(MAX_RENDERED_COMPLETION_ITEMS);
        if shown == 0 {
            return;
        }
        let next = (*selected as i32 + delta).rem_euclid(shown as i32) as usize;
        *selected = next;
        cx.notify();
    }

    pub(crate) fn handle_completions_dismiss_action(
        &mut self,
        _: &CompletionsDismiss,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_completions();
        cx.notify();
    }

    /// `Ctrl+Space` (GitHub issue #26) - opens the Completions popup at the caret on demand, even
    /// mid-word with no completion-worthy character just typed (`crate::lsp::client::AdeApp::
    /// invoke_completions_now` builds a real, forced `CompletionTriggerKind::INVOKED` request
    /// regardless of what `crate::lsp::completion::completion_trigger` would otherwise say about
    /// the character before the caret). Pressing it again while a popup is already open re-runs
    /// the exact same real request path, which overwrites [`AdeApp::completions`] in place - never
    /// dismissing it first - so this reads as a real in-place refresh, not a close-then-reopen
    /// flicker. A no-op (no popup, no request) when there's no active editable file or no LSP
    /// client ready for it yet - see [`crate::lsp::client::AdeApp::invoke_completions_now`]'s own
    /// docs for exactly which real preconditions this needs.
    pub(crate) fn handle_completions_invoke_action(
        &mut self,
        _: &CompletionsInvoke,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.active_editable_path() else {
            return;
        };
        self.invoke_completions_now(path, cx);
    }

    pub(crate) fn handle_completions_accept_action(
        &mut self,
        _: &CompletionsAccept,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.accept_active_completion(window, cx);
    }

    /// Real accept: splices the selected completion item's real text into the real buffer via
    /// [`EditBuffer::replace_range`] (the exact same real mutation method every other edit in this
    /// app - typing, paste, Backspace/Delete - reduces to, per `crate::code_surface::edit_buffer`'s own top
    /// docs), then re-runs the same real re-highlight/LSP-sync/scroll bookkeeping an ordinary
    /// keystroke would. Always dismisses the popup first (via `Option::take`).
    ///
    /// Every early-return "nothing real to accept" path (no entry at all, not `Ready`, an
    /// out-of-range selection, no buffer) falls through to the real [`AdeApp::
    /// handle_editor_enter_action`] behavior - a real newline at the real caret - rather than
    /// silently swallowing the keystroke (Revision R8.5b audit finding 1's fix). In ordinary,
    /// real keystroke-driven use this fallback is unreachable: `crate::default_key_bindings`
    /// only ever routes `Enter`/`Tab` here while `Self::completions_open_for_active_path` is
    /// true, which (per that method's own docs) now itself requires a genuine `Ready` entry -
    /// so a real user can no longer land in any of these branches through an ordinary keystroke.
    /// It stays as a real, deliberate defense-in-depth guard for this method's own *direct*
    /// callers (this crate's own tests call it that way, mirroring `crate::code_surface::editing::
    /// AdeApp::handle_editor_enter_action`'s own identical "guard the handler, not just the
    /// binding" discipline - see that method's own docs), and for the popup's mouse-click accept
    /// row (which only ever renders for a genuine `Ready` item, so it too should never actually
    /// hit these branches in practice).
    fn accept_active_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.completions.take() else {
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };
        self.completions_generation = self.completions_generation.wrapping_add(1);
        let CompletionsStatus::Ready { items, selected } = entry.status else {
            cx.notify();
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };
        let Some(item) = items.get(selected) else {
            cx.notify();
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };
        let Some(buffer) = self.edit_buffer_mut(&entry.path) else {
            cx.notify();
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };

        let (range, text) = resolve_completion_edit(buffer, item);
        let path = entry.path.clone();
        // Accepting a completion is a programmatic, whole-token edit, not a typed character - one
        // of GitHub issue #17's four named undo-group boundaries. Sealed on both sides so it is
        // its own real step: the partial word typed before it doesn't absorb it, and neither does
        // whatever is typed after. See `crate::text_history`'s own docs for the policy.
        buffer.seal_history();
        buffer.replace_range(Some(range), &text);
        buffer.seal_history();
        self.schedule_rehighlight(path.clone(), cx);
        self.schedule_lsp_sync(self.file_tree_root.clone(), path, cx);
        self.sync_cursor_and_scroll();
        self.reset_caret_blink(cx);
        cx.notify();
    }

    /// The real, cursor-anchored Completions popover - `None` whenever there's nothing real to
    /// show: no [`Self::completions`] entry, the entry belongs to a file that isn't the one
    /// currently on screen, or the real caret-row layout Revision R8.5a's own painter last
    /// recorded (see this module's own top docs) doesn't match the popup's file/line - the same
    /// honest "degrade to nothing rather than paint at a guessed position" discipline
    /// `crate::code_surface::editing`'s `EntityInputHandler::bounds_for_range`/`character_index_for_point`
    /// already established for this exact real layout cache.
    pub(crate) fn render_completions_popover(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let entry = self.completions.as_ref()?;
        if self.active_editable_path().as_deref() != Some(entry.path.as_path()) {
            return None;
        }
        let buffer = self.edit_buffer(&entry.path)?;
        let (last_path, last_line) = self.file_view_last_layout_for.clone()?;
        if last_path != entry.path {
            return None;
        }
        let last_bounds = self.file_view_last_bounds?;
        let last_layout = self.file_view_last_layout.as_ref()?;
        let (cursor_line, cursor_col) = buffer.line_col_for_offset(buffer.cursor_offset());
        if cursor_line != last_line {
            // The caret has moved off the row that was actually painted last frame (e.g. a fresh
            // debounce fired the instant before this frame's own paint caught up) - no real
            // position to anchor to yet; the very next frame's paint will catch this back up.
            return None;
        }
        let anchor_x = last_bounds.left() + last_layout.x_for_index(cursor_col);
        let row_line_height = gpui::px(self.effective_code_rem_px() * 1.6);
        let row_top = last_bounds.top();
        let row_bottom = row_top + row_line_height;

        // Flip above the caret's row when there isn't real room below it in the window body -
        // the same "measure real available space, flip if it doesn't fit" judgment
        // `vendor/zed/crates/editor/src/element.rs`'s own `layout_popovers_above_or_below_line`
        // makes (see this module's own top docs), reusing `AdeApp::body_bounds` (already captured
        // every render by `AdeApp::render_workspace_body`'s own canvas) rather than a second,
        // independently-tracked window-bounds value.
        let space_below = self.body_bounds.bottom() - row_bottom;
        let fits_below = space_below >= POPOVER_MAX_HEIGHT;
        let top = if fits_below {
            row_bottom
        } else {
            (row_top - POPOVER_MAX_HEIGHT).max(self.body_bounds.top())
        };

        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::POPOVER;
        let extension = entry.path.extension().and_then(|ext| ext.to_str());
        let macos = self.window_controls_style().is_macos();

        // The list column - always present, whatever the status. `Loading`/`Failed` show a
        // single message row here and nothing else; `Ready` shows the real item rows plus the
        // mockup's own footer hint row (`README.md`: "footer `⇅ move · ⏎ accept · ⇥ snippet`").
        let mut list_column = gpui::div()
            .flex_none()
            .w(LIST_WIDTH)
            .flex()
            .flex_col()
            // `py(3.0)`, not `py(4.0)`: the design mockup's own completions-list column padding
            // is `3px 0` (`Jerry.dc.html`: `padding:3px 0` on the `.290px` list column) - see
            // `POPOVER_MAX_HEIGHT`'s own docs for why this exact value matters there too.
            .py(gpui::px(3.0));

        // The selected item, if any - drives the detail pane below. Read once, outside the
        // `Ready` match arm's own item loop, so both the list rows and the detail pane read the
        // exact same real selection rather than two independently-indexed lookups.
        let mut selected_item: Option<&lsp_core::lsp_types::CompletionItem> = None;

        match &entry.status {
            CompletionsStatus::Loading => {
                list_column = list_column.child(popover_message_row("loading completions\u{2026}"));
            }
            CompletionsStatus::Failed(message) => {
                list_column = list_column.child(popover_message_row(&format!(
                    "completion request failed: {message}"
                )));
            }
            CompletionsStatus::Ready { items, selected } => {
                selected_item = items.get(*selected);
                // `border-right:1px solid #23282c` in the mockup, on the list column's own right
                // edge (`Jerry.dc.html`: `width:290px;border-right:1px solid #23282c`) - only
                // while there's a real detail pane beside it to divide from.
                if selected_item.is_some() {
                    list_column = list_column
                        .border_r_1()
                        .border_color(theme::border::CARD);
                }
                for (index, item) in items.iter().take(MAX_RENDERED_COMPLETION_ITEMS).enumerate() {
                    let (label, detail) = completion_view::completion_item_display(item);
                    let is_selected = index == *selected;
                    let kind_badge = completion_view::completion_kind_badge(item.kind);
                    list_column = list_column.child(
                        gpui::div()
                            .id(("completion-item", index))
                            // Lets a real test measure/find this real row (`debug_bounds` reads
                            // this, not `.id(..)`) - a no-op outside test builds, matching every
                            // other `debug_selector` in this crate.
                            .debug_selector(move || format!("completion-item-{index}"))
                            .flex_none()
                            .h(POPOVER_ROW_HEIGHT)
                            // `px(8.0)`, not `px(10.0)` - the design mockup's own completion-item
                            // row padding is `0 8px` (`Jerry.dc.html`: `padding:0 8px` on each
                            // `.completions` row).
                            .px(gpui::px(8.0))
                            .flex()
                            .items_center()
                            .gap(gpui::px(8.0))
                            .cursor_pointer()
                            // `theme::completions_popup::ITEM_SELECTED_BG` (`#243c50`), not
                            // `theme::surface::CURRENT_LINE` (`#181c20`) - `CURRENT_LINE` is the
                            // exact same hex as this popover's own background
                            // (`theme::surface::POPOVER`), so the "selected" row highlight used
                            // to be genuinely invisible; see that token's own docs.
                            .when(is_selected, |el| {
                                el.bg(theme::completions_popup::ITEM_SELECTED_BG)
                            })
                            .children(kind_badge.map(render_completion_kind_badge))
                            .child(
                                gpui::div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if is_selected {
                                        theme::completions_popup::ITEM_SELECTED_FG
                                    } else {
                                        theme::completions_popup::ITEM_FG
                                    })
                                    .child(label),
                            )
                            .children(detail.map(|detail| {
                                gpui::div()
                                    .flex_none()
                                    .text_size(gpui::px(10.0))
                                    .text_color(theme::text::GHOST)
                                    .child(detail)
                            }))
                            // A real click both selects *and* accepts this row in one step -
                            // `on_mouse_down`, not `on_click`, matching this app's own established
                            // idiom for a popover row that both selects and immediately commits
                            // (`crate::work_surface::render::render_dropdown_menu_row`'s sibling
                            // rows use `on_click` since picking one never needs an intermediate
                            // "select" state the way this popup's keyboard nav does).
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(
                                    move |this, _event: &gpui::MouseDownEvent, window, cx| {
                                        if let Some(entry) = this.completions.as_mut() {
                                            if let CompletionsStatus::Ready { selected, .. } =
                                                &mut entry.status
                                            {
                                                *selected = index;
                                            }
                                        }
                                        this.accept_active_completion(window, cx);
                                        cx.stop_propagation();
                                    },
                                ),
                            ),
                    );
                }
                if items.len() > MAX_RENDERED_COMPLETION_ITEMS {
                    list_column = list_column.child(popover_message_row(&format!(
                        "+ {} more",
                        items.len() - MAX_RENDERED_COMPLETION_ITEMS
                    )));
                }
                list_column = list_column.child(
                    gpui::div()
                        .id("completions-footer-hints")
                        // Lets a real test measure this real row's own painted bounds
                        // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test
                        // builds, matching every other `debug_selector` in this crate.
                        .debug_selector(|| "completions-footer-hints".to_string())
                        .flex_none()
                        .h(gpui::px(20.0))
                        .px(gpui::px(8.0))
                        .mt(gpui::px(3.0))
                        .border_t_1()
                        .border_color(theme::border::CARD)
                        .flex()
                        .items_center()
                        .child(render_hint_row(
                            [
                                (
                                    keymap::resolve_combo("\u{2191}\u{2193}", macos),
                                    "move",
                                ),
                                (keymap::resolve_combo("enter", macos), "accept"),
                                (keymap::resolve_combo("tab", macos), "snippet"),
                            ]
                            .into_iter()
                            .map(|(keys, label)| render_hint_pair(&keys, label).into_any_element()),
                        )),
                );
            }
        }

        let mut popover = gpui::div()
            .id("completions-popover")
            // Lets a real test measure this real popover's own painted bounds (`debug_bounds`
            // reads this, not `.id(..)`) - a no-op outside test builds, matching every other
            // `debug_selector` in this crate.
            .debug_selector(|| "completions-popover".to_string())
            .absolute()
            .left(anchor_x)
            .top(top)
            .max_h(POPOVER_MAX_HEIGHT)
            .overflow_hidden()
            .flex()
            .bg(theme::surface::POPOVER)
            .border_1()
            .border_color(theme::border::POPOVER)
            // `CARD_SM` (`5px`), not `CARD` (`6px`) - the design mockup's own completions popup
            // border-radius is `5px` (`Jerry.dc.html`: `border-radius:5px` on the popup itself),
            // matching `crate::code_surface::lsp_ui::AdeApp::render_hover_card`'s own popover,
            // which already used the correct radius here.
            .rounded(theme::radius::CARD_SM)
            .shadow(vec![BoxShadow::new(
                shadow_x,
                shadow_y,
                // `0.50`, not `0.55` - the design mockup's own completions popup shadow is
                // `rgba(0,0,0,.5)` (`Jerry.dc.html`: `box-shadow:0 8px 20px rgba(0,0,0,.5)`),
                // matching `theme::shadow::POPOVER`'s own doc comment, which already recorded
                // the correct `0.50` even though this call site had drifted from it.
                gpui::black().opacity(0.50),
            )
            .blur_radius(shadow_blur)])
            .font(gpui::font(theme::font::MONO))
            .text_size(gpui::px(11.5))
            .child(list_column);

        // The detail pane - `border-right` lives on the list column's own right edge in the
        // mockup (`Jerry.dc.html`: `border-right:1px solid #23282c` on the 290px list), so it's
        // applied to `list_column` above only when there's a real detail pane beside it to
        // divide from; a lone list column (Loading/Failed) has no seam to draw.
        if let Some(item) = selected_item {
            popover = popover.child(render_completion_detail_pane(item, extension));
        }

        Some(popover.into_any_element())
    }
}

/// A real completion item's kind badge - a 13x13 box with a one-letter glyph, colored per
/// [`completion_view::CompletionKindBadge`] - matches the design mockup's own kind-badge markup
/// exactly (`Jerry.dc.html`: `width:13px;height:13px;border-radius:2px` with `font:500 8px`).
fn render_completion_kind_badge(kind: completion_view::CompletionKindBadge) -> gpui::AnyElement {
    let (fg, bg) = match kind {
        completion_view::CompletionKindBadge::Function => theme::completions_popup::KIND_FUNCTION,
        completion_view::CompletionKindBadge::Variable => theme::completions_popup::KIND_VARIABLE,
        completion_view::CompletionKindBadge::Type => theme::completions_popup::KIND_TYPE,
    };
    gpui::div()
        .flex_none()
        .w(gpui::px(13.0))
        .h(gpui::px(13.0))
        .rounded(theme::radius::MARK)
        .flex()
        .items_center()
        .justify_center()
        .bg(bg)
        .text_color(fg)
        .font(gpui::font(theme::font::MONO))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(gpui::px(8.0))
        .child(kind.letter())
        .into_any_element()
}

/// The Completions popup's own detail pane - the design mockup's real, 300px right column
/// (`design_handoff_jerry_ade/revision 3/Jerry.dc.html`: `width:300px;padding:8px 10px`),
/// describing whichever item is currently selected: a syntax-highlighted signature line, doc
/// prose, and a module-path footer - mirroring `crate::code_surface::lsp_ui`'s Hover card
/// exactly (same three-piece shape, same reasoning for showing it), just for the selected
/// completion item instead of a `textDocument/hover` response.
///
/// Design-review follow-up: this pane didn't exist at all before - the popup was list-only, a
/// real, previously undocumented-to-the-user scope gap (see [`DETAIL_WIDTH`]'s own docs).
fn render_completion_detail_pane(
    item: &lsp_core::lsp_types::CompletionItem,
    extension: Option<&str>,
) -> gpui::AnyElement {
    let mut pane = gpui::div()
        .id("completions-detail-pane")
        // Lets a real test measure this real pane's own painted bounds (`debug_bounds` reads
        // this, not `.id(..)`) - a no-op outside test builds, matching every other
        // `debug_selector` in this crate.
        .debug_selector(|| "completions-detail-pane".to_string())
        .flex_none()
        .w(DETAIL_WIDTH)
        .px(gpui::px(10.0))
        .py(gpui::px(8.0));

    // Signature: `item.detail` when the server sent one (rust-analyzer/typescript-language-
    // server/pyright all commonly do, inline, no `completionItem/resolve` round trip needed) -
    // the bare label otherwise, so the pane is never left blank for a real, selected item.
    // Highlighted the same real way `crate::code_surface::code_view::highlight_block` highlights
    // any other standalone fragment (a diff hunk, a merge conflict side) - see that function's
    // own docs.
    let signature_text = item
        .detail
        .as_ref()
        .map(|detail| detail.trim())
        .filter(|detail| !detail.is_empty())
        .unwrap_or(item.label.as_str());
    let signature_runs = code_view::highlight_block(
        std::iter::once(signature_text),
        extension,
        code_view::HighlightOptions::default(),
    )
    .into_iter()
    .next()
    .map(|line| line.runs)
    .unwrap_or_default();
    let mut signature_row = gpui::div()
        .flex()
        .flex_wrap()
        .font(gpui::font(theme::font::MONO))
        .text_size(gpui::px(11.0));
    for (index, (run_text, kind)) in signature_runs.into_iter().enumerate() {
        signature_row = signature_row.child(
            gpui::div()
                .id(("completion-detail-signature-token", index))
                .text_color(code_view::color_for_kind(kind))
                .child(run_text),
        );
    }
    pane = pane.child(signature_row);

    if let Some(doc) = completion_view::completion_documentation_text(item) {
        pane = pane.child(
            gpui::div()
                .mt(gpui::px(7.0))
                .font(gpui::font(theme::font::SANS))
                .text_size(gpui::px(11.0))
                .text_color(theme::text::DIMMER)
                .child(doc),
        );
    }

    if let Some(module_path) = completion_view::completion_module_path(item) {
        pane = pane.child(
            gpui::div()
                .mt(gpui::px(9.0))
                .pt(gpui::px(7.0))
                .border_t_1()
                .border_color(theme::border::CARD)
                .font(gpui::font(theme::font::MONO))
                .text_size(gpui::px(10.0))
                .text_color(theme::text::GHOST)
                .child(module_path),
        );
    }

    pane.into_any_element()
}

fn popover_message_row(text: &str) -> gpui::AnyElement {
    gpui::div()
        .flex_none()
        .h(POPOVER_ROW_HEIGHT)
        .px(gpui::px(8.0))
        .flex()
        .items_center()
        .text_color(theme::text::FAINT)
        .child(text.to_string())
        .into_any_element()
}

/// The real `(byte_range, text)` [`AdeApp::accept_active_completion`] should splice into `buffer`
/// for `item` - prefers a real `text_edit` (via [`completion_view::completion_text_edit`]) over
/// the plain `insert_text`/`label` fallback, which replaces the identifier prefix already typed
/// before the caret (via [`completion_view::identifier_prefix_start`]) rather than inserting
/// after it verbatim (which would duplicate the prefix - `"pri"` + accepting `"println!"` must
/// yield `"println!"`, not `"priprintln!"`).
fn resolve_completion_edit(
    buffer: &EditBuffer,
    item: &lsp_core::lsp_types::CompletionItem,
) -> (std::ops::Range<usize>, String) {
    if let Some((lsp_range, text)) = completion_view::completion_text_edit(item) {
        let start = buffer.offset_for_position(lsp_range.start.line, lsp_range.start.character);
        let end = buffer.offset_for_position(lsp_range.end.line, lsp_range.end.character);
        return (start.min(end)..start.max(end), text);
    }

    let text = completion_view::completion_plain_insert_text(item);
    let cursor = buffer.cursor_offset();
    let (line, _) = buffer.line_col_for_offset(cursor);
    let line_range = buffer
        .line_ranges
        .get(line)
        .cloned()
        .unwrap_or(cursor..cursor);
    let line_text = buffer
        .content
        .get(line_range.clone())
        .unwrap_or_default()
        .to_string();
    let local_cursor = cursor.saturating_sub(line_range.start).min(line_text.len());
    let prefix_start_local = completion_view::identifier_prefix_start(&line_text, local_cursor);
    (line_range.start + prefix_start_local..cursor, text)
}

/// Regression coverage for the design-review follow-up: the Completions popup used to be a
/// list-only single column - `design_handoff_jerry_ade/revision 3/Jerry.dc.html`'s own mockup
/// (and `README.md`'s "Right 300: signature in mono, doc in 11px Plex Sans #7d848b, module path
/// footer") describes a real two-column popup with a detail pane and a footer hint row, neither
/// of which existed before this fix. These tests prove the real painted layout, not just that
/// the new code compiles and runs.
#[cfg(test)]
mod completion_detail_pane_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn seed_ready_popup(
        cx: &mut TestAppContext,
        items: Vec<lsp_core::lsp_types::CompletionItem>,
    ) -> (
        gpui::Entity<AdeApp>,
        &mut gpui::VisualTestContext,
        PathBuf,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file = repo.path().join("sample.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write sample.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file, window, cx);
        });
        cx.run_until_parked();
        let relative = PathBuf::from("sample.rs");
        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready { items, selected: 0 },
            });
            cx.notify();
        });
        cx.run_until_parked();
        (app, cx, relative)
    }

    /// A real, fully-populated item (a real `detail`, `documentation`, and `label_details`
    /// description - the three real fields the detail pane reads) must paint a real list column,
    /// a real detail pane beside it, and the real footer hint row - the whole shape the mockup's
    /// own two-column popup describes, none of which existed at all before this fix.
    #[gpui::test]
    fn a_real_selected_item_paints_both_the_list_column_and_a_real_detail_pane(
        cx: &mut TestAppContext,
    ) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn push_str(&mut self, string: &str)".to_string()),
            documentation: Some(lsp_core::lsp_types::Documentation::String(
                "Appends a given string slice.".to_string(),
            )),
            label_details: Some(lsp_core::lsp_types::CompletionItemLabelDetails {
                detail: None,
                description: Some("alloc::string::String".to_string()),
            }),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        assert!(
            cx.debug_bounds("completion-item-0").is_some(),
            "sanity check: the real item row must have painted"
        );
        let popover = cx
            .debug_bounds("completions-popover")
            .expect("the real popover must have painted");
        let detail_pane = cx
            .debug_bounds("completions-detail-pane")
            .expect("a real selected item with real detail/documentation must paint a real \
                     detail pane, not silently stay list-only");
        let footer_hints = cx
            .debug_bounds("completions-footer-hints")
            .expect("the real footer hint row must have painted alongside the item rows");

        // `+ px(2.0)`: the popover's own real `.border_1()` (1px on each side) is part of its
        // painted border-box width, on top of the two columns' own content widths.
        assert_eq!(
            popover.size.width,
            LIST_WIDTH + DETAIL_WIDTH + gpui::px(2.0),
            "the real popover must be exactly as wide as its list column plus its detail pane \
             combined (plus its own 1px border on each side) - matching the mockup's own real \
             590px total (290 list + 300 detail)"
        );
        // `+ px(1.0)`: the popover's own real 1px left border sits between its own left edge and
        // the list column's content.
        assert!(
            (detail_pane.left() - (popover.left() + gpui::px(1.0) + LIST_WIDTH)).abs()
                < gpui::px(1.0),
            "the real detail pane must begin exactly where the real list column ends (popover \
             left {:?}, list width {:?}, detail pane left {:?})",
            popover.left(),
            LIST_WIDTH,
            detail_pane.left()
        );
        assert!(
            footer_hints.top() >= detail_pane.top(),
            "the real footer hint row belongs to the list column, below the real item rows, not \
             floating above the detail pane"
        );
    }

    /// `Loading` has no real selected item to describe - the detail pane and footer hints must
    /// both stay genuinely absent, and the popover must paint at the narrower list-only width,
    /// not the wider two-column one.
    #[gpui::test]
    fn a_loading_popup_paints_only_the_list_column_with_no_detail_pane_or_footer_hints(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file = repo.path().join("sample.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write sample.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file, window, cx);
        });
        cx.run_until_parked();
        let relative = PathBuf::from("sample.rs");
        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative,
                status: CompletionsStatus::Loading,
            });
            cx.notify();
        });
        cx.run_until_parked();

        let popover = cx
            .debug_bounds("completions-popover")
            .expect("the real popover must still paint a real loading message");
        // `+ px(2.0)`: the popover's own real `.border_1()` (1px on each side).
        assert_eq!(
            popover.size.width,
            LIST_WIDTH + gpui::px(2.0),
            "a real Loading popup has no real item to describe, so it must stay at the narrow \
             list-only width, not the wider two-column one"
        );
        assert!(
            cx.debug_bounds("completions-detail-pane").is_none(),
            "a real Loading popup must never paint a real detail pane - there is no real \
             selected item for it to describe"
        );
        assert!(
            cx.debug_bounds("completions-footer-hints").is_none(),
            "a real Loading popup must never paint the real footer hint row either - it belongs \
             to the Ready list, not the loading message"
        );
    }
}
