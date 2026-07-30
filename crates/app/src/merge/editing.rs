//! Real hand-editing for Surface D's merge-conflict resolution flow (Revision R8.5c) - the
//! whole-file, marker-visible editable view [`AdeApp::render_merge_edit_view`] renders in place
//! of the read-only two-column quick-pick view (`crate::merge::render::
//! render_conflict_columns`) once [`AdeApp::start_merge_hand_edit`] toggles hand-edit mode on for
//! the currently active conflicted file.
//!
//! ## What's genuinely shared with the File view, and what's structurally new
//!
//! The real editing engine ([`crate::code_surface::edit_buffer::EditBuffer`]), the single, generalized
//! `EntityInputHandler` impl, and the `Editor*` action handler *bodies* (`crate::code_surface::editing`)
//! are all reused as-is, routed here whenever [`crate::code_surface::editing::AdeApp::active_edit_target`]
//! resolves to the merge buffer rather than a File-view one - see that method's own docs for the
//! real "at most one editable surface is ever on screen" guarantee this relies on.
//!
//! What's deliberately *not* shared: this module's own row-painting function
//! ([`render_merge_edit_line`]) and its own dedicated layout-cache fields
//! ([`AdeApp::merge_edit_row_layout`]/[`AdeApp::merge_edit_last_layout`]/
//! [`AdeApp::merge_edit_last_bounds`]/[`AdeApp::merge_edit_last_layout_for`]/
//! [`AdeApp::merge_edit_scroll_handle`]) - kept structurally separate from the File view's own
//! equivalents (`crate::code_surface::editing::render_editable_file_view_line`,
//! `AdeApp::file_view_row_layout`, etc.) rather than reused verbatim, so the two independently
//! virtualized row lists' click/cursor hit-testing caches can never cross-contaminate - the exact
//! class of bug this project's own audits (BUILD-LOG's Revision R9a diff-highlight-cache finding)
//! keep finding when two structurally different surfaces share one cache.
//!
//! ## Deliberately no syntax highlighting, no diagnostics, no hover, no LSP, no completions
//!
//! [`AdeApp::start_merge_hand_edit`] seeds this view's [`crate::code_surface::edit_buffer::EditBuffer`] with
//! `extension: None`, so [`crate::code_surface::edit_buffer::EditBuffer::highlighter`] always resolves to
//! `None` and every line stays a single plain [`code_view::HighlightKind::Text`] run - real,
//! deliberate scope cuts: a mid-merge buffer full of `<<<<<<<`/`=======`/`>>>>>>>` markers has no
//! meaningful language-server semantics, and no language server relationship is ever established
//! for this flow, so there is no debounced re-highlight task class here at all (unlike
//! `crate::code_surface::editing::AdeApp::schedule_rehighlight`), no diagnostics gutter, no hover, and no
//! `"completions"` key-context tag is ever added to this surface's own `"merge-editor"` context
//! (`crate::default_key_bindings` never binds a `Completions*` action to it).
//!
//! ## No undo/redo
//!
//! Same real, documented scope cut as the File view's own `EditBuffer` (see that module's own
//! docs) - the only real "undo" this phase offers is a coarse, whole-buffer
//! [`AdeApp::discard_merge_hand_edit`].

use gpui::{canvas, fill, point, size, Bounds, ElementInputHandler, Entity, TextRun};

use super::*;
use crate::code_surface::editing::split_runs_for_marked_range;
use crate::code_surface::zoom::zoom_scoped;

