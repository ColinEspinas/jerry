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
//! ## Scrolling (GitHub issue #185)
//!
//! The item list is a real virtualized [`gpui::uniform_list`] tracked by
//! [`AdeApp::completions_scroll_handle`], capped at [`popover_list_max_height`]
//! ([`MAX_VISIBLE_COMPLETION_ROWS`] rows) with this app's own overlay scrollbar
//! ([`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`]) as a sibling. There is no
//! render cap: an earlier version of this module truncated the real, live-returned response at
//! `MAX_RENDERED_COMPLETION_ITEMS` (12) and painted a static `"+ N more"` row instead, which made
//! every item past the twelfth permanently unreachable by keyboard *and* by mouse - and, since
//! that version's popup was only `182px` tall inside an `overflow_hidden()` container, seven
//! whole rows was all it could actually show of the twelve it built, so it silently clipped the
//! rest *and* the `"+ N more"` row itself. [`AdeApp::move_completions_selection`] now wraps over
//! the whole list and scrolls the viewport to follow the selection.
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
        /// **Every** item the server actually returned, in the order it returned them - never
        /// narrowed in place. Keeping the full response intact is what lets Backspace genuinely
        /// widen the popup back out (see [`AdeApp::refilter_completions`]) without re-asking the
        /// server for a set it already sent.
        items: Vec<lsp_core::lsp_types::CompletionItem>,
        /// Indices into `items`, best match first, for the prefix currently typed at the caret -
        /// the real, client-side filtered/re-ranked view (GitHub issue #189), computed by
        /// [`completion_view::rank_completion_items`]. This, not `items`, is what the popup
        /// renders and what keyboard navigation walks; never empty for a live `Ready` entry (an
        /// empty filter result dismisses the popup outright, exactly as an empty server response
        /// already does).
        visible: Vec<usize>,
        /// An index into `visible`, **not** into `items` - so "the Nth visible row" and "the Nth
        /// selected item" can never drift apart as the filter narrows.
        selected: usize,
    },
    Failed(String),
}

impl CompletionsStatus {
    /// A real `Ready` state for a server response, already narrowed/re-ranked against `query` (the
    /// identifier prefix currently typed at the caret - see [`AdeApp::completion_filter_query`]).
    /// `None` when nothing in `items` matches `query` at all, which the caller treats exactly the
    /// way it already treats a genuinely empty server response: no popup.
    pub(crate) fn ready(
        items: Vec<lsp_core::lsp_types::CompletionItem>,
        query: &str,
    ) -> Option<Self> {
        let visible = completion_view::rank_completion_items(&items, query);
        if visible.is_empty() {
            return None;
        }
        Some(Self::Ready {
            items,
            visible,
            selected: 0,
        })
    }
}

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
/// `uniform_list`'s one real requirement is that every row is exactly this tall - see
/// [`AdeApp::render_completions_popover`]'s own docs.
const POPOVER_ROW_HEIGHT: gpui::Pixels = gpui::px(22.0);
/// How many real completion rows the popup shows at once before the rest have to be *scrolled*
/// to (GitHub issue #185). This is a viewport height, no longer a render cap: every item a real
/// server returned is reachable by keyboard and by mouse wheel/scrollbar. `12` matches the same
/// judgment every other real editor makes here - `vendor/zed/crates/editor`'s own completions
/// menu defaults to 12 lines too (`max_height_in_lines`), as does VS Code's.
const MAX_VISIBLE_COMPLETION_ROWS: usize = 12;
/// The popover's own real `py(3.0)` top/bottom padding, as a single vertical total - the design
/// mockup's own completions-list column padding is `3px 0` (`Jerry.dc.html`: `padding:3px 0` on
/// the `290px` list column), so the popup is exactly this much taller than its list.
const POPOVER_VERTICAL_PADDING: gpui::Pixels = gpui::px(6.0);
/// The popover's own real `border_1()`, top plus bottom. GPUI lays out border-box, so this counts
/// toward the popup's painted height and therefore toward its own `max_h` - leaving it out of
/// [`popover_max_height`] would silently cost the twelfth row two of its twenty-two pixels.
/// Measured, not assumed: `completions_scroll_tests` asserts the popup's real painted height
/// against exactly this arithmetic.
const POPOVER_BORDER_HEIGHT: gpui::Pixels = gpui::px(2.0);
/// The footer hint row's own real painted height (`h(20.0)` plus its `mt(3.0)` top margin, plus
/// its own `border_t_1()` collapsing half a pixel into the row above it under GPUI's layout) -
/// only present for a real [`CompletionsStatus::Ready`] popup (see [`AdeApp::
/// render_completions_popover`]'s own match arm), but folded into [`popover_max_height`]
/// unconditionally since that height also drives the popover's flip-above-the-caret decision and
/// its own `overflow_hidden()` clamp, both of which must have real room for the footer whenever a
/// `Ready` popup is the tallest thing being measured. Measured, not assumed: `completions_scroll_tests`
/// asserts the popup's real painted height against exactly this arithmetic.
const POPOVER_FOOTER_HEIGHT: gpui::Pixels = gpui::px(23.0);

/// A real, typical upper bound on [`render_completion_detail_pane`]'s own painted height for a
/// normal item: its doc paragraph is capped at `.line_clamp(6)` (~17px/line ≈ 102px), a real
/// signature line rarely wraps past two real lines (~40px), and the module-path footer plus the
/// pane's own paddings/margins add roughly 45px more - about 190px total, rounded up for real
/// headroom. Used only for [`AdeApp::render_completions_popover`]'s own flip-above-the-caret
/// *estimate*, never as a hard cap (that's still [`popover_max_height`]'s own job via the popup's
/// real `.max_h()`/`.overflow_hidden()`) - an unusually long signature can still legitimately
/// exceed this and simply get clipped by that real cap, exactly as it always could.
const DETAIL_PANE_TYPICAL_HEIGHT: gpui::Pixels = gpui::px(190.0);

/// [`MAX_VISIBLE_COMPLETION_ROWS`] rows' worth of [`POPOVER_ROW_HEIGHT`] - the scrolling list's
/// own `max_h`, which is what clamps `gpui::ListSizingBehavior::Infer`'s laid-out height and so
/// gives the scroll handle a real viewport smaller than its content. See
/// [`AdeApp::render_completions_popover`]'s own comment at the `max_h` call site for the measured,
/// honest account of how this and [`popover_max_height`] divide the work (they overlap).
///
/// A `fn` rather than a `const`, since `gpui::Pixels`' inner `f32` is only `pub(crate)` inside
/// `gpui`, so `Pixels * f32` (`vendor/zed/crates/gpui/src/geometry.rs:2707`) isn't
/// `const`-callable here - deriving it for real still beats restating the product as a magic
/// number.
fn popover_list_max_height() -> gpui::Pixels {
    POPOVER_ROW_HEIGHT * MAX_VISIBLE_COMPLETION_ROWS as f32
}

/// [`popover_list_max_height`] plus the popover's own [`POPOVER_VERTICAL_PADDING`],
/// [`POPOVER_BORDER_HEIGHT`], and [`POPOVER_FOOTER_HEIGHT`] - the whole popup's real maximum
/// painted height, still the real cap [`AdeApp::render_completions_popover`]'s own `.max_h()`/
/// `.overflow_hidden()` enforces (see [`estimated_popover_height`]'s own docs for what drives the
/// *positioning* decision instead - a real, worst-case-only height was itself the bug: for a
/// short, common list this always assumed the full `MAX_VISIBLE_COMPLETION_ROWS` list, which
/// could put the flipped-above popup dozens of real pixels higher than its own actually-painted
/// content, leaving a large, visibly wrong gap between it and the real caret row it was supposed
/// to anchor to).
fn popover_max_height() -> gpui::Pixels {
    popover_list_max_height()
        + POPOVER_VERTICAL_PADDING
        + POPOVER_BORDER_HEIGHT
        + POPOVER_FOOTER_HEIGHT
}