impl AdeApp {
    /// Surface D's real whole-file hand-edit view, shown in place of the read-only two-column
    /// quick-pick view whenever [`AdeApp::merge_edit`] matches the currently active conflicted
    /// file (see `crate::merge::render`'s own docs for exactly where this is called
    /// from) - `&self`, matching `crate::merge::render::AdeApp::
    /// render_merge_flow_surface`'s own receiver (the caller already holds an immutable `session`
    /// borrow from `self.sessions`, so `&mut self` isn't available there; every real mutation
    /// here happens later, inside an event handler's own `&mut Self` lease, not during this
    /// render call).
    pub(in crate::merge) fn render_merge_edit_view(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(edit) = self.merge_edit.as_ref() else {
            return Empty.into_any_element();
        };
        let relative_path = edit.relative_path.clone();
        let line_count = edit.buffer.lines.len();
        let dirty = edit.buffer.is_dirty();
        let saving = self.merge_edit_save_running;
        let save_error = self.merge_edit_save_error.clone();
        let row_line_height = px(self.effective_code_rem_px() * 1.6);
        let focus_handle = self.merge_edit_focus_handle.clone();
        let entity = cx.entity();

        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .h(px(28.0))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::SECONDARY)
                    .child(relative_path.display().to_string()),
            )
            .when(dirty, |el| {
                el.child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(px(10.0))
                        .text_color(theme::text::FAINT)
                        .child("unsaved"),
                )
            })
            .child(div().flex_1())
            .when_some(save_error, |el, message| {
                el.child(
                    div()
                        .max_w(px(320.0))
                        .font(font(theme::font::SANS))
                        .text_size(px(10.5))
                        .text_color(theme::button::DANGER_FG)
                        .child(message),
                )
            })
            .child(
                div()
                    .id("merge-hand-edit-discard")
                    .cursor_pointer()
                    .h(px(22.0))
                    .px(px(10.0))
                    .rounded(theme::radius::BUTTON)
                    .border_1()
                    .border_color(theme::border::BUTTON)
                    .flex()
                    .items_center()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .text_color(theme::text::SECONDARY)
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child("Discard")
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.discard_merge_hand_edit(cx);
                    })),
            )
            .child({
                let save = div()
                    .id("merge-hand-edit-save")
                    .h(px(22.0))
                    .px(px(10.0))
                    .rounded(theme::radius::BUTTON)
                    .flex()
                    .items_center()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0));
                if dirty && !saving {
                    save.cursor_pointer()
                        .bg(theme::button::GREEN_BG)
                        .text_color(theme::button::GREEN_FG)
                        .hover(|el| el.bg(theme::button::GREEN_BG_HOVER))
                        .child("Save")
                        .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.save_merge_edit(cx);
                        }))
                } else {
                    save.cursor_default()
                        .bg(theme::border::BUTTON_DISABLED)
                        .text_color(theme::text::GHOSTER)
                        .child(if saving { "Saving\u{2026}" } else { "Save" })
                }
            });

        let row_path = relative_path.clone();
        let code = uniform_list(
            "merge-hand-edit-code",
            line_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                if this
                    .merge_edit
                    .as_ref()
                    .is_none_or(|edit| edit.relative_path != row_path)
                {
                    return Vec::new();
                }
                let total = this
                    .merge_edit
                    .as_ref()
                    .map(|edit| edit.buffer.lines.len())
                    .unwrap_or(0);
                let start = range.start.min(total);
                let end = range.end.min(total);
                // Pruned to this frame's own visible range, mirroring
                // `crate::code_surface::file_view::AdeApp::render_file_view`'s identical discipline
                // for `AdeApp::file_view_row_layout` - see that call site's own docs for the real,
                // measured unbounded-growth risk this avoids.
                let visible_line_numbers = (start + 1)..=end;
                this.merge_edit_row_layout
                    .retain(|line_number, _| visible_line_numbers.contains(line_number));
                let cursor_line_index = this.merge_edit.as_ref().map(|edit| {
                    edit.buffer
                        .line_col_for_offset(edit.buffer.cursor_offset())
                        .0
                });
                let mut rows = Vec::with_capacity(end.saturating_sub(start));
                for index in start..end {
                    let Some(edit) = this.merge_edit.as_ref() else {
                        break;
                    };
                    let Some(line) = edit.buffer.lines.get(index) else {
                        break;
                    };
                    let line_number = index + 1;
                    let selection_local = edit.buffer.selection_within_line(index);
                    let cursor_local = edit.buffer.cursor_within_line(index);
                    let marked_local = edit.buffer.marked_within_line(index);
                    let context = MergeEditLineContext {
                        entity: entity.clone(),
                        focus_handle: focus_handle.clone(),
                        relative_path: row_path.clone(),
                        line_index: index,
                        line_number,
                        line,
                        selection_local,
                        cursor_local,
                        marked_local,
                        is_cursor_line: cursor_line_index == Some(index),
                    };
                    rows.push(render_merge_edit_line(context, row_line_height, cx));
                }
                rows
            }),
        )
        .track_scroll(&self.merge_edit_scroll_handle)
        .flex_1()
        .min_h_0()
        .bg(theme::surface::PTY)
        .font(font(theme::font::MONO))
        .text_size(rems(1.0))
        .line_height(rems(1.6));

        div()
            .id("merge-hand-edit-surface")
            // Same "must be the exact node the focused-node-to-root context stack walks" real
            // discipline `crate::code_surface::render::AdeApp::render_code_surface` already
            // establishes for `"file-editor"` - see that method's own docs for the real,
            // live-verified bug getting this wrong once already caused.
            .track_focus(&self.merge_edit_focus_handle)
            // `"text-input"` rides alongside `"merge-editor"` here for the same real reason it
            // does on the code surface (GitHub issue #17): this is a real, editable text buffer,
            // so `secondary-z` must mean text undo over it, never `crate::worktree_history`'s
            // worktree-level `Undo`. See `crate::default_key_bindings`' own docs.
            .key_context("merge-editor text-input")
            .on_action(cx.listener(Self::handle_editor_backspace_action))
            .on_action(cx.listener(Self::handle_editor_delete_action))
            .on_action(cx.listener(Self::handle_editor_enter_action))
            .on_action(cx.listener(Self::handle_editor_left_action))
            .on_action(cx.listener(Self::handle_editor_right_action))
            .on_action(cx.listener(Self::handle_editor_up_action))
            .on_action(cx.listener(Self::handle_editor_down_action))
            .on_action(cx.listener(Self::handle_editor_select_left_action))
            .on_action(cx.listener(Self::handle_editor_select_right_action))
            .on_action(cx.listener(Self::handle_editor_select_up_action))
            .on_action(cx.listener(Self::handle_editor_select_down_action))
            .on_action(cx.listener(Self::handle_editor_home_action))
            .on_action(cx.listener(Self::handle_editor_end_action))
            .on_action(cx.listener(Self::handle_editor_select_all_action))
            .on_action(cx.listener(Self::handle_editor_copy_action))
            .on_action(cx.listener(Self::handle_editor_cut_action))
            .on_action(cx.listener(Self::handle_editor_paste_action))
            .on_action(cx.listener(Self::handle_editor_save_action))
            .on_action(cx.listener(Self::handle_text_undo_action))
            .on_action(cx.listener(Self::handle_text_redo_action))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(theme::surface::CENTER)
            .child(header)
            .child(zoom_scoped(self.effective_code_rem_px(), code))
            .into_any_element()
    }
}

/// One visible row's real per-row painting context - the merge hand-edit view's own, deliberately
/// smaller equivalent of `crate::code_surface::editing::EditableLineContext` (no diagnostics, no hover -
/// see this module's own top docs for why).
struct MergeEditLineContext<'a> {
    entity: Entity<AdeApp>,
    focus_handle: FocusHandle,
    relative_path: PathBuf,
    line_index: usize,
    line_number: usize,
    line: &'a code_view::RenderedLine,
    selection_local: Option<Range<usize>>,
    cursor_local: Option<usize>,
    marked_local: Option<Range<usize>>,
    is_cursor_line: bool,
}

/// The merge hand-edit view's own row painter - structurally mirrors
/// `crate::code_surface::editing::render_editable_file_view_line`'s real per-row div-based text plus
/// canvas-overlay cursor/selection/click-hit-testing/`EntityInputHandler`-registration approach
/// (see that function's own docs for why a bare `gpui::canvas` alone can't paint the visible
/// glyphs), but with no diagnostics gutter, no hover, and no changed-line git-gutter stripe - just
/// a line-number gutter and plain text, per this module's own top docs.
fn render_merge_edit_line(
    context: MergeEditLineContext<'_>,
    row_line_height: Pixels,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    let MergeEditLineContext {
        entity,
        focus_handle,
        relative_path,
        line_index,
        line_number,
        line,
        selection_local,
        cursor_local,
        marked_local,
        is_cursor_line,
    } = context;

    let runs = build_plain_text_runs(line, &marked_local);
    let line_text: gpui::SharedString = line.text.clone().into();
    let visible_runs = build_plain_text_run_divs(line, &marked_local);

    let row_path = relative_path.clone();
    let click_line_index = line_index;
    let click_line_number = line_number;

    let paint_entity = entity;
    let paint_path = relative_path;

    // Overlay-only - see `render_editable_file_view_line`'s own docs for why this never paints
    // the visible glyphs itself (only real cursor/selection quads plus the real
    // `EntityInputHandler` registration for the caret's own row).
    let cursor_overlay = canvas(
        move |bounds, window, _cx| {
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let shaped = window
                .text_system()
                .shape_line(line_text.clone(), font_size, &runs, None);

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
                    theme::syntax::CARET.resolve().opacity(0.28),
                )
            });
            let cursor_quad = cursor_local.map(|offset| {
                fill(
                    Bounds::new(
                        point(bounds.left() + shaped.x_for_index(offset), bounds.top()),
                        size(gpui::px(2.0), bounds.bottom() - bounds.top()),
                    ),
                    theme::syntax::CARET,
                )
            });
            (shaped, selection_quad, cursor_quad)
        },
        move |bounds, (shaped, selection_quad, cursor_quad), window, cx| {
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
            if focus_handle.is_focused(window) {
                if let Some(cursor_quad) = cursor_quad {
                    window.paint_quad(cursor_quad);
                }
            }
            paint_entity.update(cx, |this, _cx| {
                this.merge_edit_row_layout
                    .insert(line_number, (bounds, shaped.clone()));
                if is_cursor_line {
                    this.merge_edit_last_layout = Some(shaped);
                    this.merge_edit_last_bounds = Some(bounds);
                    this.merge_edit_last_layout_for = Some((paint_path.clone(), line_index));
                }
            });
        },
    )
    .absolute()
    .size_full();

    let text_row = div()
        .id(("merge-hand-edit-text", line_number))
        .relative()
        .flex_1()
        .min_w_0()
        .h(row_line_height)
        .flex()
        .children(visible_runs)
        .child(cursor_overlay)
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                let Some((bounds, shaped)) =
                    this.merge_edit_row_layout.get(&click_line_number).cloned()
                else {
                    return;
                };
                let Some(local_point) = bounds.localize(&event.position) else {
                    return;
                };
                let local_offset = shaped.closest_index_for_x(local_point.x);
                // `this`, not a second, independent `Entity<AdeApp>::read(cx)` - see
                // `render_editable_file_view_line`'s own docs for the real double-lease panic
                // that reasoning avoids.
                let Some(edit) = this.merge_edit.as_ref() else {
                    return;
                };
                if edit.relative_path != row_path {
                    return;
                }
                let Some(line_range) = edit.buffer.line_ranges.get(click_line_index).cloned()
                else {
                    return;
                };
                let absolute_offset = line_range.start + local_offset;

                window.focus(&this.merge_edit_focus_handle, cx);
                if let Some(edit) = this.merge_edit.as_mut() {
                    if event.modifiers.shift {
                        edit.buffer.select_to(absolute_offset);
                    } else {
                        edit.buffer.move_to(absolute_offset);
                    }
                }
                cx.stop_propagation();
                cx.notify();
            }),
        );

    let row = div()
        .id(("merge-hand-edit-line", line_number))
        .flex_none()
        .flex()
        .items_center()
        .child(
            div()
                .flex_none()
                .w(px(44.0))
                .pr(px(10.0))
                .text_right()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_color(theme::text::GUTTER)
                .text_size(px(10.0))
                .debug_selector(move || format!("merge-hand-edit-gutter-{line_number}"))
                .child(line_number.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl(px(10.0))
                .flex()
                .items_center()
                .debug_selector(move || format!("merge-hand-edit-row-{line_number}"))
                .child(text_row),
        );

    row.into_any_element()
}