/// A real, tighter *estimate* of what [`AdeApp::render_completions_popover`] is actually about to
/// paint for `status` - used only to decide *where* to position the popup (the flip-above-the-
/// caret judgment and, when flipped, the real vertical offset), never as the popup's own render
/// cap (that stays [`popover_max_height`], via the popup's own `.max_h()`/`.overflow_hidden()`,
/// completely unchanged). [`popover_max_height`]'s own worst-case number (a full
/// `MAX_VISIBLE_COMPLETION_ROWS`-row list) is the right cap for "never paint past this", but the
/// wrong number for "how far above the caret should this be positioned" - a real, common 2-3 item
/// filtered list is a small fraction of that, and positioning it as if it were the full 12-row
/// list left a large, real gap between the flipped-above popup and the caret row it's meant to
/// anchor to, which is what made the popup look like it was floating at the wrong, "super high"
/// position entirely.
fn estimated_popover_height(status: &CompletionsStatus) -> gpui::Pixels {
    match status {
        CompletionsStatus::Ready { visible, .. } => {
            let rows = visible.len().clamp(1, MAX_VISIBLE_COMPLETION_ROWS) as f32;
            let list_height = POPOVER_ROW_HEIGHT * rows
                + POPOVER_VERTICAL_PADDING
                + POPOVER_BORDER_HEIGHT
                + POPOVER_FOOTER_HEIGHT;
            // A real `Ready` popup always has a real selected item (`visible` is never empty -
            // see that field's own docs), so the detail pane always paints alongside the list.
            list_height.max(DETAIL_PANE_TYPICAL_HEIGHT)
        }
        CompletionsStatus::Loading | CompletionsStatus::Failed(_) => {
            POPOVER_ROW_HEIGHT + POPOVER_VERTICAL_PADDING + POPOVER_BORDER_HEIGHT
        }
    }
}

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
        self.completions_resolved_items.clear();
    }

    /// The real text the user has typed since the completion was triggered, as a live-completions
    /// client must match it: the identifier prefix immediately before the caret in `path`'s own
    /// buffer (`crate::lsp::completion::identifier_prefix_start` - the exact same real word-start
    /// scan [`resolve_completion_edit`] already uses to decide what an accepted item replaces, so
    /// "what gets filtered on" and "what gets overwritten on accept" can never disagree).
    ///
    /// Deliberately derived from the buffer every time rather than accumulated in a separate
    /// "typed since trigger" field: that scan is already the real definition of the word being
    /// completed, it stays correct through Backspace/Delete and caret moves with no extra
    /// bookkeeping to get out of sync, and it is empty exactly when it should be - right after a
    /// real trigger character (`foo.`, `std::`), where nothing has been typed to narrow by yet.
    /// This is the same "word range at the position" query model VSCode's own suggest widget uses.
    ///
    /// `None` when there is no buffer for `path` at all; the empty string (match everything) is a
    /// real, distinct answer from that.
    pub(crate) fn completion_filter_query(&self, path: &Path) -> Option<String> {
        let buffer = self.edit_buffer(path)?;
        let cursor = buffer.cursor_offset();
        let (line, _) = buffer.line_col_for_offset(cursor);
        let line_range = buffer.line_ranges.get(line).cloned()?;
        let line_text = buffer.content.get(line_range.clone())?;
        let local_cursor = cursor.saturating_sub(line_range.start).min(line_text.len());
        let prefix_start = completion_view::identifier_prefix_start(line_text, local_cursor);
        Some(line_text[prefix_start..local_cursor].to_string())
    }

    /// Re-applies the real, client-side filter (GitHub issue #189) to whatever completion list is
    /// currently held, against the prefix now typed at the caret - the fix's whole point: this runs
    /// synchronously on every real keystroke (from `crate::lsp::client::AdeApp::schedule_lsp_sync`,
    /// the one call site every real edit path already funnels through), so the popup narrows
    /// instantly rather than waiting on the debounced `textDocument/completion` round trip that
    /// refreshes the underlying candidate set behind it.
    ///
    /// The two compose without fighting: the server's own response still *replaces* `items`
    /// wholesale when it lands (a moved position genuinely changes what's semantically valid), and
    /// `crate::lsp::client::AdeApp::apply_completion_result` re-derives `visible` from this same
    /// query as it does so - so a response always arrives already narrowed to what's typed, and
    /// every keystroke in between narrows further without a round trip.
    ///
    /// Narrowing to nothing dismisses the popup outright, matching both what a real suggest widget
    /// does when the typed text stops matching anything and this module's own existing discipline
    /// (`crate::lsp::client::AdeApp::prepare_lsp_sync` already dismisses when the context that
    /// justified the popup is gone). It matters mechanically too: [`Self::
    /// completions_open_for_active_path`] claims `Enter`/`Up`/`Down` for *any* `Ready` entry, so an
    /// entry left open with nothing visible would silently swallow those keystrokes.
    pub(crate) fn refilter_completions(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .completions
            .as_ref()
            .filter(|entry| matches!(entry.status, CompletionsStatus::Ready { .. }))
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        let query = self.completion_filter_query(&path).unwrap_or_default();
        let Some(entry) = self.completions.as_mut() else {
            return;
        };
        let CompletionsStatus::Ready {
            items,
            visible,
            selected,
        } = &mut entry.status
        else {
            return;
        };

        let previously_selected_item = visible.get(*selected).copied();
        let next_visible = completion_view::rank_completion_items(items, &query);
        if next_visible.is_empty() {
            self.dismiss_completions();
            cx.notify();
            return;
        }
        // Follow the *item* the user had selected to its new row, rather than keeping the raw row
        // number (which would silently point at a different completion once the list narrows).
        // Falls back to the new best match when that item didn't survive the narrowing. No
        // keyboard-reachable-row cap here (unlike an earlier version of this fix): GitHub issue
        // #185's real virtualized scrolling means every real row is reachable regardless of how
        // far down the now-narrowed list it lands, so the followed row is scrolled into view
        // below instead of being discarded.
        let next_selected = previously_selected_item
            .and_then(|item| next_visible.iter().position(|index| *index == item))
            .unwrap_or(0);
        *selected = next_selected;
        *visible = next_visible;
        // Same real "scroll the minimum amount needed to bring this row into view" strategy
        // `Self::move_completions_selection` uses for the identical job - the followed row can
        // land well outside the current viewport (a filter narrowing the list changes every row's
        // own position), and without this the selection highlight would move somewhere the user
        // can't actually see.
        self.completions_scroll_handle
            .scroll_to_item(next_selected, gpui::ScrollStrategy::Nearest);
        self.maybe_resolve_selected_completion_item(cx);
        cx.notify();
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

    /// Moves the popup's keyboard selection by `delta` rows, wrapping at both ends, and scrolls
    /// the list's viewport just enough to keep the newly selected row visible.
    ///
    /// The wrap is over the **whole** real, live-returned item list (GitHub issue #185). It used
    /// to be over `items.len().min(MAX_RENDERED_COMPLETION_ITEMS)` - a 12-item render cap with no
    /// scroll mechanism behind it - which made every item past the twelfth permanently unreachable
    /// by keyboard *and* by mouse. There is no cap of any kind now: [`Self::completions_scroll_handle`]
    /// plus the `uniform_list` in [`Self::render_completions_popover`] is the real scrolling the
    /// cap was standing in for.
    ///
    /// `gpui::ScrollStrategy::Nearest` (not `Top`/`Center`) is the "scroll the minimum amount
    /// needed to bring this row fully into view, and don't move at all if it already is"
    /// strategy: the same one `crate::code_surface::editing::AdeApp::sync_cursor_and_scroll` uses
    /// to keep the real caret's own row in view, and the same one `vendor/zed/crates/editor/src/
    /// code_context_menus.rs:611` uses for this exact job in Zed's own completions menu. The
    /// scroll is a *deferred* target resolved in the list's next prepaint (see
    /// `vendor/zed/crates/gpui/src/elements/uniform_list.rs:150`), so the `cx.notify()` below is
    /// what actually makes it happen - it is not an immediate offset write.
    fn move_completions_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(entry) = self.completions.as_mut() else {
            return;
        };
        // Navigation walks the *filtered* view (`visible`), never the raw server list - `selected`
        // is an index into `visible` (see that field's own docs), so "the Nth visible row" and
        // "the Nth item keyboard nav lands on" stay the same thing however far the filter narrows.
        let CompletionsStatus::Ready {
            visible, selected, ..
        } = &mut entry.status
        else {
            return;
        };
        // Navigation walks the whole real, filtered `visible` list, not a truncated slice of it -
        // GitHub issue #185 replaced the old hard render cap with real virtualized scrolling, so
        // there is no shorter "shown" subset to clamp against anymore.
        let total = visible.len();
        if total == 0 {
            return;
        }
        let next = (*selected as i32 + delta).rem_euclid(total as i32) as usize;
        *selected = next;
        self.completions_scroll_handle
            .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        self.maybe_resolve_selected_completion_item(cx);
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
        // Taken, not read: the generation bump above has already retired this response, so the map
        // has to be emptied - but the item being accepted right now still needs whatever resolve
        // landed for it, so it is moved out rather than dropped.
        let resolved_items = std::mem::take(&mut self.completions_resolved_items);
        let CompletionsStatus::Ready {
            items,
            visible,
            selected,
        } = entry.status
        else {
            cx.notify();
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };
        // `selected` indexes the filtered view, so it must be resolved *through* `visible` back
        // into the real server list - accepting "the row I can see" and "the item that gets
        // inserted" are the same item by construction. Prefers the merged
        // `completionItem/resolve` response when one has landed, because that is where a real
        // server puts the `additionalTextEdits` that write an auto-import's own `import` line -
        // accepting the inline item instead would insert the name and silently skip the import.
        let resolved_selected = visible
            .get(selected)
            .and_then(|index| resolved_items.get(index))
            .cloned();
        let Some(item) = resolved_selected
            .as_ref()
            .or_else(|| visible.get(selected).and_then(|index| items.get(*index)))
        else {
            cx.notify();
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };
        // Read before the buffer is borrowed mutably below.
        let auto_import = self.settings.editor.auto_import;
        let Some(buffer) = self.edit_buffer_mut(&entry.path) else {
            cx.notify();
            self.replace_text_in_range(None, "\n", window, cx);
            self.sync_cursor_and_scroll();
            self.reset_caret_blink(cx);
            return;
        };

        let (range, text) = resolve_completion_edit(buffer, item);
        // The item's own `additionalTextEdits` - which for every real server is where an
        // auto-import's `import`/`use` line lives, and which nothing in this app used to apply at
        // all. Accepting `appendFile` from `@types/node` therefore wrote the identifier and left
        // the file without the import that makes it mean anything: code that does not compile,
        // from a completion the popup had offered. Resolved into this buffer's own offsets here,
        // while it is still in its pre-edit state, because that is the document the server's own
        // line/character coordinates describe.
        let mut import_edits: Vec<(std::ops::Range<usize>, String)> = item
            .additional_text_edits
            .iter()
            .filter(|_| auto_import)
            .flatten()
            .map(|edit| {
                let start =
                    buffer.offset_for_position(edit.range.start.line, edit.range.start.character);
                let end = buffer.offset_for_position(edit.range.end.line, edit.range.end.character);
                (start.min(end)..start.max(end), edit.new_text.clone())
            })
            .collect();
        // Applied after the main edit and in descending document order, so no edit ever shifts a
        // later one's offsets out from under it. The spec guarantees these never overlap the main
        // edit, so ordering is the only real hazard.
        import_edits.sort_by_key(|(edit_range, _)| std::cmp::Reverse(edit_range.start));

        let path = entry.path.clone();
        // Accepting a completion is a programmatic, whole-token edit, not a typed character - one
        // of GitHub issue #17's four named undo-group boundaries. Sealed on both sides so it is
        // its own real step: the partial word typed before it doesn't absorb it, and neither does
        // whatever is typed after. See `crate::text_history`'s own docs for the policy. The import
        // edits sit inside the same seal, because accepting a completion and writing the import it
        // needs are one action to a user and must undo as one.
        buffer.seal_history();
        buffer.replace_range(Some(range), &text);
        // `replace_range` leaves the caret just past whatever it inserted - which is what the user
        // wants for the completion itself, and exactly what must *not* be left behind after an
        // import edit at the top of the file. So the caret the user should end on is remembered
        // here, carried across each import edit by that edit's own length change, and restored.
        // Without it, accepting an auto-import parks the caret up in the `import` line it just
        // wrote.
        let mut caret = buffer.cursor_offset();
        for (edit_range, edit_text) in &import_edits {
            buffer.replace_range(Some(edit_range.clone()), edit_text);
            if edit_range.start <= caret {
                caret = (caret + edit_text.len()).saturating_sub(edit_range.len());
            }
        }
        if !import_edits.is_empty() {
            let caret = caret.min(buffer.content.len());
            buffer.selected_range = caret..caret;
        }
        buffer.seal_history();
        self.schedule_rehighlight(path.clone(), cx);
        // The accepted text routinely still ends in a real identifier character (accepting a
        // bare `println` leaves the caret right after a real `n`) - without this, the very next
        // debounce tick would read as a fresh, completion-worthy keystroke and immediately
        // reopen the popup, filtered down to essentially just the item the user had just picked.
        // See this field's own docs.
        self.completions_suppress_next_trigger = true;
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
        // independently-tracked window-bounds value. Measured against `estimated_popover_height`
        // (a real, tight estimate of what's actually about to paint), not `popover_max_height`
        // (a real, worst-case-only number that made a short, common list flip dozens of pixels
        // higher than its own actual content ever needed - see that function's own docs).
        let estimated_height = estimated_popover_height(&entry.status);
        let space_below = self.body_bounds.bottom() - row_bottom;
        let fits_below = space_below >= estimated_height;
        let top = if fits_below {
            row_bottom
        } else {
            (row_top - estimated_height).max(self.body_bounds.top())
        };

        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::POPOVER;
        let extension = entry.path.extension().and_then(|ext| ext.to_str());
        let macos = self.window_controls_style().is_macos();

        // The list column - always present, whatever the status. `Loading`/`Failed` show a
        // single message row here and nothing else; `Ready` shows the real, virtualized item
        // rows (GitHub issue #185's real scrolling over the GitHub issue #189 client-side
        // filtered/re-ranked `visible` view) plus the mockup's own footer hint row (`README.md`:
        // "footer `⇅ move · ⏎ accept · ⇥ snippet`").
        let mut list_column = gpui::div()
            .flex_none()
            .w(LIST_WIDTH)
            .flex()
            .flex_col()
            // `py(3.0)`, not `py(4.0)`: the design mockup's own completions-list column padding
            // is `3px 0` (`Jerry.dc.html`: `padding:3px 0` on the `290px` list column) - see
            // `POPOVER_VERTICAL_PADDING`'s own docs, which restate the same `3px 0` as one
            // vertical total.
            .py(gpui::px(3.0));

        // The selected item, if any - drives the detail pane below. Read once, outside the
        // `Ready` match arm's own item loop, so both the list rows and the detail pane read the
        // exact same real selection rather than two independently-indexed lookups. Resolved
        // *through* `visible` back into the real server list, matching every other real
        // selection read in this module (see [`CompletionsStatus::Ready::selected`]'s own docs).
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
            CompletionsStatus::Ready {
                items,
                visible,
                selected,
            } => {
                // The *described* item - the merged `completionItem/resolve` response when one has
                // landed, the inline item until then. This is the one place in the popup that
                // reads resolved data: the detail pane is what a resolve is allowed to fill in,
                // and the rows below deliberately are not. See
                // `crate::root::AdeApp::completions_resolved_items`.
                selected_item = visible
                    .get(*selected)
                    .and_then(|index| self.described_completion_item(items, *index));
                // `border-right:1px solid #23282c` in the mockup, on the list column's own right
                // edge (`Jerry.dc.html`: `width:290px;border-right:1px solid #23282c`) - only
                // while there's a real detail pane beside it to divide from.
                if selected_item.is_some() {
                    list_column = list_column.border_r_1().border_color(theme::border::CARD);
                }

                // Real virtualization, not a render cap (GitHub issue #185): `uniform_list` only
                // ever builds the rows genuinely inside its own viewport, so `visible.len()` here
                // is the *whole* real, filtered/re-ranked view (GitHub issue #189) - hundreds of
                // items included when the filter is empty - and every one of them is reachable by
                // keyboard (`Self::move_completions_selection` scrolls the viewport to follow the
                // selection), by mouse wheel, and by the overlay scrollbar below. `index` below is
                // always a position *in `visible`*, matching `selected`'s own indexing convention
                // (see [`CompletionsStatus::Ready::selected`]'s own docs) - never a raw index into
                // the untouched server list. The same `uniform_list` idiom
                // `crate::sidebar::render::AdeApp::render_file_tree` and
                // `crate::code_surface::file_view::AdeApp::render_file_view` already use, and the
                // same one Zed's own completions menu uses for this exact surface
                // (`vendor/zed/crates/editor/src/code_context_menus.rs:929-1155`:
                // `uniform_list(...).max_h(...).track_scroll(...).with_sizing_behavior(Infer)`).
                // Every row is exactly `POPOVER_ROW_HEIGHT` tall, which is `uniform_list`'s one
                // real requirement.
                let list = gpui::uniform_list(
                    "completions-list",
                    visible.len(),
                    cx.processor(
                        move |this: &mut Self,
                              range: std::ops::Range<usize>,
                              _window,
                              cx: &mut Context<Self>| {
                            // Re-read the live state rather than capturing a clone of `items`/
                            // `visible`: this closure runs once per frame, and a real "complete
                            // everything in scope" response is large enough that cloning it every
                            // frame would be a genuine cost. Every index is clamped rather than
                            // trusted, so a future divergence degrades to "renders fewer rows"
                            // instead of panicking.
                            let Some(entry) = this.completions.as_ref() else {
                                return Vec::new();
                            };
                            let CompletionsStatus::Ready {
                                items,
                                visible,
                                selected,
                            } = &entry.status
                            else {
                                return Vec::new();
                            };
                            let selected = *selected;
                            let end = range.end.min(visible.len());
                            let start = range.start.min(end);
                            visible[start..end]
                                .iter()
                                .enumerate()
                                .filter_map(|(offset, item_index)| {
                                    let index = start + offset;
                                    let item = items.get(*item_index)?;
                                    Some(render_completion_row(index, item, index == selected, cx))
                                })
                                .collect::<Vec<_>>()
                        },
                    ),
                )
                // The list's own real height cap. *Some* real maximum has to reach this element,
                // or `ListSizingBehavior::Infer`'s measure function
                // (`vendor/zed/crates/gpui/src/elements/uniform_list.rs:290-313`) lays the list
                // out at its full `row_height * item_count` content height, its viewport equals
                // its content, `max_offset` is zero, and nothing can ever scroll - it would just
                // be clipped by the popover's `overflow_hidden()`, which is exactly the pre-fix
                // bug. Verified by deleting it: with *both* this and the popover's own
                // `max_h(popover_max_height())` gone, `completions_scroll_tests::
                // the_viewport_really_scrolls_to_follow_keyboard_selection` fails on a zero
                // `max_offset`.
                //
                // Honest note on redundancy: in this layout the two are interchangeable - the
                // popover's `max_h` cascades a definite available height into `Infer`'s measure
                // call, so deleting *either one alone* still passes those tests. Both are kept
                // deliberately. The popover's is what the flip-above-the-caret math above already
                // measures against (`popover_max_height()` has to be the popup's real height for
                // that decision to be correct), and this one is what pins the *list's* cap
                // directly to its own node, independent of whatever the parent chain does - the
                // same placement Zed's own completions menu uses
                // (`vendor/zed/crates/editor/src/code_context_menus.rs:1153`).
                .max_h(popover_list_max_height())
                // `Infer` (not the default `Auto`): `Auto` has no measure function at all, so the
                // list's intrinsic height is zero and it must inherit a definite height from a
                // `flex_1` parent - which is wrong here, because this popup has to *shrink to fit*
                // a short list (three real items must paint a 66px-tall popup, not a 264px one
                // with 198px of empty background). `Infer` gives it a real content-derived height,
                // clamped by the `max_h` above.
                .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
                .track_scroll(&self.completions_scroll_handle);

                // The scrollbar is a *sibling* of the list inside its own non-scrolling
                // `.relative()` wrapper, never a child of the list itself - see
                // `crate::sidebar::render::AdeApp::render_file_tree`'s own docs for the real
                // reason (GPUI applies a scrolling element's own scroll translation to *every*
                // child, absolutely-positioned ones included, so a scrollbar painted inside would
                // scroll away with the rows). `render_vertical_scrollbar` returns `None` outright
                // when the handle's own `max_offset` shows the content genuinely doesn't overflow,
                // so a short list paints no scrollbar at all.
                list_column = list_column.child(
                    gpui::div()
                        .relative()
                        .flex()
                        .flex_col()
                        .min_h_0()
                        // `flex_1()`, not left implicit: `list_column`'s own outer box stretches
                        // to match `render_completion_detail_pane`'s real height whenever the
                        // detail pane's own content (a long signature, a real resolved doc
                        // paragraph) makes it taller than the list side needs - GPUI's default
                        // cross-axis stretch on `popover`'s own row layout, the same way a CSS
                        // flex row would. Without `flex_1()` here, this wrapper (and the footer
                        // hints row below it) just kept their own natural, shorter height inside
                        // that taller stretched box, leaving real, visible empty space between
                        // the footer and the popover's real bottom edge instead of the footer
                        // genuinely sitting flush against it. `flex_1()` is what makes this
                        // wrapper absorb that real extra space instead, so the footer - a
                        // `flex_none()` sibling right after it - lands at the true bottom
                        // regardless of how tall the detail pane makes the row.
                        .flex_1()
                        .child(list)
                        .children(self.render_vertical_scrollbar(
                            "completions-scrollbar",
                            &self.completions_scroll_handle,
                            &[],
                            cx,
                        )),
                );

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
                                (keymap::resolve_combo("\u{2191}\u{2193}", macos), "move"),
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
            .max_h(popover_max_height())
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
            // See `crate::code_surface::lsp_ui::render_hover_card_content`'s own identical
            // `.occlude()` docs for why - a real scroll (over the list column or the detail
            // pane's own doc region) or click over this popover must never also reach the File
            // view content behind it.
            .occlude()
            .child(list_column);

        // The detail pane - `border-right` lives on the list column's own right edge in the
        // mockup (`Jerry.dc.html`: `border-right:1px solid #23282c` on the 290px list), so it's
        // applied to `list_column` above only when there's a real detail pane beside it to
        // divide from; a lone list column (Loading/Failed) has no seam to draw.
        if let Some(item) = selected_item {
            popover = popover.child(self.render_completion_detail_pane(item, extension, cx));
        }

        Some(popover.into_any_element())
    }
}