/// Builds real (uncolored beyond the single [`code_view::HighlightKind::Text`] run every merge
/// hand-edit line has - see this module's own top docs) `TextRun`s for `shape_line`'s real pixel
/// math, splitting for a real IME-composition range via [`split_runs_for_marked_range`] - the
/// exact same real function the File view's own `crate::code_surface::editing::build_text_runs` uses for
/// this, reused rather than duplicated.
fn build_plain_text_runs(
    line: &code_view::RenderedLine,
    marked_local: &Option<Range<usize>>,
) -> Vec<TextRun> {
    let mut runs: Vec<TextRun> = line
        .runs
        .iter()
        .map(|(text, kind)| TextRun {
            len: text.len(),
            font: gpui::font(theme::font::MONO),
            color: code_view::color_for_kind(*kind).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        })
        .collect();
    if let Some(marked) = marked_local {
        runs = split_runs_for_marked_range(runs, marked);
    }
    runs
}

/// The real, visible per-run `div`s for one row's text - splits a run further at a real
/// IME-composition range's own byte boundaries, mirroring
/// `crate::code_surface::editing::build_text_run_divs`'s identical real splitting (minus the
/// diagnostic/hover cases that function also handles, which don't apply here).
fn build_plain_text_run_divs(
    line: &code_view::RenderedLine,
    marked_local: &Option<Range<usize>>,
) -> Vec<gpui::AnyElement> {
    let mut cursor = 0usize;
    let mut elements = Vec::new();
    for (text, kind) in &line.runs {
        let start = cursor;
        let end = start + text.len();
        cursor = end;

        let marked_overlap = marked_local.as_ref().and_then(|marked| {
            let overlap_start = marked.start.max(start);
            let overlap_end = marked.end.min(end);
            (overlap_start < overlap_end).then_some((overlap_start, overlap_end))
        });

        let Some((marked_start, marked_end)) = marked_overlap else {
            elements.push(plain_text_run_div(*kind, text.as_ref(), false));
            continue;
        };
        if marked_start > start {
            elements.push(plain_text_run_div(
                *kind,
                &text[0..marked_start - start],
                false,
            ));
        }
        elements.push(plain_text_run_div(
            *kind,
            &text[marked_start - start..marked_end - start],
            true,
        ));
        if end > marked_end {
            elements.push(plain_text_run_div(
                *kind,
                &text[marked_end - start..],
                false,
            ));
        }
    }
    elements
}

/// One real display `div` for a single (possibly marked-range-split) text piece.
fn plain_text_run_div(
    kind: code_view::HighlightKind,
    text: &str,
    is_marked: bool,
) -> gpui::AnyElement {
    let mut run = div()
        .text_color(code_view::color_for_kind(kind))
        .child(text.to_string());
    if is_marked {
        run = run
            .border_b_1()
            .border_color(code_view::color_for_kind(kind));
    }
    run.into_any_element()
}