/// One real completion row, built for whichever indices [`AdeApp::render_completions_popover`]'s
/// `uniform_list` asked for this frame. Extracted out of that method's own body when the popup
/// gained real scrolling (GitHub issue #185) purely because a `uniform_list` builds its rows
/// inside a `cx.processor` closure rather than inline - the row's own markup, padding, colors and
/// click-to-accept behavior are unchanged.
fn render_completion_row(
    index: usize,
    item: &lsp_core::lsp_types::CompletionItem,
    is_selected: bool,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    // `item` here is the server's own untouched response entry - never a resolve-merged one (see
    // `crate::root::AdeApp::completions_resolved_items`), so everything this row paints is known
    // the instant the popup opens and none of it can change under the user afterwards.
    let label = item.label.clone();
    let row_hint = completion_view::completion_row_hint(item);
    let kind_badge = completion_view::completion_kind_badge(item.kind);
    gpui::div()
        .id(("completion-item", index))
        // Test-only (a no-op outside test builds, like every other `debug_selector` in this
        // codebase) - lets a real render test read a row's own painted bounds back with
        // `VisualTestContext::debug_bounds` and prove which rows `uniform_list` genuinely
        // painted this frame, rather than trusting that scrolling "probably" happened.
        .debug_selector(move || format!("completion-item-{index}"))
        .flex_none()
        .w_full()
        .h(POPOVER_ROW_HEIGHT)
        // `px(8.0)`, not `px(10.0)` - the design mockup's own completion-item row padding is
        // `0 8px` (`Jerry.dc.html`: `padding:0 8px` on each `.completions` row).
        .px(gpui::px(8.0))
        .flex()
        .items_center()
        .gap(gpui::px(8.0))
        .cursor_pointer()
        // `theme::completions_popup::ITEM_SELECTED_BG` (`#243c50`), not
        // `theme::surface::CURRENT_LINE` (`#181c20`) - `CURRENT_LINE` is the exact same hex as
        // this popover's own background (`theme::surface::POPOVER`), so the "selected" row
        // highlight used to be genuinely invisible; see that token's own docs.
        .when(is_selected, |el| {
            el.bg(theme::completions_popup::ITEM_SELECTED_BG)
        })
        .children(kind_badge.map(render_completion_kind_badge))
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                // A real item label can be longer than the row is wide (a fully-qualified
                // constructor, a long generic instantiation) - `.truncate()` (`overflow_hidden`
                // + `whitespace_nowrap` + `text_ellipsis`) keeps it on the row's own single line
                // instead of wrapping, which would grow the row past `POPOVER_ROW_HEIGHT` and
                // desync every row after it in the virtualized list (each row is painted at a
                // fixed `POPOVER_ROW_HEIGHT` offset, so one row quietly growing taller than that
                // just overlaps the next one rather than pushing it down).
                .truncate()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(if is_selected {
                    theme::completions_popup::ITEM_SELECTED_FG
                } else {
                    theme::completions_popup::ITEM_FG
                })
                .child(label),
        )
        // The row's one secondary string: where this item comes from, or the signature the server
        // sent inline when it named no origin - see `completion_view::completion_row_hint` for
        // which, per server, and for why the *type* is deliberately not this. It is read off the
        // server's untouched response, so it is here the instant the popup opens and no resolve
        // can rewrite it.
        .children(row_hint.map(|hint| {
            gpui::div()
                .id(("completion-item-detail", index))
                // Lets a real test measure this real span's own painted bounds (`debug_bounds`
                // reads this, not `.id(..)`) - a no-op outside test builds, matching every other
                // `debug_selector` in this crate.
                .debug_selector(move || format!("completion-item-{index}-detail"))
                .flex_none()
                // Same real overflow the label just above needs to guard against, on the
                // right-hand hint instead - a real, unbounded signature (a deeply nested generic,
                // a long tuple) capped to a reasonable share of the row rather than left free to
                // push the row's total content wider than the popup itself.
                .max_w(gpui::px(120.0))
                .truncate()
                .text_size(gpui::px(10.0))
                .text_color(theme::text::GHOST)
                .child(hint)
        }))
        // A real click both selects *and* accepts this row in one step - `on_mouse_down`, not
        // `on_click`, matching this app's own established idiom for a popover row that both
        // selects and immediately commits (`crate::work_surface::render::render_dropdown_menu_row`'s
        // sibling rows use `on_click` since picking one never needs an intermediate "select"
        // state the way this popup's keyboard nav does).
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                if let Some(entry) = this.completions.as_mut() {
                    if let CompletionsStatus::Ready { selected, .. } = &mut entry.status {
                        *selected = index;
                    }
                }
                this.accept_active_completion(window, cx);
                cx.stop_propagation();
            }),
        )
        .into_any_element()
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

impl AdeApp {
    /// The Completions popup's own detail pane - the design mockup's real, 300px right column
    /// (`design_handoff_jerry_ade/revision 3/Jerry.dc.html`: `width:300px;padding:8px 10px`),
    /// describing whichever item is currently selected: a syntax-highlighted signature line, doc
    /// prose, and a module-path footer - mirroring `crate::code_surface::lsp_ui`'s Hover card
    /// exactly (same three-piece shape, same reasoning for showing it), just for the selected
    /// completion item instead of a `textDocument/hover` response.
    ///
    /// Design-review follow-up: this pane didn't exist at all before - the popup was list-only, a
    /// real, previously undocumented-to-the-user scope gap (see [`DETAIL_WIDTH`]'s own docs).
    ///
    /// A method, not a free function, since the signature+doc region below now needs
    /// [`Self::completions_detail_scroll_handle`] and [`crate::root::scrollbar::
    /// AdeApp::render_vertical_scrollbar`], both of which need `&self` - the identical reason
    /// `crate::code_surface::lsp_ui::AdeApp::render_hover_card_content` stopped being a free
    /// function.
    fn render_completion_detail_pane(
        &self,
        item: &lsp_core::lsp_types::CompletionItem,
        extension: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut pane = gpui::div()
            .id("completions-detail-pane")
            // Lets a real test measure this real pane's own painted bounds (`debug_bounds` reads
            // this, not `.id(..)`) - a no-op outside test builds, matching every other
            // `debug_selector` in this crate.
            .debug_selector(|| "completions-detail-pane".to_string())
            .flex_none()
            .w(DETAIL_WIDTH)
            .flex()
            .flex_col()
            // A real ceiling on the pane's own total height, matching the popup's own overall
            // budget - without it, a genuinely multi-line signature (typescript-language-server
            // pretty-printing a wide utility/generic type like `Pick<{...}>` across several real
            // lines, now that it renders in full instead of being truncated to its own first
            // line) could grow the pane past the popup's own `overflow_hidden()` clip and hide
            // the module-path footer beneath it - the same real bug the Hover card had.
            .max_h(popover_max_height())
            // No `.px(...)` here (an earlier version of this fix had one, applied uniformly to
            // the whole pane) - matching `crate::code_surface::lsp_ui::render_hover_card_content`'s
            // own three independently-padded bands exactly: each section below carries its own
            // `.px(10.0)` instead, so the signature's own real bottom border can span the pane's
            // full real width edge-to-edge, the way the Hover card's own header border does,
            // rather than stopping short at a real, visible gap where the pane's uniform padding
            // used to inset it on both sides.
            .py(gpui::px(8.0));

        // Signature: see [`completion_view::completion_signature_text`]'s own docs for the real
        // `label_details.detail`-first, `item.detail`-fallback, bare-`label`-last precedence - the
        // bare label is never left blank for a real, selected item even before a real
        // `completionItem/resolve` round trip (`AdeApp::maybe_resolve_selected_completion_item`)
        // fills in a bare `label`/`kind`-only item's real `detail`. Highlighted the same real way
        // `crate::code_surface::code_view::highlight_block` highlights any other standalone
        // fragment (a diff hunk, a merge conflict side) - see that function's own docs.
        let signature_text = completion_view::completion_signature_text(item);
        // One real stacked row per source line in `signature_text`, not a single `flex_wrap` row for
        // the whole thing - a genuinely multi-line signature (e.g. typescript-language-server pretty-
        // printing a wide utility/generic type like `Pick<{...}>` across several real lines) has no
        // way to show a real line break inside one `flex_wrap` row, since wrapping there is a width
        // overflow, not a semantic newline. Mirrors `crate::code_surface::lsp_ui::render_hover_signature`'s
        // own fix for the identical bug: consuming only `highlight_block`'s first `RenderedLine` used
        // to silently drop every real line past the first.
        let signature_lines = code_view::highlight_block(
            std::iter::once(signature_text.as_str()),
            extension,
            code_view::HighlightOptions::default(),
        );
        let mut signature_column = gpui::div()
            .id("completion-detail-signature-column")
            // Lets a real test measure this real column's own painted bounds (`debug_bounds`
            // reads this, not `.id(..)`) - a no-op outside test builds, matching every other
            // `debug_selector` in this crate.
            .debug_selector(|| "completion-detail-signature-column".to_string())
            .flex()
            .flex_col()
            .font(gpui::font(theme::font::MONO))
            .text_size(gpui::px(11.0))
            .px(gpui::px(10.0))
            // A real seam between the signature and the doc/footer below it, matching
            // `crate::code_surface::lsp_ui::render_hover_card_content`'s own header exactly
            // (`.pb(px(6.0)).border_b_1().border_color(theme::border::CARD)`) - this pane never
            // had one at all before, unlike the Hover card it otherwise mirrors band-for-band.
            .pb(gpui::px(6.0))
            .border_b_1()
            .border_color(theme::border::CARD);
        let mut run_index = 0usize;
        for line in signature_lines {
            let mut signature_row = gpui::div().flex().flex_wrap();
            for (run_text, kind) in line.runs {
                let index = run_index;
                run_index += 1;
                signature_row = signature_row.child(
                    gpui::div()
                        .id(("completion-detail-signature-token", index))
                        // Lets a real test measure this real token's own painted bounds
                        // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test builds,
                        // matching every other `debug_selector` in this crate.
                        .debug_selector(move || {
                            format!("completion-detail-signature-token-{index}")
                        })
                        .text_color(code_view::color_for_kind(kind))
                        .child(run_text),
                );
            }
            signature_column = signature_column.child(signature_row);
        }

        // The scrollable region: the signature (now potentially many real lines tall) plus the
        // doc paragraph, wrapped in a real `overflow_y_scroll()` area rather than left to grow
        // the pane without bound. `.flex_1().min_h_0()` directly on this element, not just on its
        // `.relative()` wrapper below - a flex item's default `min-height: auto` otherwise
        // refuses to shrink below its own content's natural size, which would silently defeat
        // `overflow_y_scroll()` here (see `crate::code_surface::lsp_ui::render_hover_card_content`'s
        // own docs for the identical real GPUI gotcha this mirrors, and
        // `crate::rail::render`'s own `"agent-rail-list"` scrollable list for the working
        // precedent both follow).
        let mut scroll_body = gpui::div()
            .id("completions-detail-scroll-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // The overlay scrollbar below reads its geometry straight off this same handle.
            .track_scroll(&self.completions_detail_scroll_handle)
            .child(signature_column);

        if let Some(doc) = completion_view::completion_documentation_text(item) {
            scroll_body = scroll_body.child(
                gpui::div()
                    .mt(gpui::px(7.0))
                    .px(gpui::px(10.0))
                    .font(gpui::font(theme::font::SANS))
                    .text_size(gpui::px(11.0))
                    // No `.line_clamp(...)` here (an earlier version of this fix kept one) - a
                    // real doc comment can run to many paragraphs (rustdoc examples, long prose),
                    // and clamping it silently truncated the rest with no way to reach it at all:
                    // `.line_clamp` bounds the div's own painted height directly, so it never
                    // actually overflows the scroll region above into something the real
                    // scrollbar could reach. Matches `crate::code_surface::lsp_ui::
                    // render_hover_card_content`'s own doc paragraph, which has never clamped -
                    // real overflow, from either the signature or the doc, is what the scroll
                    // region and its scrollbar exist to handle.
                    .child(crate::code_surface::lsp_ui::render_doc_sections(
                        &doc,
                        theme::text::DIMMER,
                        extension,
                    )),
            );
        }

        pane = pane.child(
            gpui::div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(scroll_body)
                .children(self.render_vertical_scrollbar(
                    "completions-detail-scrollbar",
                    &self.completions_detail_scroll_handle,
                    &[],
                    cx,
                )),
        );

        if let Some(module_path) = completion_view::completion_module_path(item) {
            pane = pane.child(
                gpui::div()
                    .id("completion-detail-module-path")
                    // Lets a real test measure this real footer's own painted bounds
                    // (`debug_bounds` reads this, not `.id(..)`) - a no-op outside test builds,
                    // matching every other `debug_selector` in this crate.
                    .debug_selector(|| "completion-detail-module-path".to_string())
                    .flex_none()
                    .mt(gpui::px(9.0))
                    .pt(gpui::px(7.0))
                    .px(gpui::px(10.0))
                    .border_t_1()
                    .border_color(theme::border::CARD)
                    .font(gpui::font(theme::font::MONO))
                    .text_size(gpui::px(10.0))
                    .text_color(theme::text::GHOST)
                    // A real, fully-qualified module path can be long enough to wrap onto a
                    // second line on its own - `.truncate()` keeps this footer the real, fixed
                    // single line the design's own mockup shows.
                    .truncate()
                    .child(module_path),
            );
        }

        pane.into_any_element()
    }
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

/// GitHub issue #185's direct regression coverage: the popup used to hard-cap rendering at
/// `MAX_RENDERED_COMPLETION_ITEMS` (12) with no scroll mechanism at all, so every item past the
/// twelfth was unreachable by keyboard *and* by mouse. These tests drive the real, painted popup
/// in a real GPUI test window - real keystrokes through the real key bindings, real
/// `uniform_list` virtualization, real `gpui::UniformListScrollHandle` geometry - rather than
/// asserting on the selection index alone, which would prove nothing about whether the selected
/// row is actually on screen.
///
/// The `Ready` popup state is seeded directly rather than driven through a real language server,
/// matching `crate::code_surface::tabs::stale_completions_popup_tests`' own established precedent
/// (`crate::lsp::client::lsp_diagnostics_wiring_tests` owns the real, live end-to-end
/// rust-analyzer completion proof this module doesn't duplicate). What's under test here is the
/// popup's own scrolling, which is entirely independent of where the items came from.
#[cfg(test)]
mod completions_scroll_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::root::scrollbar::ScrollableHandle;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    /// Comfortably more than both the old 12-item render cap and the
    /// [`MAX_VISIBLE_COMPLETION_ROWS`] viewport, so "reachable" and "scrolled into view" are two
    /// genuinely different claims.
    const LONG: usize = 40;
    /// Fewer than one viewport's worth - the "doesn't break, doesn't grow a scrollbar" case.
    const SHORT: usize = 3;

    fn fake_items(count: usize) -> Vec<lsp_core::lsp_types::CompletionItem> {
        (0..count)
            .map(|index| lsp_core::lsp_types::CompletionItem {
                label: format!("item_{index:03}"),
                ..Default::default()
            })
            .collect()
    }

    /// Opens a real editable file in a real test window, paints it (which is what populates the
    /// `AdeApp::file_view_last_bounds`/`file_view_last_layout` caret layout
    /// [`AdeApp::render_completions_popover`] anchors to - without a real paint first the popover
    /// honestly renders nothing at all), then seeds a real `Ready` popup of `count` items for it
    /// and paints again.
    ///
    /// The returned `TempDir` is the file's own real backing directory and must be held for the
    /// lifetime of the test - dropping it deletes the file out from under the open buffer.
    fn open_with_seeded_popup(
        cx: &mut TestAppContext,
        count: usize,
    ) -> (
        Entity<AdeApp>,
        &mut VisualTestContext,
        tempfile::TempDir,
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
                // Empty query - every item stays visible, in the real server's own order (see
                // `completion_view::completion_match`'s own docs), matching what this helper's
                // real callers (real scroll-position/virtualization tests) need: an unshifted,
                // predictable `visible` for their own index-based assertions.
                status: CompletionsStatus::ready(fake_items(count), "")
                    .expect("a real, non-empty item list must produce a real Ready state"),
            });
            cx.notify();
        });
        cx.run_until_parked();
        // A second real frame: `AdeApp::render_vertical_scrollbar` reads its geometry off the
        // scroll handle's *last painted* bounds/`max_offset`, so the very first frame after a
        // list appears never has a scrollbar yet, by design (see that method's own docs).
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();
        (app, cx, repo, relative)
    }

    fn selected(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> usize {
        app.read_with(cx, |app, _| {
            let entry = app
                .completions
                .as_ref()
                .expect("popup should still be open");
            match &entry.status {
                CompletionsStatus::Ready { selected, .. } => *selected,
                other => panic!("expected a Ready popup, got {other:?}"),
            }
        })
    }

    fn scroll_offset(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> gpui::Pixels {
        app.read_with(cx, |app, _| {
            app.completions_scroll_handle.base_handle().offset().y
        })
    }

    fn max_scroll_offset(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> gpui::Pixels {
        app.read_with(cx, |app, _| {
            app.completions_scroll_handle.base_handle().max_offset().y
        })
    }

    /// The load-bearing assertion for issue #185: with a real 40-item response, `down` must reach
    /// every one of the 40 - the old `rem_euclid(items.len().min(12))` wrap made items 13..40
    /// permanently unreachable, silently, with no error and no feedback.
    #[gpui::test]
    fn keyboard_navigation_reaches_every_item_past_the_old_twelve_item_cap(
        cx: &mut TestAppContext,
    ) {
        let (app, cx, _repo, _relative) = open_with_seeded_popup(cx, LONG);

        for expected in 1..LONG {
            cx.simulate_keystrokes("down");
            assert_eq!(
                selected(&app, cx),
                expected,
                "a real `down` keystroke must advance the popup's selection one row at a time \
                 all the way through the real, live-returned item list - under the old 12-item \
                 render cap this wrapped back to 0 at row 12 and rows 12..{LONG} were \
                 unreachable"
            );
        }

        cx.simulate_keystrokes("down");
        assert_eq!(
            selected(&app, cx),
            0,
            "one more `down` past the genuinely last item must wrap to the first"
        );
        cx.simulate_keystrokes("up");
        assert_eq!(
            selected(&app, cx),
            LONG - 1,
            "`up` from the first item must wrap to the genuinely last one, not to the twelfth"
        );
    }

    /// Reaching an item is only half the fix - the viewport has to follow, or item 39 is
    /// "selected" while the popup still paints rows 0..12. This asserts on the real
    /// `uniform_list` geometry and on which rows genuinely painted, not on the index.
    #[gpui::test]
    fn the_viewport_really_scrolls_to_follow_keyboard_selection(cx: &mut TestAppContext) {
        let (app, cx, _repo, _relative) = open_with_seeded_popup(cx, LONG);

        assert!(
            cx.debug_bounds("completion-item-0").is_some(),
            "sanity check: the first real row must have painted - if it didn't, this test isn't \
             exercising a real popup at all"
        );
        assert!(
            cx.debug_bounds("completion-item-39").is_none(),
            "sanity check: the last of {LONG} rows must be genuinely virtualized away while the \
             list is scrolled to the top - {MAX_VISIBLE_COMPLETION_ROWS} rows fit in the viewport"
        );
        // Exactly `MAX_VISIBLE_COMPLETION_ROWS` rows visible, proved against the real painted
        // frame rather than against the arithmetic that produced the height. The `11`/`12` here
        // are that constant minus one and that constant - `VisualTestContext::debug_bounds` takes
        // a `&'static str`, so they cannot be interpolated.
        assert_eq!(
            MAX_VISIBLE_COMPLETION_ROWS, 12,
            "the two selectors below are hard-coded to it"
        );
        assert!(
            cx.debug_bounds("completion-item-11").is_some(),
            "the twelfth row must fit entirely inside the popup's real viewport"
        );
        assert!(
            cx.debug_bounds("completion-item-12").is_none(),
            "and the thirteenth must not - that is what makes \
             MAX_VISIBLE_COMPLETION_ROWS a real, painted viewport size rather than a number in a \
             doc comment"
        );
        assert_eq!(
            cx.debug_bounds("completions-popover")
                .expect("the popup itself must have painted")
                .size
                .height,
            popover_max_height(),
            "a long list must cap the popup at exactly {MAX_VISIBLE_COMPLETION_ROWS} rows plus \
             its own padding - a completions popup that grows to fill the screen is as unusable \
             as one that truncates"
        );
        assert!(
            max_scroll_offset(&app, cx) > gpui::px(0.0),
            "{LONG} real rows of {POPOVER_ROW_HEIGHT:?} must genuinely overflow the \
             {:?} viewport - a zero `max_offset` would mean the list laid out at full content \
             height and nothing can ever scroll",
            popover_list_max_height()
        );
        assert_eq!(
            scroll_offset(&app, cx),
            gpui::px(0.0),
            "a freshly opened popup must start at the top"
        );
        assert!(
            cx.debug_bounds("completions-scrollbar").is_some(),
            "a genuinely overflowing list must paint this app's own real overlay scrollbar, the \
             same one every other scrollable region here uses"
        );

        for _ in 0..(LONG - 1) {
            cx.simulate_keystrokes("down");
        }
        cx.run_until_parked();

        assert_eq!(selected(&app, cx), LONG - 1, "sanity check");
        assert!(
            scroll_offset(&app, cx) < gpui::px(0.0),
            "keyboard nav down to the last item must have really scrolled the viewport (GPUI's \
             own scroll offset goes negative as you scroll down), not merely moved an index"
        );
        assert!(
            cx.debug_bounds("completion-item-39").is_some(),
            "the selected row must genuinely be painted after scrolling to it - this is the \
             whole point of `ScrollStrategy::Nearest` in `move_completions_selection`"
        );
        assert!(
            cx.debug_bounds("completion-item-0").is_none(),
            "and the first row must have scrolled genuinely out of the viewport, proving the \
             `uniform_list` is really virtualizing rather than painting all {LONG} at once"
        );

        // Wrapping back around to the first item must scroll back to it, too.
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(
            selected(&app, cx),
            0,
            "sanity check: wrapped to the first item"
        );
        assert_eq!(
            scroll_offset(&app, cx),
            gpui::px(0.0),
            "wrapping to the first item must scroll the viewport back to the top with it"
        );
        assert!(
            cx.debug_bounds("completion-item-0").is_some(),
            "the first row must be painted again after wrapping back to it"
        );
    }

    /// A row that only exists past the old cap must be clickable, too - `uniform_list` builds a
    /// real, hit-testable element for it once it's scrolled into view, which the old truncated
    /// render never did. Clicks the row's own real painted bounds.
    #[gpui::test]
    fn a_row_past_the_old_cap_can_be_accepted_with_a_real_mouse_click(cx: &mut TestAppContext) {
        let (app, cx, _repo, relative) = open_with_seeded_popup(cx, LONG);

        for _ in 0..(LONG - 1) {
            cx.simulate_keystrokes("down");
        }
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("completion-item-39")
            .expect("the last row must be painted after scrolling to it");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "a real click on a real completion row must accept it and close the popup"
        );
        let content = app.read_with(cx, |app, _| {
            app.edit_buffer(&relative)
                .expect("a real buffer")
                .content
                .clone()
        });
        assert!(
            content.contains("item_039"),
            "clicking the 40th row must splice *that* item's real text into the real buffer - \
             under the old 12-item cap this row was never painted, so it could never be clicked \
             at all. Got: {content:?}"
        );
    }

    /// The other half of the ask: a list that comfortably fits must not grow a scrollbar, must
    /// not scroll, and must paint every one of its rows.
    #[gpui::test]
    fn a_short_completions_list_neither_scrolls_nor_paints_a_scrollbar(cx: &mut TestAppContext) {
        let (app, cx, _repo, _relative) = open_with_seeded_popup(cx, SHORT);

        for selector in [
            "completion-item-0",
            "completion-item-1",
            "completion-item-2",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "every row of a short list must paint - {selector} did not"
            );
        }
        assert_eq!(
            cx.debug_bounds("completions-popover")
                .expect("the popup itself must have painted")
                .size
                .height,
            POPOVER_ROW_HEIGHT * SHORT as f32
                + POPOVER_VERTICAL_PADDING
                + POPOVER_BORDER_HEIGHT
                + POPOVER_FOOTER_HEIGHT,
            "a short list must shrink the popup to exactly its own {SHORT} rows plus padding plus \
             the real footer hint row - not leave {MAX_VISIBLE_COMPLETION_ROWS} rows' worth of \
             empty popover background below them"
        );
        assert!(
            max_scroll_offset(&app, cx) <= gpui::px(0.5),
            "{SHORT} real rows must not overflow the {:?} viewport at all - a non-zero \
             `max_offset` here would mean the list laid itself out taller than its own content, \
             i.e. `ListSizingBehavior::Infer` isn't shrinking the popup to fit",
            popover_list_max_height()
        );
        assert!(
            cx.debug_bounds("completions-scrollbar").is_none(),
            "a list that genuinely doesn't overflow must paint no scrollbar - \
             `render_vertical_scrollbar` returns `None` off the same real `max_offset` asserted \
             above"
        );

        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("down");
        assert_eq!(
            selected(&app, cx),
            2,
            "keyboard nav must still work normally in a short list"
        );
        assert_eq!(
            scroll_offset(&app, cx),
            gpui::px(0.0),
            "navigating within a list that fits must never scroll it"
        );
        cx.simulate_keystrokes("down");
        assert_eq!(
            selected(&app, cx),
            0,
            "and it must still wrap at the genuine end of a short list"
        );
    }
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
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext, PathBuf) {
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
                // Empty query - every item stays visible, matching this helper's real callers'
                // need for an unshifted, predictable `visible` for their own assertions (mirrors
                // `completions_scroll_tests::open_with_seeded_popup`'s own reasoning).
                status: CompletionsStatus::ready(items, "")
                    .expect("a real, non-empty item list must produce a real Ready state"),
            });
            cx.notify();
        });
        cx.run_until_parked();
        (app, cx, relative)
    }

    /// GitHub issue #200's rendered-side coverage: a real completion item whose documentation
    /// contains a real JSDoc-style block tag must paint each tag as its own real, separately-
    /// coloured `render_doc_prose` run, mirroring `crate::code_surface::lsp_ui`'s identical hover
    /// coverage - the two real places this shared render helper is called from.
    #[gpui::test]
    fn a_real_jsdoc_tag_in_the_completion_doc_body_paints_its_own_tag_run(cx: &mut TestAppContext) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn push_str(&mut self, string: &str)".to_string()),
            documentation: Some(lsp_core::lsp_types::Documentation::String(
                "Appends a given string slice. See {@link String::push} for more.".to_string(),
            )),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        assert!(
            cx.debug_bounds("doc-prose-tag-0").is_some(),
            "a real inline {{@link ...}} tag inside the completion doc body's own description \
             must still paint its own real `doc-prose-tag` run, even after block tags moved into \
             their own real sections"
        );
    }

    /// GitHub issue #200's own real "params/returns/example ... displayed like code in their own
    /// section" ask, mirroring `crate::code_surface::lsp_ui`'s identical hover coverage: a real
    /// `@param`/`@example` block tag inside a completion item's own documentation must paint as
    /// its own real, structured section here too, not just differently-coloured inline text.
    #[gpui::test]
    fn real_jsdoc_block_tags_in_the_completion_doc_body_paint_their_own_structured_sections(
        cx: &mut TestAppContext,
    ) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn push_str(&mut self, string: &str)".to_string()),
            documentation: Some(lsp_core::lsp_types::Documentation::String(
                "Appends a given string slice.\n\n@param string the slice to append\n@example\n\
                 s.push_str(\"abc\")"
                    .to_string(),
            )),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        assert!(
            cx.debug_bounds("doc-param-row-0").is_some(),
            "a real @param tag must paint its own real parameter row"
        );
        assert!(
            cx.debug_bounds("doc-example-block").is_some(),
            "a real @example tag must paint its own real, syntax-highlighted code block"
        );
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
        let detail_pane = cx.debug_bounds("completions-detail-pane").expect(
            "a real selected item with real detail/documentation must paint a real \
                     detail pane, not silently stay list-only",
        );
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

    /// Direct regression coverage for the real, live-reported "position is not right and can be
    /// super high compared to the actual typing location" bug: when the popup has to flip above
    /// the caret (no real room below), the pre-fix version always positioned it as if a full
    /// `MAX_VISIBLE_COMPLETION_ROWS`-row list were about to paint - `popover_max_height()`'s own
    /// worst case - even for a real, short, filtered list. That left a large, real, visibly wrong
    /// gap between the flipped-above popup and the caret row it's meant to anchor to. Proven by
    /// forcing a real flip (a real multi-line file, caret on its last line, a genuinely short
    /// window) and checking the real gap between the popup and the caret row against
    /// `estimated_popover_height` - the tight, real estimate - rather than the old worst case.
    #[gpui::test]
    fn a_short_lists_flipped_above_position_is_close_to_the_caret_not_the_worst_case_height(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file = repo.path().join("sample.rs");
        let lines: Vec<String> = (0..30).map(|i| format!("fn f{i}() {{}}")).collect();
        std::fs::write(&file, lines.join("\n") + "\n").expect("write sample.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file, window, cx);
        });
        cx.run_until_parked();

        // A genuinely short window, so there's real, deliberately insufficient room below the
        // caret's own last-line row for the popup to fit without flipping, under either the old
        // or the new logic.
        cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(350.0)));
        cx.run_until_parked();

        let relative = PathBuf::from("sample.rs");
        app.update(cx, |app, cx| {
            let buffer = app.edit_buffer_mut(&relative).expect("a real buffer");
            let last_line_offset = buffer.content.rfind("fn f29").expect("real last line");
            buffer.move_to(last_line_offset);
            app.sync_cursor_and_scroll();
            cx.notify();
        });
        cx.run_until_parked();
        // `scroll_to_item`'s own target is deferred, resolved on the *next* real prepaint (see
        // `Self::move_completions_selection`'s own docs for the identical mechanism) - the first
        // real frame after arming it still paints the old scroll position, so a real caret row
        // this far down the file needs a couple more real, settled frames before its own
        // `AdeApp::file_view_last_bounds`/`file_view_last_layout_for` genuinely reflect it.
        for _ in 0..3 {
            app.update(cx, |_app, cx| cx.notify());
            cx.run_until_parked();
        }

        let row_top = app
            .read_with(cx, |app, _| app.file_view_last_bounds)
            .expect("the real caret row must have painted real bounds")
            .top();

        // A real, short (one-item) `Ready` popup - the common case a filtered list narrows down
        // to, and the one the pre-fix worst-case-height math treated identically to a full
        // 12-item list.
        let short_status = CompletionsStatus::ready(
            vec![lsp_core::lsp_types::CompletionItem {
                label: "short".to_string(),
                ..Default::default()
            }],
            "",
        )
        .expect("a real, non-empty item list must produce a real Ready state");
        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative,
                status: short_status.clone(),
            });
            cx.notify();
        });
        cx.run_until_parked();

        let popover = cx
            .debug_bounds("completions-popover")
            .expect("the real popover must have painted, flipped above the caret");
        assert!(
            popover.top() < row_top,
            "sanity check: the popup must genuinely have flipped above the caret row in this \
             deliberately short window (row top {row_top:?}, popover top {:?})",
            popover.top()
        );

        let real_gap = row_top - popover.top();
        let estimated = estimated_popover_height(&short_status);
        assert!(
            (real_gap - estimated).abs() < gpui::px(5.0),
            "the flipped-above popup's real gap from the caret row must match the tight, real \
             `estimated_popover_height` for this short list, not the old worst-case \
             `popover_max_height()` - real gap {real_gap:?}, estimated {estimated:?}, old \
             worst-case would have been {:?}",
            popover_max_height()
        );
        assert!(
            popover_max_height() - real_gap > gpui::px(50.0),
            "the real gap must be meaningfully smaller than the old worst-case height - a short, \
             one-item list positioned as if it were a full {MAX_VISIBLE_COMPLETION_ROWS}-row list \
             is exactly the real, reported \"super high\" bug"
        );
    }

    /// A real completion row's own selected/hover background (`theme::completions_popup::
    /// ITEM_SELECTED_BG`) must cover the *whole* row, not just as much of it as the label text
    /// happens to need - an earlier version left `.w_full()` off the row `div()` entirely, so the
    /// row (and therefore its background) shrank to its own content width instead of stretching
    /// to fill `LIST_WIDTH`, leaving a real, visible gap of unhighlighted background to the right
    /// of a short label.
    #[gpui::test]
    fn a_selected_rows_background_spans_the_full_list_width_not_just_its_text(
        cx: &mut TestAppContext,
    ) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "x".to_string(),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        let row = cx
            .debug_bounds("completion-item-0")
            .expect("the real selected row must have painted");
        // Within 2px of `LIST_WIDTH`, not exactly it: this fixture's own single item is a real
        // selected item with nothing to show in the detail pane's own signature slot, so
        // `list_column`'s conditional `border_r_1()` (only added once a real detail pane sits
        // beside it) is present here too, and border-box sizing takes that real 1px out of the
        // row's own available content width.
        assert!(
            (row.size.width - LIST_WIDTH).abs() <= gpui::px(2.0),
            "a real completion row - and therefore its selected/hover background - must span \
             the full real list column width, not shrink to fit a short label's own text width \
             (got {:?}, expected close to {:?})",
            row.size.width,
            LIST_WIDTH
        );
    }

    /// Accepting an auto-import must write the import, not just the name.
    ///
    /// Verbatim from a live `typescript-language-server`, resolving the `appendFile` candidate the
    /// duplicate report was about: alongside the completion itself it returns
    /// `additionalTextEdits: [{range: 1:0-1:0, newText: "import { appendFile } from 'fs';\n"}]`.
    /// Nothing in this app applied that field, so accepting the row inserted `appendFile` into a
    /// file with no import of it - an identifier that does not resolve, written by the editor
    /// itself. It is also what makes collapsing several import candidates into one row honest:
    /// the surviving row names a module, and now genuinely adds that module's import.
    ///
    /// Also pins the caret. The import lands *above* the caret, so every character it inserts
    /// shifts the whole document under it; leaving the caret where `replace_range` put it would
    /// park it back inside the `import` line it had just written.
    #[gpui::test]
    fn accepting_a_real_auto_import_writes_its_import_line_and_keeps_the_caret(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file = repo.path().join("main.ts");
        std::fs::write(&file, "const a = 1;\n\nconst other = app\n").expect("write main.ts");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file, window, cx);
        });
        cx.run_until_parked();
        let relative = PathBuf::from("main.ts");

        // Caret at the end of `const other = app`, exactly where the popup would have opened.
        let caret_before = app.update(cx, |app, _| {
            let buffer = app.edit_buffer_mut(&relative).expect("a real buffer");
            let caret = buffer.content.find("= app").expect("the fixture line") + "= app".len();
            buffer.selected_range = caret..caret;
            caret
        });

        let item = lsp_core::lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_core::lsp_types::CompletionItemKind::FUNCTION),
            detail: Some("fs".to_string()),
            additional_text_edits: Some(vec![lsp_core::lsp_types::TextEdit {
                range: lsp_core::lsp_types::Range {
                    start: lsp_core::lsp_types::Position::new(1, 0),
                    end: lsp_core::lsp_types::Position::new(1, 0),
                },
                new_text: "import { appendFile } from 'fs';\n".to_string(),
            }]),
            ..Default::default()
        };
        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::ready(vec![item], "app").expect("a real Ready state"),
            });
            cx.notify();
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.accept_active_completion(window, cx);
        });
        cx.run_until_parked();

        let (content, caret_after) = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffer(&relative).expect("a real buffer");
            (buffer.content.clone(), buffer.cursor_offset())
        });
        assert!(
            content.contains("import { appendFile } from 'fs';"),
            "accepting an auto-import has to write the import the server said it needs - without \
             it the editor has just typed an identifier that does not resolve. Got: {content:?}"
        );
        assert!(
            content.contains("const other = appendFile"),
            "and the completion itself still lands at the caret. Got: {content:?}"
        );
        let import_len = "import { appendFile } from 'fs';\n".len();
        let typed_growth = "appendFile".len() - "app".len();
        assert_eq!(
            caret_after,
            caret_before + typed_growth + import_len,
            "the caret must end just past the accepted word, having moved down by exactly what \
             the import inserted above it - not left sitting inside the import line. Got: \
             {content:?}"
        );
    }

    /// The live-requested setting: with `editor.auto_import` off, accepting the very same item
    /// inserts the name and nothing else - no `import` line - while everything else about the
    /// accept is unchanged.
    ///
    /// It exists because a language server offers auto-imports for everything its own index can
    /// reach, which in a browser project includes `@types/node`: `import { appendFile } from
    /// 'node:fs'` is valid TypeScript there (verified against a live server - it raises no
    /// diagnostic at all) and still cannot be bundled.
    #[gpui::test]
    fn the_auto_import_setting_off_inserts_the_name_without_the_import(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file = repo.path().join("main.ts");
        std::fs::write(&file, "const a = 1;\n\nconst other = app\n").expect("write main.ts");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file, window, cx);
        });
        cx.run_until_parked();
        let relative = PathBuf::from("main.ts");

        let item = lsp_core::lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_core::lsp_types::CompletionItemKind::FUNCTION),
            detail: Some("node:fs".to_string()),
            additional_text_edits: Some(vec![lsp_core::lsp_types::TextEdit {
                range: lsp_core::lsp_types::Range {
                    start: lsp_core::lsp_types::Position::new(1, 0),
                    end: lsp_core::lsp_types::Position::new(1, 0),
                },
                new_text: "import { appendFile } from 'node:fs'\n".to_string(),
            }]),
            ..Default::default()
        };
        app.update(cx, |app, cx| {
            app.settings.editor.auto_import = false;
            let buffer = app.edit_buffer_mut(&relative).expect("a real buffer");
            let caret = buffer.content.find("= app").expect("the fixture line") + "= app".len();
            buffer.selected_range = caret..caret;
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::ready(vec![item], "app").expect("a real Ready state"),
            });
            cx.notify();
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.accept_active_completion(window, cx);
        });
        cx.run_until_parked();

        let content = app.read_with(cx, |app, _| {
            app.edit_buffer(&relative)
                .expect("a real buffer")
                .content
                .clone()
        });
        assert!(
            content.contains("const other = appendFile"),
            "the completion itself is still accepted. Got: {content:?}"
        );
        assert!(
            !content.contains("import"),
            "with auto-import off, no import may be written - that is the whole switch. Got: \
             {content:?}"
        );
    }

    /// The live-reported "`appendFile` appears four times", on screen. Node ships every builtin
    /// under two specifiers and `typescript-language-server` offers both, so these two items
    /// (dumped verbatim - see
    /// `completion_view::tests::a_real_auto_import_row_says_which_module_it_comes_from_and_is_not_repeated`)
    /// would write the identical import. One row survives, and it paints the module it comes from.
    #[gpui::test]
    fn repeated_auto_import_candidates_paint_one_row_naming_its_module(cx: &mut TestAppContext) {
        let candidate = |module: &str| lsp_core::lsp_types::CompletionItem {
            label: "appendFile".to_string(),
            kind: Some(lsp_core::lsp_types::CompletionItemKind::FUNCTION),
            detail: Some(module.to_string()),
            ..Default::default()
        };
        let (_app, cx, _relative) =
            seed_ready_popup(cx, vec![candidate("node:fs"), candidate("fs")]);

        assert!(
            cx.debug_bounds("completion-item-0").is_some(),
            "the surviving row must have painted"
        );
        assert!(
            cx.debug_bounds("completion-item-1").is_none(),
            "and the row that would have read `appendFile` a second time must not exist at all - \
             that repeat is the whole report"
        );
        let hint = cx.debug_bounds("completion-item-0-detail").expect(
            "an auto-import row has to name its own module, up front - it is the only thing \
             distinguishing it from the candidates collapsed into it",
        );
        assert!(
            hint.size.width > gpui::px(0.0),
            "the module must be genuinely painted, not a zero-width span"
        );
    }

    /// The load-bearing rule behind the live-reported "all data should be here without needing to
    /// select the suggestion": a row is built from the server's own untouched response, so a
    /// `completionItem/resolve` landing later cannot add anything to it or change it.
    ///
    /// Verbatim from a live `typescript-language-server`: `app` arrives bare
    /// (`{"label":"app","kind":6}`) and only its resolve carries `detail: "const app:
    /// App<Element>"`. This drives that merge through the real path
    /// (`AdeApp::apply_resolved_completion_item`) and then re-reads the painted row.
    #[gpui::test]
    fn a_landed_resolve_fills_the_detail_pane_and_leaves_the_row_alone(cx: &mut TestAppContext) {
        let bare = lsp_core::lsp_types::CompletionItem {
            label: "app".to_string(),
            kind: Some(lsp_core::lsp_types::CompletionItemKind::VARIABLE),
            ..Default::default()
        };
        let (app, cx, relative) = seed_ready_popup(cx, vec![bare.clone()]);

        assert!(
            cx.debug_bounds("completion-item-0-detail").is_none(),
            "sanity check: the server said nothing about this item up front, so its row says \
             nothing up front"
        );

        let generation = app.read_with(cx, |app, _| app.completions_generation);
        app.update(cx, |app, cx| {
            app.apply_resolved_completion_item(
                &relative,
                generation,
                0,
                Ok(lsp_core::lsp_types::CompletionItem {
                    detail: Some("const app: App<Element>".to_string()),
                    ..bare.clone()
                }),
                cx,
            );
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("completion-item-0-detail").is_none(),
            "a resolve must never put anything on a row - a row that fills in once you select it \
             is the reported bug itself"
        );
        app.read_with(cx, |app, _| {
            let entry = app.completions.as_ref().expect("popup still open");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("expected Ready");
            };
            assert_eq!(
                items[0].detail, None,
                "the server's own response must be left exactly as it arrived"
            );
            assert_eq!(
                app.described_completion_item(items, 0)
                    .and_then(|item| item.detail.as_deref()),
                Some("const app: App<Element>"),
                "while the detail pane's own view of that item - the one thing a resolve is for - \
                 genuinely gains the type"
            );
        });
    }

    /// The other side of that: an ordinary item with a real type and no import at all must paint
    /// no import-source span, so the row gains nothing it doesn't genuinely have. Dumped from a
    /// live `rust-analyzer` (`label: "count"`, `kind: FIELD`, `detail: "usize"`).
    #[gpui::test]
    fn an_ordinary_item_with_no_import_paints_no_import_source(cx: &mut TestAppContext) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "count".to_string(),
            kind: Some(lsp_core::lsp_types::CompletionItemKind::FIELD),
            detail: Some("usize".to_string()),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        assert!(
            cx.debug_bounds("completion-item-0-detail").is_some(),
            "sanity check: a real one-word field type still paints in the type slot"
        );
        assert!(
            cx.debug_bounds("completion-item-0-import-source").is_none(),
            "an item that would import nothing must paint no import source - the span exists to \
             carry a real, server-supplied module, never a placeholder"
        );
    }

    /// A real, unusually long detail/type hint (a deeply nested generic, a long tuple return
    /// type) must not be left free to grow the right-hand hint span past a real, bounded width -
    /// unbounded, it would push the row's total content wider than the list column itself
    /// (`flex_none` doesn't shrink), overflowing the popup horizontally. `.max_w(120px)` plus
    /// `.truncate()` caps it at a real, fixed share of the row instead.
    #[gpui::test]
    fn a_very_long_detail_hint_is_capped_to_a_bounded_width_not_left_to_grow(
        cx: &mut TestAppContext,
    ) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "x".to_string(),
            detail: Some(
                "a genuinely long real detail string describing a deeply nested generic return \
                 type that just keeps going and going far past any reasonable row width"
                    .to_string(),
            ),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        let detail_span = cx
            .debug_bounds("completion-item-0-detail")
            .expect("the real detail hint span must have painted for a real, non-empty detail");
        assert!(
            detail_span.size.width <= gpui::px(120.0) + gpui::px(1.0),
            "a real, unusually long detail/type hint must be capped at its own bounded max \
             width, not left to grow with the real text - got {:?}",
            detail_span.size.width
        );
    }

    /// A real, long documentation string (a multi-paragraph rustdoc comment is common) must not
    /// grow the detail pane past the popup's own real maximum height - `.max_h(popover_max_height())`
    /// on the pane itself is the real, hard backstop - and, unlike an earlier version of this fix
    /// that clamped the doc paragraph to 6 visible lines with no way to read the rest, the real
    /// overflow must be reachable through the same real scrollbar a tall signature gets: a doc
    /// this long never fits, so it must show one.
    #[gpui::test]
    fn a_very_long_documentation_string_does_not_grow_the_pane_without_bound(
        cx: &mut TestAppContext,
    ) {
        let long_doc = "This is a genuinely long real documentation paragraph. ".repeat(60);
        let long_doc_item = lsp_core::lsp_types::CompletionItem {
            label: "long_doc".to_string(),
            documentation: Some(lsp_core::lsp_types::Documentation::String(long_doc)),
            ..Default::default()
        };
        let (app, cx, _relative) = seed_ready_popup(cx, vec![long_doc_item]);
        // See `a_tall_signature_keeps_the_module_path_footer_pinned_and_shows_a_real_scrollbar`'s
        // own docs for why this second real frame is needed before the scrollbar assertion below.
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        let pane = cx
            .debug_bounds("completions-detail-pane")
            .expect("the real detail pane must have painted for the long-doc item");

        // Unclamped, 60 real repetitions of that sentence wrapped inside the pane's own ~280px
        // real content width would run to dozens of real lines - hundreds of real pixels tall.
        // `popover_max_height()` (the outer popup's own cap) is comfortably less than that, so a
        // pane genuinely respecting its own `.max_h()` must paint well under it.
        assert!(
            pane.size.height < popover_max_height(),
            "a real, genuinely long documentation string (many repeated sentences) must not \
             grow the detail pane's own painted height past the popup's own real maximum - got \
             pane height {:?}, popover_max_height() {:?}",
            pane.size.height,
            popover_max_height()
        );
        assert!(
            cx.debug_bounds("completions-detail-scrollbar").is_some(),
            "a real, genuinely long documentation string that can't fully fit must be reachable \
             through the real scrollbar, not silently truncated with no way to read the rest"
        );
    }

    /// Direct regression coverage for the real, live-reported bug: when the real detail pane's
    /// own content (a long signature plus a real, resolved doc paragraph) makes it taller than
    /// the list side's own natural content needs, `list_column`'s own box stretches to match it
    /// (GPUI's default cross-axis stretch on `popover`'s own row layout) - but without
    /// `flex_1()` on the scrolling-list wrapper, the footer hints row just kept its own shorter,
    /// natural height inside that taller box, leaving real, visible empty space between the
    /// footer and the popover's true bottom edge instead of sitting flush against it.
    #[gpui::test]
    fn the_footer_hints_row_stays_pinned_to_the_real_bottom_even_when_the_detail_pane_is_taller(
        cx: &mut TestAppContext,
    ) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "x".to_string(),
            detail: Some(
                "fn x(a: i32, b: i32, c: i32, d: i32) -> SomeReallyLongReturnType<WithGenerics, AndEvenMore>"
                    .to_string(),
            ),
            documentation: Some(lsp_core::lsp_types::Documentation::String(
                "A genuinely long real doc paragraph that keeps going. ".repeat(30),
            )),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        let popover = cx
            .debug_bounds("completions-popover")
            .expect("the real popover must have painted");
        let footer = cx
            .debug_bounds("completions-footer-hints")
            .expect("the real footer hints row must have painted for a real Ready popup");

        assert!(
            (popover.bottom() - footer.bottom()).abs() < gpui::px(6.0),
            "the real footer hints row must stay pinned near the popover's own real bottom edge \
             even when the detail pane's own content is much taller than the list side needs - \
             popover bottom {:?}, footer bottom {:?} (gap {:?})",
            popover.bottom(),
            footer.bottom(),
            popover.bottom() - footer.bottom()
        );
    }

    /// Direct regression coverage for the real, reported bug: a genuinely tall signature (the
    /// real shape typescript-language-server produces pretty-printing a wide object/union type
    /// across many real lines, now that it renders in full instead of being truncated to its own
    /// first line) used to grow the detail pane's own content past the popup's `overflow_hidden()`
    /// clip, hiding the module-path footer beneath it - the same real bug the Hover card had. The
    /// module-path footer must stay pinned near the pane's own real bottom regardless of how tall
    /// the signature above it is, and a real scrollbar must appear for the overflowing region.
    #[gpui::test]
    fn a_tall_signature_keeps_the_module_path_footer_pinned_and_shows_a_real_scrollbar(
        cx: &mut TestAppContext,
    ) {
        let tall_signature = (0..30)
            .map(|index| format!("    field_{index}: string;"))
            .collect::<Vec<_>>()
            .join("\n");
        let item = lsp_core::lsp_types::CompletionItem {
            label: "x".to_string(),
            detail: Some(format!("const x: {{\n{tall_signature}\n}}")),
            label_details: Some(lsp_core::lsp_types::CompletionItemLabelDetails {
                detail: None,
                description: Some("alloc::string::String".to_string()),
            }),
            ..Default::default()
        };
        let (app, cx, _relative) = seed_ready_popup(cx, vec![item]);
        // `AdeApp::render_vertical_scrollbar` reads its geometry off the scroll handle's *last
        // painted* bounds/`max_offset` (see that method's own docs) - the very first frame after
        // the tall signature appears never has a scrollbar yet, by design. A second real frame
        // (mirroring `completions_scroll_tests::open_with_seeded_popup`'s own identical settling
        // step) lets that settle before the assertions below read it.
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        let pane = cx
            .debug_bounds("completions-detail-pane")
            .expect("the real detail pane must have painted for the tall-signature item");
        let module_path = cx.debug_bounds("completion-detail-module-path").expect(
            "the real module-path footer must still paint even though the real signature above \
             it is far taller than the pane's own max height",
        );
        assert!(
            (pane.bottom() - module_path.bottom()).abs() < gpui::px(10.0),
            "the module-path footer must stay pinned near the pane's own real bottom edge \
             regardless of how tall the content above it is (pane bottom {:?}, footer bottom \
             {:?}) - the old bug pushed it below the pane's own overflow clip instead",
            pane.bottom(),
            module_path.bottom()
        );
        assert!(
            cx.debug_bounds("completions-detail-scrollbar").is_some(),
            "a real scrollbar must appear for the signature/doc region once its own real content \
             genuinely overflows"
        );
    }

    /// The other half: an ordinary, short signature that fits comfortably within the pane's own
    /// max height must never paint a scrollbar - the common case stays exactly as unadorned as it
    /// always was.
    #[gpui::test]
    fn a_short_signature_paints_no_detail_scrollbar(cx: &mut TestAppContext) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "push_str".to_string(),
            detail: Some("fn push_str(&mut self, string: &str)".to_string()),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        assert!(
            cx.debug_bounds("completions-detail-pane").is_some(),
            "sanity check: the real detail pane must have painted"
        );
        assert!(
            cx.debug_bounds("completions-detail-scrollbar").is_none(),
            "an ordinary short signature must never paint a real scrollbar - only genuinely \
             overflowing content should"
        );
    }

    /// Direct regression coverage for the real, reported bug: the signature column had no
    /// explicit width, so it shrank to fit its own (often much narrower) text - a short signature
    /// like `"fn x()"` left the real `.border_b_1()` seam below it visibly short of the pane's
    /// own real 300px-wide right edge, a real gap on the side the design mockup's own Hover card
    /// equivalent never has.
    #[gpui::test]
    fn the_signature_border_spans_the_full_pane_width_not_just_its_text(cx: &mut TestAppContext) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "x".to_string(),
            detail: Some("fn x()".to_string()),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        let pane = cx
            .debug_bounds("completions-detail-pane")
            .expect("the real detail pane must have painted");
        let signature_column = cx
            .debug_bounds("completion-detail-signature-column")
            .expect("the real signature column must have painted");

        // The pane itself carries no horizontal padding (each section owns its own, matching
        // `render_hover_card_content`'s independently-padded bands) - the column's own outer box
        // must span the pane's real edge-to-edge width, so its own bottom border does too.
        assert!(
            (pane.size.width - signature_column.size.width).abs() < gpui::px(1.0),
            "the real signature column - and so its own real bottom border - must span the \
             pane's full real edge-to-edge width, not just its own short text (pane width {:?}, \
             column width {:?})",
            pane.size.width,
            signature_column.size.width
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

    /// Direct regression coverage for the real, reported bug: a genuinely multi-line `detail` -
    /// the real shape `typescript-language-server` produces for a wide utility/generic type like
    /// `Pick<{ a: string; b: number }, "a">` once it pretty-prints across several lines - used to
    /// render as just its own first line, with every real line after the first newline silently
    /// dropped (the old code took only `highlight_block`'s first `RenderedLine`). A token from a
    /// real *second* line must still paint.
    #[gpui::test]
    fn a_genuinely_multi_line_detail_keeps_every_real_line_not_just_the_first(
        cx: &mut TestAppContext,
    ) {
        let item = lsp_core::lsp_types::CompletionItem {
            label: "x".to_string(),
            detail: Some("const x: Pick<{\n    a: string;\n    b: number;\n}, \"a\">".to_string()),
            ..Default::default()
        };
        let (_app, cx, _relative) = seed_ready_popup(cx, vec![item]);

        assert!(
            cx.debug_bounds("completion-detail-signature-token-0")
                .is_some(),
            "sanity check: the first real line's own first token must have painted"
        );
        // The first real line ("const x: Pick<{") tokenizes into exactly 8 runs (indices 0..7),
        // so index 9 - the real "a" identifier in "a: string;" - is unambiguously on the real
        // second line, past the exact boundary the old `.next()` truncation dropped everything
        // after.
        assert!(
            cx.debug_bounds("completion-detail-signature-token-9")
                .is_some(),
            "a token from a real line past the first newline (\"a\" in \"a: string;\", the real \
             second line) must still paint, not have been silently dropped with the rest of the \
             signature past the first line"
        );
    }
}
